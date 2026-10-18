//! The mixer the audio callback owns. Game threads never touch it: they send
//! [`Command`]s down a lock-free queue, and read state back from [`State`],
//! which the mixer publishes after every block. Nothing here renders a
//! `SoundDoc`: every render happens on the sending side, before the command.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use tono_core::adaptive::{AdaptiveMusic, LoopBuffer, Quantize, Stinger};
use tono_core::dsl::SoundDoc;
use tono_core::instrument::{Instrument, Note};
use tono_core::runtime::{AudioSource, Engine, InstanceHandle, PreparedVoice, Spawn, Tween};

/// The most commands one block applies, so a flood of them cannot starve the
/// output; the rest wait for the next block.
const COMMANDS_PER_BLOCK: usize = 256;

/// No section sounding, in [`State::current_section`].
pub(crate) const NO_SECTION: usize = usize::MAX;

/// Scratch the mixer allocates up front, in frames: covers the callback's
/// chunk and the headless drain without allocating on the audio thread.
const SCRATCH_FRAMES: usize = 1024;

/// One change for the mixer, built (and rendered) on the sending thread.
pub(crate) enum Command {
    Load(Box<SoundDoc>),
    Play {
        voice: u64,
        prepared: PreparedVoice,
        how: Spawn,
        gain: f32,
    },
    Stop(u64),
    Gain(u64, f32),
    Layer(LoopBuffer, f32),
    Intensity(f32),
    IntensityAt(f32, Quantize),
    Stinger(Stinger),
    StingerAt(Stinger, Quantize),
    Pause(bool),
    Reset(u64),
    Tempo(f32, u32),
    Duck(f32, Duration),
    Section(String, LoopBuffer),
    Transition(usize, Quantize),
    Instrument(Box<Instrument>),
    NoteOn(usize, Note, f32),
    NoteOff(usize, Note),
    Bend(usize, f32),
    MasterGain(f32),
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
    instruments: Vec<Instrument>,
    /// The engine instance behind each live `Voice` id.
    voices: Vec<(u64, InstanceHandle)>,
    scratch: Vec<f32>,
    master_gain: f32,
    /// The newest reset applied, published with the position it produced.
    reset_seen: u64,
    published_section: usize,
}

impl Mixer {
    pub(crate) fn new(sample_rate: u32, commands: Receiver<Command>, state: Arc<State>) -> Self {
        Mixer {
            commands,
            state,
            sample_rate,
            engine: Engine::new(sample_rate),
            music: AdaptiveMusic::new(sample_rate),
            instruments: Vec::with_capacity(8),
            voices: Vec::with_capacity(64),
            scratch: vec![0.0; SCRATCH_FRAMES * 2],
            master_gain: 1.0,
            reset_seen: 0,
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
        if self.scratch.len() < len {
            self.scratch.resize(len, 0.0);
        }
        // Add the music bed, then each live instrument, through the scratch.
        self.music.fill(&mut self.scratch[..len]);
        for (o, &s) in out.iter_mut().zip(&self.scratch[..len]) {
            *o += s;
        }
        for inst in &mut self.instruments {
            inst.fill(&mut self.scratch[..len]);
            for (o, &s) in out.iter_mut().zip(&self.scratch[..len]) {
                *o += s;
            }
        }
        // Master gain + clamp.
        let g = self.master_gain;
        for o in out.iter_mut() {
            *o = (*o * g).clamp(-1.0, 1.0);
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
            } => {
                let h = self.engine.spawn_prepared(prepared, how);
                if gain != 1.0 {
                    self.engine.set_gain(h, gain, Tween::IMMEDIATE);
                }
                if self.engine.is_active(h) {
                    self.voices.push((voice, h));
                }
            }
            Command::Stop(voice) => {
                if let Some(h) = self.handle(voice) {
                    self.engine.stop(h, Tween::ms(8.0, sr));
                }
            }
            Command::Gain(voice, gain) => {
                if let Some(h) = self.handle(voice) {
                    self.engine.set_gain(h, gain, Tween::ms(20.0, sr));
                }
            }
            Command::Layer(stem, fade_in_at) => {
                self.music.add_layer(stem, fade_in_at);
            }
            Command::Intensity(x) => self.music.set_intensity(x),
            Command::IntensityAt(x, q) => self.music.set_intensity_at(x, q),
            Command::Stinger(stinger) => self.music.play_stinger(&stinger),
            Command::StingerAt(stinger, q) => self.music.play_stinger_at(&stinger, q),
            Command::Pause(true) => self.music.pause(),
            Command::Pause(false) => self.music.resume(),
            Command::Reset(epoch) => {
                self.music.reset();
                self.reset_seen = epoch;
            }
            Command::Tempo(bpm, beats_per_bar) => self.music.set_tempo(bpm, beats_per_bar),
            Command::Duck(depth, release) => self.music.duck(depth, release),
            Command::Section(name, buffer) => {
                self.music.add_section_buffer(name, buffer);
            }
            Command::Transition(section, q) => self.music.transition_to(section, q),
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
            Command::MasterGain(gain) => self.master_gain = gain,
        }
    }

    fn handle(&self, voice: u64) -> Option<InstanceHandle> {
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
        let section = self.music.current_section().unwrap_or(NO_SECTION);
        if section != self.published_section {
            self.published_section = section;
            state.current_section.store(section, Ordering::Release);
        }
    }
}
