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
//! use tono_core::dsl::SoundDoc;
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
use tono_core::runtime::{AudioSource, Engine, InstanceHandle, PatchId, Tween};

/// A registered sound — a `SoundDoc` loaded into the engine, ready to play.
#[derive(Clone, Copy, Debug)]
pub struct Sound(PatchId);

/// A sounding voice — one playing instance of a [`Sound`].
#[derive(Clone, Copy, Debug)]
pub struct Voice(InstanceHandle);

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

    /// The device sample rate the engine renders at.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// The plugin: sets up real-time audio and inserts [`TonoAudio`].
pub struct TonoPlugin;

impl Plugin for TonoPlugin {
    fn build(&self, app: &mut App) {
        let (bus, sample_rate) = spawn_audio();
        app.insert_resource(TonoAudio { bus, sample_rate });
    }
}

/// The mixed audio bus: the SFX/one-shot [`Engine`] plus an [`AdaptiveMusic`]
/// bed, summed to stereo.
struct GameBus {
    engine: Engine,
    music: AdaptiveMusic,
    scratch: Vec<f32>,
}

impl GameBus {
    fn new(sample_rate: u32) -> Self {
        GameBus {
            engine: Engine::new(sample_rate),
            music: AdaptiveMusic::new(sample_rate),
            scratch: Vec::new(),
        }
    }
}

impl AudioSource for GameBus {
    fn fill(&mut self, out: &mut [f32]) -> usize {
        let n = self.engine.fill(out); // SFX / one-shots (writes the whole buffer)
        if self.scratch.len() < out.len() {
            self.scratch.resize(out.len(), 0.0);
        }
        let music = &mut self.scratch[..out.len()];
        self.music.fill(music);
        for (o, &m) in out.iter_mut().zip(music.iter()) {
            *o = (*o + m).clamp(-1.0, 1.0);
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
}
