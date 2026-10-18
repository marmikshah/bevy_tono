//! The mixer the audio callback owns. Game threads never touch it: they send
//! [`Command`]s down a lock-free queue, and read state back from [`State`],
//! which the mixer publishes after every block. Nothing here renders a
//! `SoundDoc`: every render happens on the sending side, before the command.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sections::{Sections, SectionsLayer};
use crate::{Limiter, SectionEntry, TonoConfig};
use tono_core::adaptive::{AdaptiveMusic, LoopBuffer, Quantize, Stinger};
use tono_core::dsl::{Node, SoundDoc};
use tono_core::instrument::{Instrument, Note};
use tono_core::runtime::{
    AudioSource, Engine, InstanceHandle, PatchId, PreparedVoice, Priority, Spawn, Tween,
};

/// The most commands one block applies, so a flood of them cannot starve the
/// output; the rest wait for the next block.
const COMMANDS_PER_BLOCK: usize = 256;

/// No section sounding, in [`State::current_section`].
pub(crate) const NO_SECTION: usize = usize::MAX;

/// Scratch the mixer allocates up front, in frames: covers the callback's
/// chunk and the headless drain without allocating on the audio thread.
const SCRATCH_FRAMES: usize = 1024;

/// The engine's declick for a stolen voice; a resampled voice whose slot was
/// stolen fades over the same time.
const DECLICK_MS: f32 = 5.0;

/// How long a master gain change ramps.
const MASTER_RAMP_MS: f32 = 10.0;

/// Load the silent looping patch a resampled voice holds its engine slot
/// with. Both the game side's engine and the mixer's load it first, so it
/// has the same id in each.
pub(crate) fn load_slot(engine: &mut Engine) -> PatchId {
    let mut doc = SoundDoc::new("bevy_tono-slot", Node::Mix { inputs: Vec::new() });
    doc.duration = 0.01;
    doc.sample_rate = engine.sample_rate();
    engine.load(&doc)
}

/// A resampled or filtered voice as the game side builds it.
pub(crate) struct Sampled {
    /// The sound's render, interleaved stereo at the device rate.
    pub(crate) frames: Arc<[f32]>,
    pub(crate) gain: f32,
    pub(crate) pan: f32,
    pub(crate) rate: f32,
    pub(crate) lowpass_hz: Option<f32>,
    pub(crate) looping: bool,
}

/// One change for the mixer, built (and rendered) on the sending thread.
pub(crate) enum Command {
    Load(Box<SoundDoc>),
    Play {
        voice: u64,
        prepared: PreparedVoice,
        how: Spawn,
        gain: f32,
        pan: f32,
    },
    PlaySampled {
        voice: u64,
        sampled: Sampled,
        slot: PreparedVoice,
        priority: Priority,
    },
    Stop(u64),
    MaxVoices(usize),
    Gain(u64, f32),
    /// Left/right levels and a ramp length in frames.
    Levels(u64, [f32; 2], u32),
    Layer(LoopBuffer, f32),
    Intensity(f32),
    IntensityAt(f32, Quantize),
    Stinger(Stinger),
    StingerAt(Stinger, Quantize),
    /// Pause or resume the music, fading over this many frames first.
    Pause(bool, u32),
    /// Hold the music while the app is unfocused or suspended, with a fade.
    Suspend(bool, u32),
    Reset(u64),
    Tempo(f32, u32),
    Duck(f32, Duration),
    Section(LoopBuffer),
    Transition(usize, Quantize, SectionEntry),
    Instrument(Box<Instrument>),
    NoteOn(usize, Note, f32),
    NoteOff(usize, Note),
    Bend(usize, f32),
    MasterGain(f32),
    /// A target and a ramp length in frames.
    MusicGain(f32, u32),
}

/// What the game reads back without reaching the mixer. The mixer stores
/// after each block; the game-side fields are written by `TonoAudio` itself.
pub(crate) struct State {
    /// Music frames played since the last reset the mixer has applied.
    pub(crate) position: AtomicU64,
    /// The last reset the mixer applied (see `resets`).
    pub(crate) reset_seen: AtomicU64,
    /// Resets the game has asked for. While `reset_seen` lags it, the music
    /// clock reads 0: the reset is on its way.
    pub(crate) resets: AtomicU64,
    /// The sounding section, or [`NO_SECTION`].
    pub(crate) current_section: AtomicUsize,
    /// Whether the game has paused the music.
    pub(crate) paused: AtomicBool,
}

