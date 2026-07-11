//! bevy_tono — play the [tono](https://github.com/marmikshah/tono) sound engine
//! from a Bevy game.
//!
//! Add [`TonoPlugin`], then reach for the [`TonoAudio`] resource in your systems:
//! register a `SoundDoc` once, fire it as a one-shot on gameplay events, and
//! drive an adaptive music bed with a single intensity knob. All deterministic —
//! the same doc sounds identical every run.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_tono::{TonoAudio, TonoPlugin, Sound};
//! use bevy_tono::tono_core::dsl::SoundDoc; // re-exported — one dependency
//!
//! # fn setup(mut commands: Commands, audio: Res<TonoAudio>) {
//! let doc: SoundDoc = serde_json::from_str(r#"{ "name":"blip", "duration":0.2,
//!     "root": { "type":"sine", "freq":880 } }"#).unwrap();
//! let blip: Sound = audio.register(&doc);
//! commands.insert_resource(Sfx(blip));
//! # }
//! # #[derive(Resource)] struct Sfx(Sound);
//! # fn on_jump(audio: Res<TonoAudio>, sfx: Res<Sfx>) { audio.play(sfx.0); }
//! ```
//!
//! Audio runs on a dedicated thread that owns the `cpal` stream; the callback
//! only ever `try_lock`s, so a control-thread poke never blocks (or clicks) the
//! output. If no audio device is present the plugin degrades to silence rather
//! than failing to start.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use bevy::prelude::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use tono_core::adaptive::{AdaptiveMusic, LoopBuffer};
use tono_core::dsl::SoundDoc;
use tono_core::instrument::{Instrument, InstrumentDesign};
use tono_core::runtime::{AudioSource, Engine, InstanceHandle, PatchId, Tween};

/// The engine, re-exported so a downstream game needs only `bevy_tono` as a
/// dependency — author sounds with [`tono_core::dsl::SoundDoc`] without wiring
/// up a second crate.
pub use tono_core;
/// Beat/bar quantization for scheduled music changes (intensity, stingers,
/// section swaps), re-exported for convenience.
pub use tono_core::adaptive::Quantize;
/// A pitch for the live instrument layer, re-exported for convenience.
pub use tono_core::instrument::Note;

/// Offline, build-time audio audit — render each [`SoundDoc`] and grade it
/// against tono's review targets, with no audio device.
///
/// tono-core has a full review layer (archetype targets, PASS/WARN/FAIL
/// findings, LUFS / true-peak analysis) but it is only reachable through the CLI
/// or `tono_core::review` directly, so a game wiring an `audio` build target had
/// to hand-roll the render → analyse → review glue. This surfaces it: a Makefile
/// `audio` target renders every registered doc, grades it, and fails on a
/// `FAIL`.
///
/// ```no_run
/// use bevy_tono::review::{self, AuditItem, Archetype};
/// use bevy_tono::tono_core::dsl::SoundDoc;
///
/// # fn docs() -> Vec<(String, SoundDoc)> { Vec::new() }
/// let owned = docs();
/// let items: Vec<AuditItem> = owned
///     .iter()
///     .map(|(name, doc)| AuditItem { name, doc, archetype: Some(Archetype::Ui) })
///     .collect();
/// let reviews = review::audit(&items);
/// if review::any_failed(&reviews) {
///     std::process::exit(1); // fail the build target
/// }
/// ```
pub mod review {
    use tono_core::dsl::{Playback, SoundDoc};
    use tono_core::{analysis, render};

    /// The review types, re-exported so the audit reads with one dependency.
    pub use tono_core::review::{Archetype, Finding, Review, Status};

    /// Render `doc` offline and grade it. `archetype` selects the target table
    /// (`None` runs only the universal ship checklist — clipping, silence, loop
    /// seam, onset count). Deterministic and device-free: safe in a build script
    /// or a test. Renders at the doc's own `sample_rate`.
    pub fn grade(doc: &SoundDoc, archetype: Option<Archetype>) -> Review {
        // A zero/unset sample rate would make `analysis::stats` divide by the
        // rate → NaN/garbage. Grade at a sane rate rather than trust the field.
        let doc = if doc.sample_rate == 0 {
            let mut d = doc.clone();
            d.sample_rate = 48_000;
            std::borrow::Cow::Owned(d)
        } else {
            std::borrow::Cow::Borrowed(doc)
        };
        let doc = doc.as_ref();
        let samples = render::render(doc);
        let a = analysis::stats(&samples, doc.sample_rate);
        // The seam check only applies to looping docs; pass it only then.
        let seam =
            matches!(doc.playback, Playback::Loop { .. }).then(|| render::loop_seam_db(&samples));
        tono_core::review::review(doc, &a, archetype, seam)
    }

