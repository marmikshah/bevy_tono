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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};

use bevy::prelude::*;

use tono_core::adaptive::LoopBuffer;
use tono_core::dsl::SoundDoc;
use tono_core::instrument::{Instrument, InstrumentDesign};
use tono_core::runtime::{Engine, PatchId, PreparedVoice, Spawn, Transport};

/// A voice's importance under [`TonoAudio::set_max_voices`], re-exported
/// from the engine.
pub use tono_core::runtime::Priority;

mod mixer;
mod output;
mod sections;
#[cfg(test)]
mod tests;

use mixer::{Command, Mixer, NO_SECTION, Sampled, State};

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

/// How one play of a [`Sound`] sounds — the argument to
/// [`TonoAudio::play_with`]. `Play::default()` plays exactly as
/// [`TonoAudio::play`] does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Play {
    /// Starting gain (1.0 = unity), applied from the first sample.
    pub gain: f32,
    /// Stereo balance, −1 (left) to +1 (right). The engine's balance law:
    /// the far side is attenuated and the near side stays at unity, so a
    /// panned cue never gains level and still reads in mono.
    pub pan: f32,
    /// Playback rate (1.0 = as rendered). A resample of the rendered sound,
    /// so pitch and length change together: 2.0 is an octave up at half the
    /// length. Non-positive or non-finite plays at 1.0.
    pub rate: f32,
    /// A one-pole lowpass on this voice, at this cutoff in Hz. `None` = off.
    pub lowpass_hz: Option<f32>,
    /// Importance under [`TonoAudio::set_max_voices`].
    pub priority: Priority,
    /// Loop until stopped instead of playing once.
    pub looping: bool,
}

impl Default for Play {
    fn default() -> Self {
        Play {
            gain: 1.0,
            pan: 0.0,
            rate: 1.0,
            lowpass_hz: None,
            priority: Priority::NORMAL,
            looping: false,
        }
    }
}

/// Per-play variation a [`Sound`] carries (see
/// [`TonoAudio::register_with`]), so a cue repeated often does not sound
/// stamped out. Each field is the width of a uniform random offset drawn on
/// every play; the default is no variation at all.
///
/// The offsets come from a seeded generator, not the clock: the same plays
/// in the same order vary the same way every run.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Variation {
    /// Rate spread: each play's rate is scaled by `1 ± pitch` (0.03 ≈ half a
    /// semitone either way).
    pub pitch: f32,
    /// Pan spread: each play's pan moves by up to `± pan`.
    pub pan: f32,
    /// Level spread: each play's gain drops by up to this fraction.
    pub level: f32,
}

/// How a section enters on [`TonoAudio::transition_to_section_with`]. The
/// default is what [`TonoAudio::transition_to_section`] does: from the
/// section's head, on the grid counted from the last
/// [`music_reset`](TonoAudio::music_reset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionEntry {
    /// Pick the section up where it was when it was last left, instead of
    /// rewinding it to its head — returning to a place resumes its music.
    /// A section that has not played yet (or since a reset) starts at its
    /// head either way.
    pub resume: bool,
    /// Count [`Quantize::Beat`], [`Bar`](Quantize::Bar) and
    /// [`Bars(n)`](Quantize::Bars) from the frame the sounding section
    /// entered (its phrase origin) rather than from the last reset, so a
    /// `Bars(4)` change lands on that section's own phrase boundary.
    /// [`Quantize::AtBeat`] stays absolute.
    pub from_entry: bool,
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
    /// One rendered buffer per [`Sound`] played with a rate or a lowpass.
    samples: Mutex<HashMap<PatchId, Arc<[f32]>>>,
    /// The silent engine voice a resampled voice holds its place with.
    slot: PreparedVoice,
    next_voice: AtomicU64,
    /// The variation generator's state (xorshift32).
    random: AtomicU32,
    sample_rate: u32,
}

