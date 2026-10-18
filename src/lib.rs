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
//! The audio callback owns the mixer. [`TonoAudio`] sends it commands down a
//! lock-free queue and reads its state back from atomics, so a game-thread
//! call never blocks or drops an output buffer. Every render — one-shots,
//! stingers, stems, sections — runs on the calling thread before the command
//! is sent. If no audio device is present the mixer drains in real time into
//! silence, so the music clock still advances.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};

use bevy::prelude::*;

use tono_core::adaptive::LoopBuffer;
use tono_core::dsl::SoundDoc;
use tono_core::instrument::{Instrument, InstrumentDesign};
use tono_core::runtime::{Engine, PatchId, PreparedVoice, Spawn, Transport};

mod mixer;
mod output;
#[cfg(test)]
mod tests;

use mixer::{Command, Mixer, NO_SECTION, State};

/// The engine, re-exported so a downstream game needs only `bevy_tono` as a
/// dependency — author sounds with [`tono_core::dsl::SoundDoc`] without wiring
/// up a second crate.
pub use tono_core;
/// Beat/bar quantization for scheduled music changes (intensity, stingers,
/// section swaps), re-exported for convenience.
pub use tono_core::adaptive::Quantize;
/// A stinger rendered ahead of time, re-exported for convenience — see
/// [`TonoAudio::render_stinger`].
pub use tono_core::adaptive::Stinger;
/// A pitch for the live instrument layer, re-exported for convenience.
pub use tono_core::instrument::Note;

/// Offline, build-time audio audit — render each [`SoundDoc`] and grade it
/// against tono's review targets, with no audio device. A game's build target
/// can render every doc, grade it, and fail on a `FAIL`.
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
            d.sample_rate = super::FALLBACK_SAMPLE_RATE;
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
pub struct Voice(u64);

impl Voice {
    /// The voice a play that never sounded returns; every control ignores it.
    const INERT: Voice = Voice(0);
}

/// A live, polyphonic instrument you can play note-by-note (from a preset or an
/// [`InstrumentDesign`]). Register one, then `note_on`/`note_off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstrumentId(usize);

/// The Bevy resource: play SFX and drive adaptive music from your systems.
#[derive(Resource, Clone)]
pub struct TonoAudio {
    inner: Arc<Inner>,
}

struct Inner {
    /// The queue to the mixer the audio callback owns.
    commands: mpsc::Sender<Command>,
    /// State the mixer publishes, plus the flags this side sets.
    state: Arc<State>,
    /// Ids and names handed out here, in the order the mixer receives them.
    /// The audio callback never takes this lock.
    registry: Mutex<Registry>,
    /// One rendered voice per played [`Sound`], so a replay renders nothing.
    voices: Mutex<HashMap<PatchId, PreparedVoice>>,
    next_voice: AtomicU64,
    sample_rate: u32,
}

/// The game side's copy of what it has registered with the mixer.
struct Registry {
    /// Every loaded patch, in mixer order, so a voice can be prepared here
    /// and rendered off the audio thread. Never mixed.
    engine: Engine,
    sections: Vec<String>,
    instruments: usize,
    tempo: (f32, u32),
}