impl Default for State {
    fn default() -> Self {
        State {
            position: AtomicU64::new(0),
            reset_seen: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            current_section: AtomicUsize::new(NO_SECTION),
            paused: AtomicBool::new(false),
        }
    }
}

impl State {
    /// The music clock as of the newest block, or 0 while a reset is queued.
    pub(crate) fn position_frames(&self) -> u64 {
        if self.reset_seen.load(Ordering::Acquire) != self.resets.load(Ordering::Acquire) {
            return 0;
        }
        self.position.load(Ordering::Acquire)
    }
}

/// The SFX engine, the adaptive music bed and the live instruments, summed to
/// stereo. Owned by the audio callback (or the headless drain).
pub(crate) struct Mixer {
    commands: Receiver<Command>,
    state: Arc<State>,
    sample_rate: u32,
    pub(crate) engine: Engine,
    pub(crate) music: AdaptiveMusic,
    /// The bed's first layer; only this thread touches it.
    sections: Arc<Mutex<Sections>>,
    instruments: Vec<Instrument>,
    /// The engine instance behind each live `Voice` id.
    voices: Vec<(u64, InstanceHandle)>,
    /// Voices the sample player mixes (a rate or a lowpass).
    pub(crate) sampled: Vec<SampledVoice>,
    scratch: Vec<f32>,
    master_gain: Ramp,
    music_gain: Ramp,
    /// The fade a held bed goes silent through before it pauses.
    hold: Ramp,
    limiter: Option<LimiterState>,
    /// The newest reset applied, published with the position it produced.
    reset_seen: u64,
    paused: bool,
    suspended: bool,
    published_section: usize,
}