/// The game side's copy of what it has registered with the mixer.
struct Registry {
    /// Every loaded patch, in mixer order, so a voice can be prepared here
    /// and rendered off the audio thread. Never mixed.
    engine: Engine,
    /// Each sound's doc at the device rate, to render its sample buffer.
    docs: HashMap<PatchId, SoundDoc>,
    variations: HashMap<PatchId, Variation>,
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
    fn with_mixer(sample_rate: u32, config: &TonoConfig) -> (TonoAudio, Mixer) {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(State::default());
        let mixer = Mixer::new(sample_rate, rx, state.clone(), config);
        let mut engine = Engine::new(sample_rate);
        let slot_patch = mixer::load_slot(&mut engine);
        let mut slot = engine.prepare(slot_patch).expect("just loaded");
        slot.render();
        let audio = TonoAudio {
            inner: Arc::new(Inner {
                commands: tx,
                state,
                registry: Mutex::new(Registry {
                    engine,
                    docs: HashMap::new(),
                    variations: HashMap::new(),
                    sections: Vec::new(),
                    instruments: 0,
                    tempo: (0.0, 4),
                }),
                voices: Mutex::default(),
                samples: Mutex::default(),
                slot,
                next_voice: AtomicU64::new(1),
                random: AtomicU32::new(0x7f4a_7c15),
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
        registry.docs.insert(patch, d.clone());
        self.send(Command::Load(Box::new(d)));
        Sound(patch)
    }

    /// [`register`](Self::register) a sound that varies a little on every
    /// play — pitch, pan and level, within `variation`'s ranges.
    pub fn register_with(&self, doc: &SoundDoc, variation: Variation) -> Sound {
        let sound = self.register(doc);
        if variation != Variation::default() {
            let mut registry = lock(&self.inner.registry);
            registry.variations.insert(sound.0, variation);
        }
        sound
    }

    /// Render `sound` on a background thread, so its first play costs the
    /// game thread nothing. Renders what a plain play needs, plus the sample
    /// buffer a resampled play needs when the sound carries a
    /// [`Variation`]. A play that arrives before the render finishes renders
    /// it itself, as without a prewarm.
    pub fn prewarm(&self, sound: Sound) {
        let audio = self.clone();
        // With no thread to spare, the first play renders, as before.
        std::thread::Builder::new()
            .name("bevy_tono-prewarm".into())
            .spawn(move || {
                audio.rendered_voice(sound);
                let varied = lock(&audio.inner.registry)
                    .variations
                    .contains_key(&sound.0);
                if varied {
                    audio.samples(sound);
                }
            })
            .ok();
    }

    /// The next variation offset, uniform in −1..1.
    fn draw(&self) -> f32 {
        let step = |mut x: u32| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let random = &self.inner.random;
        let old = random
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| Some(step(x)))
            .unwrap_or_else(|x| x);
        (step(old) >> 8) as f32 / 8_388_607.5 - 1.0
    }

    /// `play` with the sound's variation, if it has one, drawn into it.
    fn varied(&self, sound: Sound, mut play: Play) -> Play {
        let variation = lock(&self.inner.registry).variations.get(&sound.0).copied();
        if let Some(v) = variation {
            play.rate *= 1.0 + self.draw() * v.pitch;
            play.pan += self.draw() * v.pan;
            play.gain *= 1.0 - self.draw().abs() * v.level.clamp(0.0, 1.0);
        }
        play
    }

    /// Fire a one-shot — the classic SFX trigger. Returns the voice (for stop /
    /// gain); the engine reclaims it when it finishes. The first play of a
    /// sound renders it on the calling thread; later plays reuse that render.
    pub fn play(&self, sound: Sound) -> Voice {
        self.play_with(sound, Play::default())
    }

    /// Fire a one-shot that starts at `gain` (no 20 ms ramp from unity, so a
    /// short/transient SFX honours the level from its very first sample).
    pub fn play_with_gain(&self, sound: Sound, gain: f32) -> Voice {
        self.play_with(
            sound,
            Play {
                gain,
                ..Play::default()
            },
        )
    }

    /// Start a looping voice (e.g. an ambience bed).
    pub fn play_looping(&self, sound: Sound) -> Voice {
        self.play_with(
            sound,
            Play {
                looping: true,
                ..Play::default()
            },
        )
    }

    /// Play a sound with per-play controls: gain, pan, rate, lowpass,
    /// priority and looping (see [`Play`]).
    ///
    /// At rate 1.0 with no lowpass the engine plays the voice, exactly as
    /// [`play`](Self::play) does. Otherwise a sample player resamples the
    /// sound's render (linear interpolation) and filters it; that voice still
    /// holds a silent engine voice, so [`set_max_voices`](Self::set_max_voices)
    /// and [`Priority`] govern it like any other. The first such play of a
    /// sound renders its buffer on the calling thread.
    ///
    /// A sound registered with [`register_with`](Self::register_with) varies
    /// on top of these controls.
    pub fn play_with(&self, sound: Sound, play: Play) -> Voice {
        let play = self.varied(sound, play);
        let how = Spawn {
            looping: play.looping,
            priority: play.priority,
        };
        let rate = if play.rate.is_finite() && play.rate > 0.0 {
            play.rate
        } else {
            1.0
        };
        let pan = if play.pan.is_nan() {
            0.0
        } else {
            play.pan.clamp(-1.0, 1.0)
        };
        let gain = play.gain.max(0.0);
        let lowpass_hz = play.lowpass_hz.filter(|hz| hz.is_finite() && *hz > 0.0);
        if rate == 1.0 && lowpass_hz.is_none() {
            return self.spawn(sound, how, gain, pan);
        }
        // Not a patch of this engine: nothing to play.
        let Some(frames) = self.samples(sound) else {
            return Voice::INERT;
        };
        let voice = self.inner.next_voice.fetch_add(1, Ordering::Relaxed);
        let sampled = Sampled {
            frames,
            gain,
            pan,
            rate,
            lowpass_hz,
            looping: play.looping,
        };
        self.send(Command::PlaySampled {
            voice,
            sampled,
            slot: self.inner.slot.clone(),
            priority: play.priority,
        });
        Voice(voice)
    }

    /// Render the sound here (once), then hand the mixer a voice to start.
    fn spawn(&self, sound: Sound, how: Spawn, gain: f32, pan: f32) -> Voice {
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
            pan,
        });
        Voice(voice)
    }