/// The rate used when there is none to go by: no audio device, or a doc whose
/// `sample_rate` is 0.
const FALLBACK_SAMPLE_RATE: u32 = 48_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl TonoAudio {
    /// A resource and the mixer it drives, joined by a fresh command queue.
    fn with_mixer(sample_rate: u32) -> (TonoAudio, Mixer) {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(State::default());
        let mixer = Mixer::new(sample_rate, rx, state.clone());
        let audio = TonoAudio {
            inner: Arc::new(Inner {
                commands: tx,
                state,
                registry: Mutex::new(Registry {
                    engine: Engine::new(sample_rate),
                    sections: Vec::new(),
                    instruments: 0,
                    tempo: (0.0, 4),
                }),
                voices: Mutex::default(),
                next_voice: AtomicU64::new(1),
                sample_rate,
            }),
        };
        (audio, mixer)
    }

    /// Queue a command for the mixer. Without a running mixer it goes nowhere.
    fn send(&self, command: Command) {
        self.inner.commands.send(command).ok();
    }

    /// Load a `SoundDoc` once (resampled to the device rate) so it can be played
    /// cheaply and repeatedly.
    pub fn register(&self, doc: &SoundDoc) -> Sound {
        let mut d = doc.clone();
        d.sample_rate = self.inner.sample_rate;
        // Load and send under one lock, so both engines number it the same.
        let mut registry = lock(&self.inner.registry);
        let patch = registry.engine.load(&d);
        self.send(Command::Load(Box::new(d)));
        Sound(patch)
    }

    /// Fire a one-shot — the classic SFX trigger. Returns the voice (for stop /
    /// gain); the engine reclaims it when it finishes. The first play of a
    /// sound renders it on the calling thread; later plays reuse that render.
    pub fn play(&self, sound: Sound) -> Voice {
        self.spawn(sound, Spawn::default(), 1.0)
    }

    /// Fire a one-shot that starts at `gain` (no 20 ms ramp from unity, so a
    /// short/transient SFX honours the level from its very first sample).
    pub fn play_with_gain(&self, sound: Sound, gain: f32) -> Voice {
        self.spawn(sound, Spawn::default(), gain)
    }

    /// Start a looping voice (e.g. an ambience bed).
    pub fn play_looping(&self, sound: Sound) -> Voice {
        let looping = Spawn {
            looping: true,
            ..Spawn::default()
        };
        self.spawn(sound, looping, 1.0)
    }

    /// Render the sound here (once), then hand the mixer a voice to start.
    fn spawn(&self, sound: Sound, how: Spawn, gain: f32) -> Voice {
        // Not a patch of this engine: nothing to play.
        let Some(prepared) = self.rendered_voice(sound) else {
            return Voice::INERT;
        };
        let voice = self.inner.next_voice.fetch_add(1, Ordering::Relaxed);
        self.send(Command::Play {
            voice,
            prepared,
            how,
            gain,
        });
        Voice(voice)
    }

    /// The sound's rendered voice, rendering it on this thread the first time.
    fn rendered_voice(&self, sound: Sound) -> Option<PreparedVoice> {
        if let Some(voice) = lock(&self.inner.voices).get(&sound.0) {
            return Some(voice.clone());
        }
        let mut voice = lock(&self.inner.registry).engine.prepare(sound.0)?;
        voice.render();
        lock(&self.inner.voices).insert(sound.0, voice.clone());
        Some(voice)
    }

    /// Stop a voice (a short fade avoids a click).
    pub fn stop(&self, voice: Voice) {
        self.send(Command::Stop(voice.0));
    }

    /// Set a voice's gain (0..).
    pub fn set_gain(&self, voice: Voice, gain: f32) {
        self.send(Command::Gain(voice.0, gain));
    }

    /// Add a looping music stem that fades to full volume once
    /// [`set_intensity`](Self::set_intensity) reaches `fade_in_at` (0.0 = always
    /// playing). Layer several for reactive game music.
    pub fn music_layer(&self, doc: &SoundDoc, fade_in_at: f32) {
        let mut d = doc.clone();
        d.sample_rate = self.inner.sample_rate;
        self.send(Command::Layer(LoopBuffer::from_doc(&d), fade_in_at));
    }

    /// Set the music intensity, 0..1 — stems cross-fade toward their levels.
    pub fn set_intensity(&self, intensity: f32) {
        self.send(Command::Intensity(intensity));
    }

    /// Fire a one-shot musical stinger over the bed. Renders on the calling
    /// thread; for a long cue use [`render_stinger`](Self::render_stinger) on
    /// a worker and [`play_stinger`](Self::play_stinger).
    pub fn stinger(&self, doc: &SoundDoc) {
        self.play_stinger(&self.render_stinger(doc));
    }

    /// Render a stinger at the device rate without touching the bus. Call it
    /// from any thread (the resource is `Clone`), keep the result, and play it
    /// as often as you like — a replay neither renders nor copies.
    pub fn render_stinger(&self, doc: &SoundDoc) -> Stinger {
        Stinger::render(doc, self.inner.sample_rate)
    }

    /// Fire a pre-rendered stinger over the bed now.
    pub fn play_stinger(&self, stinger: &Stinger) {
        self.send(Command::Stinger(stinger.clone()));
    }

    /// Fire a pre-rendered stinger on a beat/bar boundary. Pair it with
    /// [`Quantize::AtBeat`] to land on a beat computed before the render, so
    /// the render time cannot push the cue a beat late.
    pub fn play_stinger_at(&self, stinger: &Stinger, quantize: Quantize) {
        self.send(Command::StingerAt(stinger.clone(), quantize));
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
        self.inner.state.paused.store(true, Ordering::Release);
        self.send(Command::Pause(true));
    }

    /// Resume the music bed from a [`music_pause`](Self::music_pause).
    pub fn music_resume(&self) {
        self.inner.state.paused.store(false, Ordering::Release);
        self.send(Command::Pause(false));
    }

    /// Whether the music bed is currently paused.
    pub fn music_is_paused(&self) -> bool {
        self.inner.state.paused.load(Ordering::Acquire)
    }

    /// Restart the bed from sample 0 — the position clock and every layer rewind
    /// to their loop head. Call this to line the music up with a beat clock.
    pub fn music_reset(&self) {
        let epoch = self.inner.state.resets.fetch_add(1, Ordering::AcqRel) + 1;
        self.send(Command::Reset(epoch));
    }

    /// Frames of music rendered while playing since start or the last
    /// [`music_reset`](Self::music_reset) — the sample-exact musical clock a
    /// beat-locked game derives its beat position from. Holds while paused.
    /// Read from the newest mixed block.
    pub fn music_position_frames(&self) -> u64 {
        self.inner.state.position_frames()
    }

    /// Duck the whole bed to `1.0 - depth`, recovering over `release` — a fast
    /// sidechain for stingers or SFX, independent of the (slower) intensity
    /// cross-fade.
    pub fn music_duck(&self, depth: f32, release: std::time::Duration) {
        self.send(Command::Duck(depth, release));
    }

    // ---- Musical time & quantized scheduling ----

    /// Set the tempo so intensity changes, stingers and section swaps can align
    /// to beats/bars. Required for any [`Quantize`] other than
    /// [`Immediate`](Quantize::Immediate).
    pub fn set_music_tempo(&self, bpm: f32, beats_per_bar: u32) {
        let mut registry = lock(&self.inner.registry);
        registry.tempo = (bpm.max(0.0), beats_per_bar);
        self.send(Command::Tempo(bpm, beats_per_bar));
    }

    /// The musical clock on the tempo's grid (0 until a tempo is set).
    fn music_clock(&self) -> Option<Transport> {
        let (bpm, beats_per_bar) = lock(&self.inner.registry).tempo;
        (bpm > 0.0).then(|| {
            let mut clock = Transport::constant(self.inner.sample_rate, bpm, beats_per_bar);
            clock.seek_frame(self.music_position_frames());
            clock
        })
    }

    /// The musical position in beats since the last [`music_reset`](Self::music_reset).
    pub fn music_beats(&self) -> f64 {
        self.music_clock().map_or(0.0, |c| c.position_beats())
    }

    /// The musical position in bars since the last [`music_reset`](Self::music_reset).
    pub fn music_bars(&self) -> f64 {
        self.music_clock().map_or(0.0, |c| c.position_bars())
    }

    /// Set the intensity on a beat/bar boundary — the quantized counterpart of
    /// [`set_intensity`](Self::set_intensity).
    pub fn set_intensity_at(&self, intensity: f32, quantize: Quantize) {
        self.send(Command::IntensityAt(intensity, quantize));
    }

    /// Fire a stinger on a beat/bar boundary. The stinger is rendered now, on
    /// the calling thread; only its playback is deferred to the boundary. A
    /// relative boundary such as [`Quantize::Beat`] is resolved after that
    /// render — see [`play_stinger_at`](Self::play_stinger_at) to avoid it.
    pub fn stinger_at(&self, doc: &SoundDoc, quantize: Quantize) {
        self.play_stinger_at(&self.render_stinger(doc), quantize);
    }

    // ---- Horizontal sections ----

    /// Add a horizontal section (a looping bed) and return its index. The first
    /// section added starts playing; switch between them with
    /// [`transition_to_section`](Self::transition_to_section).
    pub fn add_section(&self, name: impl Into<String>, doc: &SoundDoc) -> usize {
        let mut d = doc.clone();
        d.sample_rate = self.inner.sample_rate;
        let buffer = LoopBuffer::from_doc(&d);
        let name = name.into();
        let mut registry = lock(&self.inner.registry);
        let index = registry.sections.len();
        registry.sections.push(name.clone());
        self.send(Command::Section(name, buffer));
        // The first section plays at once; say so before the mixer does.
        if index == 0 {
            let current = &self.inner.state.current_section;
            current
                .compare_exchange(NO_SECTION, 0, Ordering::AcqRel, Ordering::Acquire)
                .ok();
        }
        index
    }

    /// Cross-fade to another section on a beat/bar boundary — horizontal
    /// re-sequencing (swap "explore" for "battle" on the next bar, no mid-phrase
    /// cut).
    pub fn transition_to_section(&self, section: usize, quantize: Quantize) {
        self.send(Command::Transition(section, quantize));
    }

    /// The section currently sounding, if any.
    pub fn current_section(&self) -> Option<usize> {
        let current = self.inner.state.current_section.load(Ordering::Acquire);
        (current != NO_SECTION).then_some(current)
    }

    /// Look up a section index by name.
    pub fn section_named(&self, name: &str) -> Option<usize> {
        lock(&self.inner.registry)
            .sections
            .iter()
            .position(|s| s == name)
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
        let inst = Instrument::new(design, self.inner.sample_rate).map_err(|e| format!("{e:?}"))?;
        let mut registry = lock(&self.inner.registry);
        let id = registry.instruments;
        registry.instruments += 1;
        self.send(Command::Instrument(Box::new(inst)));
        Ok(InstrumentId(id))
    }

    /// Start a note on a registered instrument (velocity 0..1). Polyphonic —
    /// hold several at once.
    pub fn note_on(&self, instrument: InstrumentId, note: Note, velocity: f32) {
        if self.known_instrument(instrument, "note_on") {
            self.send(Command::NoteOn(instrument.0, note, velocity));
        }
    }

    /// Release a note on a registered instrument.
    pub fn note_off(&self, instrument: InstrumentId, note: Note) {
        if self.known_instrument(instrument, "note_off") {
            self.send(Command::NoteOff(instrument.0, note));
        }
    }

    /// Set the pitch bend, in semitones, on a registered instrument (a whammy /
    /// bend-wheel knob; applies to its sounding voices).
    pub fn set_bend(&self, instrument: InstrumentId, semitones: f32) {
        if self.known_instrument(instrument, "set_bend") {
            self.send(Command::Bend(instrument.0, semitones));
        }
    }

    /// Whether `instrument` was registered here. An unknown id is a caller bug.
    fn known_instrument(&self, instrument: InstrumentId, op: &str) -> bool {
        let known = instrument.0 < lock(&self.inner.registry).instruments;
        debug_assert!(known, "{op}: unknown instrument {}", instrument.0);
        known
    }

    /// Set the master output gain applied to the whole bus (SFX + music),
    /// `0.0..=1.0` — a global volume knob. Clamped; applied per sample, so a
    /// change is click-free at the buffer scale.
    pub fn set_master_gain(&self, gain: f32) {
        self.send(Command::MasterGain(gain.clamp(0.0, 1.0)));
    }

    /// The device sample rate the engine renders at.
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
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
        audio.play_with_gain(e.sound, e.gain);
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
        install(app, output::spawn_audio());
    }
}

/// Insert the resource and wire the messages that feed it.
fn install(app: &mut App, audio: TonoAudio) {
    app.insert_resource(audio)
        .add_message::<PlaySfx>()
        .add_message::<PlayNote>()
        .add_message::<StopNote>()
        .add_systems(Update, (play_queued_sfx, drain_notes));
}