    /// A doc to audit: a name for reporting plus the archetype to grade against.
    pub struct AuditItem<'a> {
        /// Reporting label (e.g. the doc's file stem).
        pub name: &'a str,
        /// The document to render and grade.
        pub doc: &'a SoundDoc,
        /// The archetype target, or `None` for the universal checklist only.
        pub archetype: Option<Archetype>,
    }

    /// Grade a whole set of docs, returning each name with its [`Review`].
    pub fn audit(items: &[AuditItem<'_>]) -> Vec<(String, Review)> {
        items
            .iter()
            .map(|it| (it.name.to_string(), grade(it.doc, it.archetype)))
            .collect()
    }

    /// Did any doc grade `FAIL`? The predicate a build target gates on.
    pub fn any_failed(reviews: &[(String, Review)]) -> bool {
        reviews.iter().any(|(_, r)| r.grade == Status::Fail)
    }
}

/// A registered sound — a `SoundDoc` loaded into the engine, ready to play.
#[derive(Clone, Copy, Debug)]
pub struct Sound(PatchId);

/// A sounding voice — one playing instance of a [`Sound`].
#[derive(Clone, Copy, Debug)]
pub struct Voice(InstanceHandle);

/// A live, polyphonic instrument you can play note-by-note (from a preset or an
/// [`InstrumentDesign`]). Register one, then `note_on`/`note_off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstrumentId(usize);

/// The Bevy resource: play SFX and drive adaptive music from your systems.
#[derive(Resource, Clone)]
pub struct TonoAudio {
    bus: Arc<Mutex<GameBus>>,
    sample_rate: u32,
}

fn lock(bus: &Arc<Mutex<GameBus>>) -> MutexGuard<'_, GameBus> {
    bus.lock().unwrap_or_else(|e| e.into_inner())
}

impl TonoAudio {
    /// Load a `SoundDoc` once (resampled to the device rate) so it can be played
    /// cheaply and repeatedly.
    pub fn register(&self, doc: &SoundDoc) -> Sound {
        let mut d = doc.clone();
        d.sample_rate = self.sample_rate;
        Sound(lock(&self.bus).engine.load(&d))
    }

    /// Fire a one-shot — the classic SFX trigger. Returns the voice (for stop /
    /// gain); the engine reclaims it when it finishes.
    pub fn play(&self, sound: Sound) -> Voice {
        Voice(lock(&self.bus).engine.play(sound.0))
    }

    /// Start a looping voice (e.g. an ambience bed).
    pub fn play_looping(&self, sound: Sound) -> Voice {
        Voice(lock(&self.bus).engine.play_looping(sound.0))
    }

    /// Stop a voice (a short fade avoids a click).
    pub fn stop(&self, voice: Voice) {
        lock(&self.bus)
            .engine
            .stop(voice.0, Tween::ms(8.0, self.sample_rate));
    }

    /// Set a voice's gain (0..).
    pub fn set_gain(&self, voice: Voice, gain: f32) {
        lock(&self.bus)
            .engine
            .set_gain(voice.0, gain, Tween::ms(20.0, self.sample_rate));
    }

    /// Add a looping music stem that fades to full volume once
    /// [`set_intensity`](Self::set_intensity) reaches `fade_in_at` (0.0 = always
    /// playing). Layer several for reactive game music.
    pub fn music_layer(&self, doc: &SoundDoc, fade_in_at: f32) {
        let mut d = doc.clone();
        d.sample_rate = self.sample_rate;
        let stem = LoopBuffer::from_doc(&d);
        lock(&self.bus).music.add_layer(stem, fade_in_at);
    }

    /// Set the music intensity, 0..1 — stems cross-fade toward their levels.
    pub fn set_intensity(&self, intensity: f32) {
        lock(&self.bus).music.set_intensity(intensity);
    }