    /// The sound's interleaved stereo render, rendering it here the first time.
    fn samples(&self, sound: Sound) -> Option<Arc<[f32]>> {
        if let Some(frames) = lock(&self.inner.samples).get(&sound.0) {
            return Some(frames.clone());
        }
        let doc = lock(&self.inner.registry).docs.get(&sound.0)?.clone();
        let (left, right) = tono_core::player::render_stereo(&doc);
        let frames: Arc<[f32]> = left
            .iter()
            .zip(&right)
            .flat_map(|(&l, &r)| [l, r])
            .collect();
        lock(&self.inner.samples).insert(sound.0, frames.clone());
        Some(frames)
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

    /// Cap the voices sounding at once (SFX and beds; music and instruments
    /// are not counted). The engine's own mechanism: once full, a new voice
    /// steals the sounding voice of lowest [`Priority`] — the oldest on a tie
    /// — when that is not above its own, and is dropped otherwise. A stolen
    /// voice fades out over a few milliseconds. Unlimited until called; 0
    /// is treated as 1.
    pub fn set_max_voices(&self, max: usize) {
        self.send(Command::MaxVoices(max));
    }

    /// Stop a voice (a short fade avoids a click).
    pub fn stop(&self, voice: Voice) {
        self.send(Command::Stop(voice.0));
    }

    /// Set a voice's gain (0..).
    pub fn set_gain(&self, voice: Voice, gain: f32) {
        self.send(Command::Gain(voice.0, gain));
    }

    /// Set a voice's left and right levels, slewing linearly over `ramp` —
    /// for a continuous, panned bed (wind that follows speed, a stream that
    /// moves with the camera). Start the bed with
    /// [`play_looping`](Self::play_looping) or a looping [`Play`] and call
    /// this as often as the game likes.
    ///
    /// Beds are voices, not music: [`music_pause`](Self::music_pause) and
    /// [`music_reset`](Self::music_reset) leave them playing. The levels map
    /// onto the voice's gain (the louder side) and pan (the engine's balance
    /// law), so a later [`set_gain`](Self::set_gain) scales both sides.
    pub fn set_levels(&self, voice: Voice, levels: [f32; 2], ramp: std::time::Duration) {
        let level = |x: f32| if x.is_nan() { 0.0 } else { x.max(0.0) };
        let frames = (ramp.as_secs_f64() * self.inner.sample_rate as f64).round();
        let levels = [level(levels[0]), level(levels[1])];
        self.send(Command::Levels(voice.0, levels, frames as u32));
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
        registry.sections.push(name);
        self.send(Command::Section(buffer));
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
        self.transition_to_section_with(section, quantize, SectionEntry::default());
    }

    /// [`transition_to_section`](Self::transition_to_section) with a choice
    /// of how the section enters (see [`SectionEntry`]): from where it left
    /// off, and/or on a grid counted from the sounding section's entry.
    pub fn transition_to_section_with(
        &self,
        section: usize,
        quantize: Quantize,
        entry: SectionEntry,
    ) {
        self.send(Command::Transition(section, quantize, entry));
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
    /// `0.0..=1.0` — a global volume knob. Clamped, and ramped over 10 ms so
    /// a change never steps.
    pub fn set_master_gain(&self, gain: f32) {
        let gain = if gain.is_nan() { 0.0 } else { gain };
        self.send(Command::MasterGain(gain.clamp(0.0, 1.0)));
    }

    /// Set the music bus gain (the bed, its sections and stingers),
    /// `0.0..=1.0`, ramped linearly over `ramp`. Separate from
    /// [`music_duck`](Self::music_duck) and the intensity cross-fade, which
    /// keep working underneath it — a settings slider or a scene fade.
    pub fn set_music_gain(&self, gain: f32, ramp: std::time::Duration) {
        let gain = if gain.is_nan() { 0.0 } else { gain };
        let frames = (ramp.as_secs_f64() * self.inner.sample_rate as f64).round();
        self.send(Command::MusicGain(gain.clamp(0.0, 1.0), frames as u32));
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
///
/// The fields mirror [`Play`]; build one with [`PlaySfx::new`] and the
/// `with_*` methods.
#[derive(Message, Clone, Copy, Debug)]
pub struct PlaySfx {
    /// The registered sound to play.
    pub sound: Sound,
    /// Playback gain (1.0 = unity).
    pub gain: f32,
    /// Stereo balance, −1..1 (see [`Play::pan`]).
    pub pan: f32,
    /// Playback rate (see [`Play::rate`]).
    pub rate: f32,
    /// Per-voice lowpass cutoff in Hz (see [`Play::lowpass_hz`]).
    pub lowpass_hz: Option<f32>,
    /// Importance under a voice cap.
    pub priority: Priority,
    /// Loop until stopped.
    pub looping: bool,
}

impl PlaySfx {
    /// Play `sound` at unity gain.
    pub fn new(sound: Sound) -> Self {
        let play = Play::default();
        PlaySfx {
            sound,
            gain: play.gain,
            pan: play.pan,
            rate: play.rate,
            lowpass_hz: play.lowpass_hz,
            priority: play.priority,
            looping: play.looping,
        }
    }

    /// Set the gain (0..).
    pub fn with_gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }

    /// Set the stereo balance (−1..1).
    pub fn with_pan(mut self, pan: f32) -> Self {
        self.pan = pan;
        self
    }

    /// Set the playback rate (pitch and length together).
    pub fn with_rate(mut self, rate: f32) -> Self {
        self.rate = rate;
        self
    }

    /// Filter this voice through a one-pole lowpass at `hz`.
    pub fn with_lowpass(mut self, hz: f32) -> Self {
        self.lowpass_hz = Some(hz);
        self
    }

    /// Set the priority under a voice cap.
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Loop until stopped (the voice is not returned, so a looping message
    /// suits a bed that plays for the app's life).
    pub fn looping(mut self) -> Self {
        self.looping = true;
        self
    }

    /// The per-play controls this message carries.
    pub fn play(&self) -> Play {
        Play {
            gain: self.gain,
            pan: self.pan,
            rate: self.rate,
            lowpass_hz: self.lowpass_hz,
            priority: self.priority,
            looping: self.looping,
        }
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
        audio.play_with(e.sound, e.play());
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

/// Options [`TonoPlugin`] reads when it starts. Insert it before adding the
/// plugin; without one, every option is off and the output is as before.
///
/// ```no_run
/// # use bevy::prelude::*;
/// use bevy_tono::{Limiter, TonoConfig, TonoPlugin};
///
/// App::new()
///     .insert_resource(TonoConfig {
///         limiter: Some(Limiter::default()),
///         ..TonoConfig::default()
///     })
///     .add_plugins(TonoPlugin);
/// ```
#[derive(Resource, Clone, Debug, Default)]
pub struct TonoConfig {
    /// A peak limiter on the final mix, so coincident loud cues bend under a
    /// ceiling instead of clipping. Off by default.
    pub limiter: Option<Limiter>,
    /// Hold the music bed while the window is unfocused or the app is
    /// suspended, and pick it up where it was on return. Separate from
    /// [`music_pause`](TonoAudio::music_pause): a game's own pause stays as
    /// the game left it, and [`music_is_paused`](TonoAudio::music_is_paused)
    /// reports only that. SFX and beds keep playing. Off by default.
    pub pause_music_on_focus_loss: bool,
}

/// A stereo-linked peak limiter on the output: it pulls the gain down at
/// once when a frame would pass `ceiling`, then recovers over `release`. Both
/// channels share one gain, so the stereo image does not shift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limiter {
    /// The highest sample magnitude let through (linear, e.g. 0.95).
    pub ceiling: f32,
    /// How long the gain takes to recover (a one-pole time constant).
    pub release: std::time::Duration,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter {
            ceiling: 0.95,
            release: std::time::Duration::from_millis(100),
        }
    }
}

/// The plugin: sets up real-time audio, inserts [`TonoAudio`], and wires the
/// [`PlaySfx`] / [`PlayNote`] / [`StopNote`] messages so systems can make sound
/// without touching the resource.
pub struct TonoPlugin;

impl Plugin for TonoPlugin {
    fn build(&self, app: &mut App) {
        let config = app
            .world()
            .get_resource::<TonoConfig>()
            .cloned()
            .unwrap_or_default();
        install(app, output::spawn_audio(&config), &config);
    }
}

/// Insert the resource and wire the messages that feed it.
fn install(app: &mut App, audio: TonoAudio, config: &TonoConfig) {
    app.insert_resource(audio)
        .add_message::<PlaySfx>()
        .add_message::<PlayNote>()
        .add_message::<StopNote>()
        .add_systems(Update, (play_queued_sfx, drain_notes));
    if config.pause_music_on_focus_loss {
        use bevy::ecs::message::Messages;
        use bevy::window::{AppLifecycle, WindowFocused};
        app.add_systems(
            PreUpdate,
            (
                follow_focus.run_if(resource_exists::<Messages<WindowFocused>>),
                follow_lifecycle.run_if(resource_exists::<Messages<AppLifecycle>>),
            ),
        );
    }
}

/// Hold the music while the window is unfocused.
fn follow_focus(mut focus: MessageReader<bevy::window::WindowFocused>, audio: Res<TonoAudio>) {
    if let Some(e) = focus.read().last() {
        audio.send(Command::Suspend(!e.focused));
    }
}

/// Hold the music while the app is suspended (sent to the background).
fn follow_lifecycle(
    mut lifecycle: MessageReader<bevy::window::AppLifecycle>,
    audio: Res<TonoAudio>,
) {
    use bevy::window::AppLifecycle;
    for e in lifecycle.read() {
        match e {
            AppLifecycle::WillSuspend | AppLifecycle::Suspended => {
                audio.send(Command::Suspend(true));
            }
            AppLifecycle::WillResume | AppLifecycle::Running => {
                audio.send(Command::Suspend(false));
            }
            AppLifecycle::Idle => {}
        }
    }
}