impl Mixer {
    pub(crate) fn new(
        sample_rate: u32,
        commands: Receiver<Command>,
        state: Arc<State>,
        config: &TonoConfig,
    ) -> Self {
        let mut engine = Engine::new(sample_rate);
        load_slot(&mut engine);
        // The sections play as the bed's first layer, always at full gain.
        let sections = Arc::new(Mutex::new(Sections::new(sample_rate)));
        let mut music = AdaptiveMusic::new(sample_rate);
        music.add_layer(SectionsLayer(sections.clone()), 0.0);
        Mixer {
            commands,
            state,
            sample_rate,
            engine,
            music,
            sections,
            instruments: Vec::with_capacity(8),
            voices: Vec::with_capacity(64),
            sampled: Vec::with_capacity(64),
            scratch: vec![0.0; SCRATCH_FRAMES * 2],
            master_gain: Ramp::new(1.0),
            music_gain: Ramp::new(1.0),
            hold: Ramp::new(1.0),
            limiter: config.limiter.map(|l| LimiterState::new(l, sample_rate)),
            reset_seen: 0,
            paused: false,
            suspended: false,
            published_section: NO_SECTION,
        }
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Apply queued commands, then mix one block of interleaved stereo.
    pub(crate) fn fill(&mut self, out: &mut [f32]) {
        self.apply_commands();
        let len = out.len();
        self.engine.fill(out); // SFX / one-shots (writes the whole buffer)
        self.mix_sampled(out);
        if self.scratch.len() < len {
            self.scratch.resize(len, 0.0);
        }
        // Add the music bed, then each live instrument, through the scratch.
        // A held bed pauses on the frame its fade lands, mid-block or not.
        let frames = len / 2;
        let fading_out = self.holding() && !self.music.is_paused();
        let until = if fading_out {
            (self.hold.remaining() as usize).min(frames)
        } else {
            frames
        };
        self.music.fill(&mut self.scratch[..until * 2]);
        if fading_out && until < frames {
            self.music.pause();
            self.music.fill(&mut self.scratch[until * 2..len]);
        }
        for (o, s) in out
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(self.scratch[..len].as_chunks::<2>().0)
        {
            let g = self.music_gain.tick() * self.hold.tick();
            o[0] += s[0] * g;
            o[1] += s[1] * g;
        }
        for inst in &mut self.instruments {
            inst.fill(&mut self.scratch[..len]);
            for (o, &s) in out.iter_mut().zip(&self.scratch[..len]) {
                *o += s;
            }
        }
        // Master gain, the optional limiter, then the clamp.
        for o in out.as_chunks_mut::<2>().0 {
            let g = self.master_gain.tick();
            let (mut l, mut r) = (o[0] * g, o[1] * g);
            if let Some(limiter) = &mut self.limiter {
                let g = limiter.gain(l, r);
                (l, r) = (l * g, r * g);
            }
            o[0] = l.clamp(-1.0, 1.0);
            o[1] = r.clamp(-1.0, 1.0);
        }
        let engine = &self.engine;
        self.voices.retain(|&(_, h)| engine.is_active(h));
        self.publish();
    }

    fn apply_commands(&mut self) {
        for _ in 0..COMMANDS_PER_BLOCK {
            let Ok(command) = self.commands.try_recv() else {
                return;
            };
            self.apply(command);
        }
    }

    fn apply(&mut self, command: Command) {
        let sr = self.sample_rate;
        match command {
            Command::Load(doc) => {
                self.engine.load(&doc);
            }
            Command::Play {
                voice,
                prepared,
                how,
                gain,
                pan,
            } => {
                let h = self.engine.spawn_prepared(prepared, how);
                if gain != 1.0 {
                    self.engine.set_gain(h, gain, Tween::IMMEDIATE);
                }
                if pan != 0.0 {
                    self.engine.set_pan(h, pan, Tween::IMMEDIATE);
                }
                if self.engine.is_active(h) {
                    self.voices.push((voice, h));
                }
            }
            Command::PlaySampled {
                voice,
                sampled,
                slot,
                priority,
            } => {
                // The engine decides whether the voice may sound, and what it
                // steals, exactly as for its own voices.
                let how = Spawn {
                    looping: true,
                    priority,
                };
                let slot = self.engine.spawn_prepared(slot, how);
                if self.engine.is_active(slot) {
                    self.sampled
                        .push(SampledVoice::new(voice, slot, sampled, sr));
                }
            }
            Command::MaxVoices(max) => self.engine.set_max_voices(max),
            Command::Stop(voice) => {
                if let Some(h) = self.handle(voice) {
                    self.engine.stop(h, Tween::ms(8.0, sr));
                } else if let Some(v) = self.sampled_mut(voice) {
                    v.stop(ms_frames(8.0, sr));
                }
            }
            Command::Gain(voice, gain) => {
                if let Some(h) = self.handle(voice) {
                    self.engine.set_gain(h, gain, Tween::ms(20.0, sr));
                } else if let Some(v) = self.sampled_mut(voice) {
                    v.gain.to(gain.max(0.0), ms_frames(20.0, sr));
                }
            }
            Command::Levels(voice, [l, r], frames) => {
                let (gain, pan) = gain_and_pan(l, r);
                if let Some(h) = self.handle(voice) {
                    self.engine.set_gain(h, gain, Tween::frames(frames));
                    self.engine.set_pan(h, pan, Tween::frames(frames));
                } else if let Some(v) = self.sampled_mut(voice) {
                    v.gain.to(gain, frames);
                    v.pan.to(pan, frames);
                }
            }
            Command::Layer(stem, fade_in_at) => {
                self.music.add_layer(stem, fade_in_at);
            }
            // NaN would switch every layer off, the sections with them.
            Command::Intensity(x) => self.music.set_intensity(finite_or_zero(x)),
            Command::IntensityAt(x, q) => self.music.set_intensity_at(finite_or_zero(x), q),
            Command::Stinger(stinger) => self.music.play_stinger(&stinger),
            Command::StingerAt(stinger, q) => self.music.play_stinger_at(&stinger, q),
            Command::Pause(paused, fade) => {
                self.paused = paused;
                self.hold_music(fade);
            }
            Command::Suspend(suspended, fade) => {
                self.suspended = suspended;
                self.hold_music(fade);
            }
            Command::Reset(epoch) => {
                self.music.reset();
                self.reset_seen = epoch;
            }
            Command::Tempo(bpm, beats_per_bar) => {
                self.music.set_tempo(bpm, beats_per_bar);
                crate::lock(&self.sections).set_tempo(bpm, beats_per_bar);
            }
            Command::Duck(depth, release) => self.music.duck(depth, release),
            Command::Section(buffer) => {
                crate::lock(&self.sections).add(buffer);
            }
            Command::Transition(section, q, entry) => {
                crate::lock(&self.sections).transition_to(section, q, entry);
            }
            Command::Instrument(inst) => self.instruments.push(*inst),
            Command::NoteOn(i, note, velocity) => {
                if let Some(inst) = self.instruments.get_mut(i) {
                    inst.note_on(note, velocity);
                }
            }
            Command::NoteOff(i, note) => {
                if let Some(inst) = self.instruments.get_mut(i) {
                    inst.note_off(note);
                }
            }
            Command::Bend(i, semitones) => {
                if let Some(inst) = self.instruments.get_mut(i) {
                    inst.set_bend(semitones);
                }
            }
            Command::MasterGain(gain) => self.master_gain.to(gain, ms_frames(MASTER_RAMP_MS, sr)),
            Command::MusicGain(gain, frames) => self.music_gain.to(gain, frames),
        }
    }

    fn holding(&self) -> bool {
        self.paused || self.suspended
    }

    /// Pause the bed while the game or the app's focus asks for it: fade it
    /// out over `fade` frames and pause it when the fade lands (`fill` does
    /// that), or resume it at once and fade it back in.
    fn hold_music(&mut self, fade: u32) {
        if self.holding() {
            self.hold.to(0.0, fade);
            if fade == 0 {
                self.music.pause();
            }
        } else {
            self.music.resume();
            self.hold.to(1.0, fade);
        }
    }

    fn sampled_mut(&mut self, voice: u64) -> Option<&mut SampledVoice> {
        self.sampled.iter_mut().find(|v| v.id == voice)
    }

    /// Add the sample player's voices to `out`. A voice whose engine slot was
    /// stolen or culled fades out; a finished voice gives its slot back.
    fn mix_sampled(&mut self, out: &mut [f32]) {
        let declick = ms_frames(DECLICK_MS, self.sample_rate);
        let engine = &mut self.engine;
        self.sampled.retain_mut(|v| {
            if !v.stopping && !engine.is_active(v.slot) {
                v.stop(declick);
            }
            let sounding = v.mix_into(out);
            if !sounding {
                engine.stop(v.slot, Tween::IMMEDIATE);
            }
            sounding
        });
    }

    pub(crate) fn handle(&self, voice: u64) -> Option<InstanceHandle> {
        self.voices
            .iter()
            .find(|(id, _)| *id == voice)
            .map(|&(_, h)| h)
    }

    /// Store what the game reads back. The position goes first: a reader that
    /// sees this block's `reset_seen` then sees a post-reset position.
    fn publish(&mut self) {
        let state = &self.state;
        state
            .position
            .store(self.music.position_frames(), Ordering::Release);
        state.reset_seen.store(self.reset_seen, Ordering::Release);
        let section = crate::lock(&self.sections).current().unwrap_or(NO_SECTION);
        if section != self.published_section {
            self.published_section = section;
            state.current_section.store(section, Ordering::Release);
        }
    }
}

/// A linear per-frame ramp, as the engine's faders move.
#[derive(Clone, Copy)]
pub(crate) struct Ramp {
    value: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl Ramp {
    pub(crate) fn new(value: f32) -> Self {
        Ramp {
            value,
            target: value,
            step: 0.0,
            remaining: 0,
        }
    }

    /// Head for `target` over `frames` (0 jumps there).
    pub(crate) fn to(&mut self, target: f32, frames: u32) {
        self.target = target;
        if frames == 0 {
            self.value = target;
            self.remaining = 0;
        } else {
            self.step = (target - self.value) / frames as f32;
            self.remaining = frames;
        }
    }

    /// Advance one frame and return the value.
    pub(crate) fn tick(&mut self) -> f32 {
        if self.remaining > 0 {
            self.value += self.step;
            self.remaining -= 1;
            if self.remaining == 0 {
                self.value = self.target;
            }
        }
        self.value
    }

    /// Frames left until the target.
    pub(crate) fn remaining(&self) -> u32 {
        self.remaining
    }

    pub(crate) fn at_target(&self) -> bool {
        self.remaining == 0
    }
}

fn finite_or_zero(x: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x }
}

/// `ms` milliseconds in frames, rounded as [`Tween::ms`] rounds.
pub(crate) fn ms_frames(ms: f32, sample_rate: u32) -> u32 {
    (ms / 1000.0 * sample_rate as f32).round().max(0.0) as u32
}

/// The engine's balance law: unity at centre, the far side attenuated.
pub(crate) fn balance(pan: f32) -> (f32, f32) {
    let l = if pan <= 0.0 { 1.0 } else { 1.0 - pan };
    let r = if pan >= 0.0 { 1.0 } else { 1.0 + pan };
    (l, r)
}

/// The gain and balance-law pan that put `l` and `r` on the two sides.
pub(crate) fn gain_and_pan(l: f32, r: f32) -> (f32, f32) {
    let gain = l.max(r);
    let pan = if gain <= 0.0 {
        0.0
    } else if l >= r {
        r / l - 1.0
    } else {
        1.0 - l / r
    };
    (gain, pan)
}

/// One voice of the sample player: a resampling read head over a sound's
/// render, an optional one-pole lowpass, and the engine-style faders.
pub(crate) struct SampledVoice {
    id: u64,
    /// The silent engine voice that holds this voice's place under the cap.
    slot: InstanceHandle,
    frames: Arc<[f32]>,
    /// Read head, in frames.
    pos: f64,
    step: f64,
    looping: bool,
    /// One-pole coefficient (1.0 = no filter) and its per-channel state.
    coeff: f32,
    tone: [f32; 2],
    gain: Ramp,
    pan: Ramp,
    stopping: bool,
}

impl SampledVoice {
    fn new(id: u64, slot: InstanceHandle, sampled: Sampled, sample_rate: u32) -> Self {
        let coeff = sampled.lowpass_hz.map_or(1.0, |hz| {
            1.0 - (-std::f32::consts::TAU * hz / sample_rate as f32).exp()
        });
        SampledVoice {
            id,
            slot,
            frames: sampled.frames,
            pos: 0.0,
            step: sampled.rate as f64,
            looping: sampled.looping,
            coeff,
            tone: [0.0; 2],
            gain: Ramp::new(sampled.gain),
            pan: Ramp::new(sampled.pan),
            stopping: false,
        }
    }