    /// Fire a one-shot musical stinger over the bed.
    pub fn stinger(&self, doc: &SoundDoc) {
        let mut d = doc.clone();
        d.sample_rate = self.sample_rate;
        lock(&self.bus).music.stinger(&d);
    }

    /// Compile a [`Song`](tono_core::song::Song) and play it as an always-on
    /// looping music bed. Returns the compile error if the song is invalid.
    pub fn play_song(&self, song: &tono_core::song::Song) -> Result<(), String> {
        let doc = song.to_doc()?;
        self.music_layer(&doc, 0.0);
        Ok(())
    }

    // ---- Transport (beat-locked playback) ----

    /// Pause the music bed — output goes silent while the position clock and
    /// every layer hold, so [`music_resume`](Self::music_resume) continues
    /// seamlessly.
    pub fn music_pause(&self) {
        lock(&self.bus).music.pause();
    }

    /// Resume the music bed from a [`music_pause`](Self::music_pause).
    pub fn music_resume(&self) {
        lock(&self.bus).music.resume();
    }

    /// Whether the music bed is currently paused.
    pub fn music_is_paused(&self) -> bool {
        lock(&self.bus).music.is_paused()
    }

    /// Restart the bed from sample 0 — the position clock and every layer rewind
    /// to their loop head. Call this to line the music up with a beat clock.
    pub fn music_reset(&self) {
        lock(&self.bus).music.reset();
    }

    /// Frames of music rendered while playing since start or the last
    /// [`music_reset`](Self::music_reset) — the sample-exact musical clock a
    /// beat-locked game derives its beat position from. Holds while paused.
    pub fn music_position_frames(&self) -> u64 {
        lock(&self.bus).music.position_frames()
    }

    /// Duck the whole bed to `1.0 - depth`, recovering over `release` — a fast
    /// sidechain for stingers or SFX, independent of the (slower) intensity
    /// cross-fade.
    pub fn music_duck(&self, depth: f32, release: std::time::Duration) {
        lock(&self.bus).music.duck(depth, release);
    }

    // ---- Musical time & quantized scheduling ----

    /// Set the tempo so intensity changes, stingers and section swaps can align
    /// to beats/bars. Required for any [`Quantize`] other than
    /// [`Immediate`](Quantize::Immediate).
    pub fn set_music_tempo(&self, bpm: f32, beats_per_bar: u32) {
        lock(&self.bus).music.set_tempo(bpm, beats_per_bar);
    }

    /// The musical position in beats since the last [`music_reset`](Self::music_reset).
    pub fn music_beats(&self) -> f64 {
        lock(&self.bus).music.beats()
    }

    /// The musical position in bars since the last [`music_reset`](Self::music_reset).
    pub fn music_bars(&self) -> f64 {
        lock(&self.bus).music.bars()
    }

    /// Set the intensity on a beat/bar boundary — the quantized counterpart of
    /// [`set_intensity`](Self::set_intensity).
    pub fn set_intensity_at(&self, intensity: f32, quantize: Quantize) {
        lock(&self.bus).music.set_intensity_at(intensity, quantize);
    }

    /// Fire a stinger on a beat/bar boundary. The stinger is rendered now; only
    /// its playback is deferred to the boundary.
    pub fn stinger_at(&self, doc: &SoundDoc, quantize: Quantize) {
        let mut d = doc.clone();
        d.sample_rate = self.sample_rate;
        lock(&self.bus).music.stinger_at(&d, quantize);
    }

    // ---- Horizontal sections ----

    /// Add a horizontal section (a looping bed) and return its index. The first
    /// section added starts playing; switch between them with
    /// [`transition_to_section`](Self::transition_to_section).
    pub fn add_section(&self, name: impl Into<String>, doc: &SoundDoc) -> usize {
        let mut d = doc.clone();
        d.sample_rate = self.sample_rate;
        lock(&self.bus).music.add_section(name, &d)
    }

    /// Cross-fade to another section on a beat/bar boundary — horizontal
    /// re-sequencing (swap "explore" for "battle" on the next bar, no mid-phrase
    /// cut).
    pub fn transition_to_section(&self, section: usize, quantize: Quantize) {
        lock(&self.bus).music.transition_to(section, quantize);
    }

    /// The section currently sounding, if any.
    pub fn current_section(&self) -> Option<usize> {
        lock(&self.bus).music.current_section()
    }

