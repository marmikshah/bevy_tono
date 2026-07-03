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
/// A pitch for the live instrument layer, re-exported for convenience.
pub use tono_core::instrument::Note;

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
        if let Some(inst) = lock(&self.bus).instruments.get_mut(instrument.0) {
            inst.note_on(note, velocity);
        }
    }

    /// Release a note on a registered instrument.
    pub fn note_off(&self, instrument: InstrumentId, note: Note) {
        if let Some(inst) = lock(&self.bus).instruments.get_mut(instrument.0) {
            inst.note_off(note);
        }
    }

    /// Set the pitch bend, in semitones, on a registered instrument (a whammy /
    /// bend-wheel knob; applies to its sounding voices).
    pub fn set_bend(&self, instrument: InstrumentId, semitones: f32) {
        if let Some(inst) = lock(&self.bus).instruments.get_mut(instrument.0) {
            inst.set_bend(semitones);
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
}