    fn stop(&mut self, frames: u32) {
        // Like the engine: a stop never cuts.
        self.gain.to(0.0, frames.max(1));
        self.stopping = true;
    }

    /// Add this voice to `out`; false once it has nothing more to play.
    fn mix_into(&mut self, out: &mut [f32]) -> bool {
        let n = self.frames.len() / 2;
        if n == 0 {
            return false;
        }
        for frame in out.as_chunks_mut::<2>().0 {
            if self.pos >= n as f64 {
                if !self.looping {
                    return false;
                }
                self.pos %= n as f64;
            }
            let i = self.pos as usize;
            let next = if i + 1 < n {
                i + 1
            } else if self.looping {
                0
            } else {
                i
            };
            let t = (self.pos - i as f64) as f32;
            let mut lr = [0.0f32; 2];
            for (c, v) in lr.iter_mut().enumerate() {
                let (a, b) = (self.frames[i * 2 + c], self.frames[next * 2 + c]);
                let x = a + (b - a) * t;
                *v = if self.coeff < 1.0 {
                    self.tone[c] += (x - self.tone[c]) * self.coeff;
                    self.tone[c]
                } else {
                    x
                };
            }
            let g = self.gain.tick();
            let (lg, rg) = balance(self.pan.tick());
            frame[0] += lr[0] * g * lg;
            frame[1] += lr[1] * g * rg;
            self.pos += self.step;
        }
        !(self.stopping && self.gain.at_target())
    }
}

/// The output limiter's running gain.
struct LimiterState {
    ceiling: f32,
    /// Per-frame one-pole release coefficient.
    release: f32,
    gain: f32,
}

impl LimiterState {
    fn new(limiter: Limiter, sample_rate: u32) -> Self {
        let frames = (limiter.release.as_secs_f32() * sample_rate as f32).max(1.0);
        LimiterState {
            ceiling: limiter.ceiling.clamp(0.0, 1.0),
            release: 1.0 - (-1.0 / frames).exp(),
            gain: 1.0,
        }
    }

    /// The gain for this frame: instant attack, smooth release.
    fn gain(&mut self, l: f32, r: f32) -> f32 {
        let peak = l.abs().max(r.abs());
        let wanted = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };
        self.gain = if wanted < self.gain {
            wanted
        } else {
            self.gain + (wanted - self.gain) * self.release
        };
        self.gain
    }
}