    /// Look up a section index by name.
    pub fn section_named(&self, name: &str) -> Option<usize> {
        lock(&self.bus).music.section_named(name)
    }

    /// Register a live, playable [`Instrument`] from a factory preset (see
    /// `tono_core::presets` for names, e.g. `"vibrato_lead"`, `"sub_bass"`,
    /// `"fm_tine"`). Errs on an unknown name.
    pub fn preset(&self, name: &str) -> Result<InstrumentId, String> {
        let design =
            tono_core::presets::preset(name).ok_or_else(|| format!("unknown preset '{name}'"))?;
        self.add_instrument(design)
    }

    /// Register a live, playable instrument from an [`InstrumentDesign`].
    pub fn add_instrument(&self, design: InstrumentDesign) -> Result<InstrumentId, String> {
        let inst = Instrument::new(design, self.sample_rate).map_err(|e| format!("{e:?}"))?;
        let mut bus = lock(&self.bus);
        bus.instruments.push(inst);
        Ok(InstrumentId(bus.instruments.len() - 1))
    }

    /// Start a note on a registered instrument (velocity 0..1). Polyphonic —
    /// hold several at once.
    pub fn note_on(&self, instrument: InstrumentId, note: Note, velocity: f32) {
        let mut bus = lock(&self.bus);
        if let Some(inst) = bus.instruments.get_mut(instrument.0) {
            inst.note_on(note, velocity);
        } else {
            debug_assert!(false, "note_on: unknown instrument {}", instrument.0);
        }
    }

    /// Release a note on a registered instrument.
    pub fn note_off(&self, instrument: InstrumentId, note: Note) {
        let mut bus = lock(&self.bus);
        if let Some(inst) = bus.instruments.get_mut(instrument.0) {
            inst.note_off(note);
        } else {
            debug_assert!(false, "note_off: unknown instrument {}", instrument.0);
        }
    }

    /// Set the pitch bend, in semitones, on a registered instrument (a whammy /
    /// bend-wheel knob; applies to its sounding voices).
    pub fn set_bend(&self, instrument: InstrumentId, semitones: f32) {
        let mut bus = lock(&self.bus);
        if let Some(inst) = bus.instruments.get_mut(instrument.0) {
            inst.set_bend(semitones);
        } else {
            debug_assert!(false, "set_bend: unknown instrument {}", instrument.0);
        }
    }

    /// Set the master output gain applied to the whole bus (SFX + music),
    /// `0.0..=1.0` — a global volume knob. Clamped; applied per sample, so a
    /// change is click-free at the buffer scale.
    pub fn set_master_gain(&self, gain: f32) {
        lock(&self.bus).master_gain = gain.clamp(0.0, 1.0);
    }

    /// The device sample rate the engine renders at.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Fire a one-shot [`Sound`] by writing this message — the ECS-friendly
/// alternative to reaching for [`TonoAudio`] in a system. A game system takes a
/// `MessageWriter<PlaySfx>` and writes `PlaySfx::new(sound)`; [`TonoPlugin`]
/// drains the queue each frame and plays them.
#[derive(Message, Clone, Copy, Debug)]
pub struct PlaySfx {
    /// The registered sound to play.
    pub sound: Sound,
    /// Playback gain (1.0 = unity).
    pub gain: f32,
}

impl PlaySfx {
    /// Play `sound` at unity gain.
    pub fn new(sound: Sound) -> Self {
        PlaySfx { sound, gain: 1.0 }
    }

    /// Set the gain (0..).
    pub fn with_gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }
}

/// Start a note on a registered live instrument — the ECS-friendly way to play
/// an instrument note-by-note. Write it with a `MessageWriter<PlayNote>`.
#[derive(Message, Clone, Copy, Debug)]
pub struct PlayNote {
    /// The registered instrument to play.
    pub instrument: InstrumentId,
    /// The pitch.
    pub note: Note,
    /// Velocity, 0..1.
    pub velocity: f32,
}

impl PlayNote {
    /// Play `note` on `instrument` at a default velocity.
    pub fn new(instrument: InstrumentId, note: Note) -> Self {
        PlayNote {
            instrument,
            note,
            velocity: 0.9,
        }
    }

    /// Set the velocity (0..1).
    pub fn with_velocity(mut self, velocity: f32) -> Self {
        self.velocity = velocity;
        self
    }
}

/// Release a note on a registered live instrument (pair with [`PlayNote`]).
#[derive(Message, Clone, Copy, Debug)]
pub struct StopNote {
    /// The registered instrument.
    pub instrument: InstrumentId,
    /// The pitch to release.
    pub note: Note,
}

/// Drain queued [`PlaySfx`] messages and fire them on the audio bus.
fn play_queued_sfx(mut sfx: MessageReader<PlaySfx>, audio: Res<TonoAudio>) {
    for e in sfx.read() {
        let voice = audio.play(e.sound);
        if e.gain != 1.0 {
            audio.set_gain(voice, e.gain);
        }
    }
}

/// Drain queued [`PlayNote`]/[`StopNote`] messages onto the live instruments.
fn drain_notes(
    mut on: MessageReader<PlayNote>,
    mut off: MessageReader<StopNote>,
    audio: Res<TonoAudio>,
) {
    for e in on.read() {
        audio.note_on(e.instrument, e.note, e.velocity);
    }
    for e in off.read() {
        audio.note_off(e.instrument, e.note);
    }
}

/// The plugin: sets up real-time audio, inserts [`TonoAudio`], and wires the
/// [`PlaySfx`] / [`PlayNote`] / [`StopNote`] messages so systems can make sound
/// without touching the resource.
pub struct TonoPlugin;

impl Plugin for TonoPlugin {
    fn build(&self, app: &mut App) {
        let (bus, sample_rate) = spawn_audio();
        app.insert_resource(TonoAudio { bus, sample_rate })
            .add_message::<PlaySfx>()
            .add_message::<PlayNote>()
            .add_message::<StopNote>()
            .add_systems(Update, (play_queued_sfx, drain_notes));
    }
}

/// The mixed audio bus: the SFX/one-shot [`Engine`], an [`AdaptiveMusic`] bed,
/// and the live [`Instrument`]s, summed to stereo.
struct GameBus {
    engine: Engine,
    music: AdaptiveMusic,
    instruments: Vec<Instrument>,
    scratch: Vec<f32>,
    master_gain: f32,
}

impl GameBus {
    fn new(sample_rate: u32) -> Self {
        GameBus {
            engine: Engine::new(sample_rate),
            music: AdaptiveMusic::new(sample_rate),
            instruments: Vec::new(),
            scratch: Vec::new(),
            master_gain: 1.0,
        }
    }
}

impl AudioSource for GameBus {
    fn fill(&mut self, out: &mut [f32]) -> usize {
        let n = self.engine.fill(out); // SFX / one-shots (writes the whole buffer)
        let len = out.len();
        if self.scratch.len() < len {
            self.scratch.resize(len, 0.0);
        }
        // Add the music bed, then each live instrument, through the scratch.
        self.music.fill(&mut self.scratch[..len]);
        for (o, &s) in out.iter_mut().zip(self.scratch[..len].iter()) {
            *o += s;
        }
        for inst in &mut self.instruments {
            inst.fill(&mut self.scratch[..len]);
            for (o, &s) in out.iter_mut().zip(self.scratch[..len].iter()) {
                *o += s;
            }
        }
        // Master gain + clamp.
        let g = self.master_gain;
        for o in out.iter_mut() {
            *o = (*o * g).clamp(-1.0, 1.0);
        }
        n
    }
}

/// Spawn the audio thread (it owns the `cpal` stream for the process's life) and
/// return a shared handle to the bus. Never fails — a missing device degrades to
/// a silent (undrained) bus.
fn spawn_audio() -> (Arc<Mutex<GameBus>>, u32) {
    let fallback = || (Arc::new(Mutex::new(GameBus::new(48_000))), 48_000u32);
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("bevy_tono-audio".into())
        .spawn(move || match build_stream() {
            Ok((stream, bus, sr)) => {
                tx.send((bus, sr)).ok();
                let _stream = stream; // hold the stream open for the thread's life
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                eprintln!("bevy_tono: audio disabled ({e})");
                tx.send((Arc::new(Mutex::new(GameBus::new(48_000))), 48_000))
                    .ok();
            }
        });
    if spawned.is_err() {
        return fallback();
    }
    rx.recv().unwrap_or_else(|_| fallback())
}

/// Build the output stream + bus on the audio thread.
fn build_stream() -> anyhow::Result<(cpal::Stream, Arc<Mutex<GameBus>>, u32)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let supported = device.default_output_config()?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let bus = Arc::new(Mutex::new(GameBus::new(sample_rate)));
    let cb_bus = bus.clone();
    let mut scratch = Vec::<f32>::new();
    let err_fn = |e| eprintln!("bevy_tono: stream error: {e}");

    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config,
            move |data: &mut [f32], _| fill_output(&cb_bus, data, channels, &mut scratch),
            err_fn,
            None,
        )?,
        other => anyhow::bail!("unsupported output sample format {other:?} (needs f32)"),
    };
    stream.play()?;
    Ok((stream, bus, sample_rate))
}

/// The `cpal` callback body: render the bus (never blocking) and lay it into the
/// device's channel layout.
fn fill_output(
    bus: &Arc<Mutex<GameBus>>,
    data: &mut [f32],
    channels: usize,
    scratch: &mut Vec<f32>,
) {
    let frames = data.len() / channels.max(1);
    if scratch.len() < frames * 2 {
        scratch.resize(frames * 2, 0.0);
    }
    let st = &mut scratch[..frames * 2];
    match bus.try_lock() {
        Ok(mut b) => {
            b.fill(st);
        }
        Err(_) => st.fill(0.0),
    }
    for f in 0..frames {
        let (l, r) = (st[f * 2], st[f * 2 + 1]);
        let base = f * channels;
        if channels == 1 {
            data[base] = 0.5 * (l + r);
        } else {
            data[base] = l;
            data[base + 1] = r;
            for c in 2..channels {
                data[base + c] = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blip() -> SoundDoc {
        serde_json::from_str(
            r#"{ "name":"blip", "duration":0.1, "engine":2, "root": { "type":"mul", "inputs": [
                { "type":"sine", "freq":880 },
                { "type":"env", "a":0.002, "d":0.05, "s":0.0, "r":0.02 } ] } }"#,
        )
        .unwrap()
    }

    #[test]
    fn bus_plays_a_registered_sfx() {
        let mut bus = GameBus::new(48_000);
        let patch = bus.engine.load(&blip());
        bus.engine.play(patch);
        let mut out = vec![0.0f32; 512 * 2];
        bus.fill(&mut out);
        let peak = out.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(peak > 0.0, "the SFX sounds through the bus");
    }

    #[test]
    fn bus_mixes_music_under_sfx() {
        let mut bus = GameBus::new(48_000);
        bus.music.add_layer(LoopBuffer::from_doc(&blip()), 0.0); // always-on stem
        bus.music.set_intensity(1.0);
        let mut out = vec![0.0f32; 512 * 2];
        bus.fill(&mut out);
        assert!(out.iter().any(|&x| x != 0.0), "music bed sounds");
    }

    #[test]
    fn master_gain_scales_and_silences() {
        let mut bus = GameBus::new(48_000);
        let patch = bus.engine.load(&blip());
        bus.engine.play(patch);
        bus.master_gain = 0.0;
        let mut out = vec![0.0f32; 512 * 2];
        bus.fill(&mut out);
        let peak = out.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert_eq!(peak, 0.0, "master gain 0 mutes the bus");
    }

    #[test]
    fn ecs_layer_plays_sfx_and_beds_a_song() {
        use bevy::MinimalPlugins;
        use bevy::app::App;
        use tono_core::dsl::{Adsr, SeqWave};
        use tono_core::song::{Song, note};

        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(TonoPlugin);

        // PlaySfx: a registered sound fired via the message queue is drained and
        // played by the plugin's system on update — without panicking.
        let sound = app.world().resource::<TonoAudio>().register(&blip());
        app.world_mut()
            .write_message(PlaySfx::new(sound).with_gain(0.7));
        app.update();

        // play_song: a Song compiles and beds as a looping music layer.
        let mut song = Song::new("bg", 100.0);
        song.add_track(
            "bass",
            SeqWave::Bass,
            Adsr {
                a: 0.005,
                d: 0.1,
                s: 0.9,
                r: 0.1,
                punch: 0.0,
            },
        );
        song.add_pattern("r", 1, vec![note(0, 4, "C2"), note(8, 4, "G2")]);
        song.arrange("bass", "r", 0);
        assert!(
            app.world().resource::<TonoAudio>().play_song(&song).is_ok(),
            "the song compiles and beds"
        );
    }

    #[test]
    fn live_instrument_plays_a_held_note() {
        use bevy::MinimalPlugins;
        use bevy::app::App;

        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(TonoPlugin);

        // Register a preset instrument and play a note by message.
        let inst = app
            .world()
            .resource::<TonoAudio>()
            .preset("vibrato_lead")
            .expect("the preset exists");
        app.world_mut()
            .write_message(PlayNote::new(inst, Note::C4).with_velocity(0.9));
        app.update(); // drain_notes fires note_on

        // A held note sustains — pull a few buffers and confirm it sounds.
        let bus = app.world().resource::<TonoAudio>().bus.clone();
        let mut peak = 0.0f32;
        for _ in 0..8 {
            let mut out = vec![0.0f32; 512 * 2];
            lock(&bus).fill(&mut out);
            peak = peak.max(out.iter().fold(0.0f32, |m, &x| m.max(x.abs())));
        }
        assert!(peak > 0.0, "the held instrument note sounds");
    }

    #[test]
    fn review_grades_a_rendered_doc_offline() {
        use crate::review::{self, Archetype, AuditItem, Status};

        let mut doc = blip();
        doc.sample_rate = 48_000;

        // A single doc renders + grades (no device) to a Review with findings
        // and one of the three overall grades.
        let r = review::grade(&doc, Some(Archetype::Ui));
        assert!(!r.findings.is_empty(), "the review produced findings");
        assert!(matches!(
            r.grade,
            Status::Pass | Status::Warn | Status::Fail
        ));

        // The batch API pairs each name with its review; the gate predicate
        // reflects the worst grade in the set.
        let items = [AuditItem {
            name: "blip",
            doc: &doc,
            archetype: Some(Archetype::Ui),
        }];
        let reviews = review::audit(&items);
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].0, "blip");
        assert_eq!(
            review::any_failed(&reviews),
            reviews[0].1.grade == Status::Fail
        );
    }

    fn audio_app() -> (App, TonoAudio) {
        use bevy::MinimalPlugins;
        use bevy::app::App;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(TonoPlugin);
        let audio = app.world().resource::<TonoAudio>().clone();
        (app, audio)
    }

    #[test]
    fn transport_position_pause_and_reset() {
        let (app, audio) = audio_app();
        audio.music_layer(&blip(), 0.0); // an always-on stem so the clock advances

        assert_eq!(audio.music_position_frames(), 0);
        assert!(!audio.music_is_paused());

        // No audio device in tests — advance the bed by filling the bus directly.
        let bus = app.world().resource::<TonoAudio>().bus.clone();
        lock(&bus).fill(&mut vec![0.0f32; 256 * 2]);
        assert_eq!(audio.music_position_frames(), 256, "the clock advances");

        audio.music_pause();
        assert!(audio.music_is_paused());
        lock(&bus).fill(&mut vec![0.0f32; 256 * 2]);
        assert_eq!(
            audio.music_position_frames(),
            256,
            "the clock holds while paused"
        );

        audio.music_resume();
        audio.music_reset();
        assert_eq!(audio.music_position_frames(), 0, "reset zeroes the clock");
        // Transport-only calls stay callable without a device.
        audio.music_duck(0.5, std::time::Duration::from_millis(180));
    }

    #[test]
    fn tempo_beats_and_sections() {
        let (_app, audio) = audio_app();

        audio.set_music_tempo(120.0, 4);
        assert_eq!(audio.music_beats(), 0.0);
        assert_eq!(audio.music_bars(), 0.0);

        // Sections: first added plays; lookups resolve by name and index.
        let explore = audio.add_section("explore", &blip());
        let battle = audio.add_section("battle", &blip());
        assert_eq!((explore, battle), (0, 1));
        assert_eq!(audio.current_section(), Some(0), "the first section plays");
        assert_eq!(audio.section_named("battle"), Some(1));
        assert_eq!(audio.section_named("nope"), None);

        // Quantized scheduling is callable and doesn't panic (the scheduling
        // logic itself is covered in tono-core).
        audio.transition_to_section(battle, Quantize::Bar);
        audio.set_intensity_at(1.0, Quantize::Beat);
        audio.stinger_at(&blip(), Quantize::Beat);
    }
}
