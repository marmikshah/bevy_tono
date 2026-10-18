//! Horizontal sections: one looping bed swapped for another on a beat or
//! bar, with a short declick cross-fade.
//!
//! This follows `AdaptiveMusic`'s own sections sample for sample (the tests
//! hold it to that), and adds two things they cannot do: entering a section
//! where it left off instead of at its head, and counting the beat/bar grid
//! from the frame the sounding section entered instead of from the last
//! reset. It runs as the bed's first layer, always at full gain, so the
//! duck, pause and reset of the bed apply to it as before.

use std::sync::{Arc, Mutex};

use tono_core::adaptive::{LoopBuffer, Quantize};
use tono_core::runtime::{AudioSource, Transport};

use crate::SectionEntry;

/// The cross-fade between sections, as `AdaptiveMusic` runs it.
const FADE_SECS: f32 = 0.06;

struct Section {
    buffer: LoopBuffer,
    /// The frame this section last entered (its fade began, or it started).
    entered: u64,
}

/// A cross-fade from the current section to `to`. The gain is computed from
/// the absolute frame count, so the ramp is exact for any block size; a
/// negative step is a cancelled transition fading back.
struct Fade {
    to: usize,
    from_gain: f32,
    step: f32,
    frames_done: usize,
}

/// A transition waiting for its frame.
#[derive(Clone, Copy)]
struct Pending {
    at: u64,
    to: usize,
    resume: bool,
}

pub(crate) struct Sections {
    sections: Vec<Section>,
    current: Option<usize>,
    fade: Option<Fade>,
    pending: Option<Pending>,
    /// The bed's clock, mirrored: advanced by every frame the bed renders.
    clock: Transport,
    bpm: f32,
    beats_per_bar: u32,
    sample_rate: u32,
    step: f32,
    scratch: Vec<f32>,
}

/// A free-running constant-tempo clock, as the bed builds its own.
fn clock_at(sample_rate: u32, bpm: f32, beats_per_bar: u32) -> Transport {
    let mut clock = Transport::constant(sample_rate, bpm, beats_per_bar);
    clock.play();
    clock
}

impl Sections {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Sections {
            sections: Vec::with_capacity(16),
            current: None,
            fade: None,
            pending: None,
            clock: clock_at(sample_rate, 0.0, 4),
            bpm: 0.0,
            beats_per_bar: 4,
            sample_rate,
            step: 1.0 / (FADE_SECS * sample_rate as f32),
            scratch: vec![0.0; 2048],
        }
    }

    pub(crate) fn add(&mut self, buffer: LoopBuffer) -> usize {
        let index = self.sections.len();
        let entered = self.clock.position_frames();
        self.sections.push(Section { buffer, entered });
        if self.current.is_none() {
            self.current = Some(index);
        }
        index
    }

    pub(crate) fn current(&self) -> Option<usize> {
        self.current
    }

    /// The bed's new tempo; the position carries over.
    pub(crate) fn set_tempo(&mut self, bpm: f32, beats_per_bar: u32) {
        self.bpm = bpm.max(0.0);
        self.beats_per_bar = beats_per_bar;
        let position = self.clock.position_frames();
        self.clock = clock_at(self.sample_rate, self.bpm, beats_per_bar);
        self.clock.seek_frame(position);
    }

    /// Cross-fade to `section` on a boundary. A transition supersedes a
    /// pending one; asking for the fade's source mid-fade reverses it.
    pub(crate) fn transition_to(&mut self, section: usize, q: Quantize, entry: SectionEntry) {
        if section >= self.sections.len() {
            return;
        }
        self.pending = None;
        if let Some(f) = &self.fade {
            if f.to == section {
                return;
            }
            if self.current == Some(section) {
                // Reverse from the gain the next sample would have used.
                self.fade = Some(Fade {
                    to: f.to,
                    from_gain: f.from_gain + f.frames_done as f32 * f.step,
                    step: -f.step,
                    frames_done: 0,
                });
                return;
            }
            // A third section queues behind the fade (see `begin`).
        }
        if self.current == Some(section) {
            return;
        }
        let at = self.fire_frame(q, entry.from_entry);
        if at <= self.clock.position_frames() {
            self.begin(section, entry.resume);
        } else {
            self.pending = Some(Pending {
                at,
                to: section,
                resume: entry.resume,
            });
        }
    }

    /// The frame `q` lands on: the next boundary strictly ahead, on the grid
    /// from the last reset, or from the sounding section's entry.
    fn fire_frame(&self, q: Quantize, from_entry: bool) -> u64 {
        let now = self.clock.position_frames();
        if self.bpm <= 0.0 {
            return now;
        }
        let next = |clock: &Transport| match q {
            Quantize::Beat => clock.next_beat(),
            Quantize::Bar => clock.next_bar(1),
            Quantize::Bars(n) => clock.next_bar(n),
            _ => clock.position_frames(),
        };
        match q {
            Quantize::Immediate => now,
            Quantize::AtBeat(n) => self.clock.frame_at_beat(n as f64),
            _ if from_entry => {
                let origin = self.origin().min(now);
                let mut phrase = clock_at(self.sample_rate, self.bpm, self.beats_per_bar);
                phrase.seek_frame(now - origin);
                origin + next(&phrase)
            }
            _ => next(&self.clock),
        }
    }

    /// The entry frame of the section that is, or is becoming, current.
    fn origin(&self) -> u64 {
        let section = match &self.fade {
            Some(f) if f.step > 0.0 => Some(f.to),
            _ => self.current,
        };
        section.map_or(0, |s| self.sections[s].entered)
    }

    /// Start the cross-fade to `to` now, from its head unless resuming.
    fn begin(&mut self, to: usize, resume: bool) {
        if self.current == Some(to) {
            return;
        }
        let now = self.clock.position_frames();
        if self.current.is_none() {
            self.current = Some(to);
            self.sections[to].entered = now;
            return;
        }
        if let Some(f) = &self.fade {
            if f.to == to {
                return;
            }
            // Let the fade in flight settle first, then go on: cutting over
            // now would drop its target mid-ramp.
            let g_now = f.from_gain + f.frames_done as f32 * f.step;
            let wait = if f.step > 0.0 {
                (1.0 - g_now) / f.step
            } else {
                g_now / -f.step
            };
            self.pending = Some(Pending {
                at: now + wait.ceil().max(1.0) as u64,
                to,
                resume,
            });
            return;
        }
        if !resume {
            self.sections[to].buffer.reset();
        }
        self.sections[to].entered = now;
        self.fade = Some(Fade {
            to,
            from_gain: 0.0,
            step: self.step,
            frames_done: 0,
        });
    }

    /// Frames until the fade in flight reaches its end gain.
    fn fade_frames_left(&self) -> Option<usize> {
        let f = self.fade.as_ref()?;
        let settled = |n: usize| {
            let g = (f.from_gain + (f.frames_done + n) as f32 * f.step).clamp(0.0, 1.0);
            if f.step > 0.0 { g >= 1.0 } else { g <= 0.0 }
        };
        let end = if f.step > 0.0 { 1.0 } else { 0.0 };
        let estimate = ((end - f.from_gain) / f.step).ceil() as i64 - f.frames_done as i64;
        let mut n = estimate.max(1) as usize;
        while n > 1 && settled(n - 1) {
            n -= 1;
        }
        while !settled(n) {
            n += 1;
        }
        Some(n)
    }

    fn fire_due(&mut self) {
        let now = self.clock.position_frames();
        if let Some(p) = self.pending.filter(|p| p.at <= now) {
            self.pending = None;
            self.begin(p.to, p.resume);
        }
    }

    /// Render one uninterrupted span into `out` (already zeroed).
    fn render_span(&mut self, out: &mut [f32], frames: usize) {
        if self.scratch.len() < frames * 2 {
            self.scratch.resize(frames * 2, 0.0);
        }
        let scratch = &mut self.scratch[..frames * 2];
        if let Some((to, start, step, done)) = self
            .fade
            .as_ref()
            .map(|f| (f.to, f.from_gain, f.step, f.frames_done))
        {
            let g = |f: usize| (start + (done + f) as f32 * step).clamp(0.0, 1.0);
            if let Some(cur) = self.current {
                self.sections[cur].buffer.fill(scratch);
                for f in 0..frames {
                    let g = g(f);
                    out[f * 2] += scratch[f * 2] * (1.0 - g);
                    out[f * 2 + 1] += scratch[f * 2 + 1] * (1.0 - g);
                }
            }
            self.sections[to].buffer.fill(scratch);
            for f in 0..frames {
                let g = g(f);
                out[f * 2] += scratch[f * 2] * g;
                out[f * 2 + 1] += scratch[f * 2 + 1] * g;
            }
            let end = g(frames);
            if end >= 1.0 {
                self.current = Some(to);
                self.fade = None;
            } else if end <= 0.0 {
                self.fade = None;
            } else if let Some(f) = self.fade.as_mut() {
                f.frames_done = done + frames;
            }
        } else if let Some(cur) = self.current {
            self.sections[cur].buffer.fill(scratch);
            for f in 0..frames {
                out[f * 2] += scratch[f * 2];
                out[f * 2 + 1] += scratch[f * 2 + 1];
            }
        }
    }
}

impl AudioSource for Sections {
    /// Fire each transition at its exact frame, rendering sub-spans around
    /// it, so the output is the same for any block size.
    fn fill(&mut self, out: &mut [f32]) -> usize {
        let frames = out.len() / 2;
        out.fill(0.0);
        let mut done = 0;
        while done < frames {
            self.fire_due();
            let position = self.clock.position_frames();
            let mut span = match self.pending.map(|p| p.at).filter(|&at| at > position) {
                Some(at) => ((at - position) as usize).min(frames - done),
                None => frames - done,
            };
            // End the span where a fade settles, so the section left behind
            // stops on that frame whatever the block size (its gain there is
            // already 0, so the sound is the same).
            if let Some(left) = self.fade_frames_left() {
                span = span.min(left);
            }
            self.render_span(&mut out[done * 2..(done + span) * 2], span);
            self.clock.advance(span as u64);
            done += span;
        }
        frames
    }

    /// The bed's reset: every section back to its head, the clock to 0, any
    /// fade or pending transition dropped. The current section stays.
    fn reset(&mut self) {
        self.clock.stop();
        self.clock.play();
        self.fade = None;
        self.pending = None;
        for s in &mut self.sections {
            s.buffer.reset();
            s.entered = 0;
        }
    }
}

/// The sections as a layer of the bed. The mixer keeps the other handle; both
/// live on the audio thread, so the lock is never contended.
pub(crate) struct SectionsLayer(pub(crate) Arc<Mutex<Sections>>);

impl AudioSource for SectionsLayer {
    fn fill(&mut self, out: &mut [f32]) -> usize {
        crate::lock(&self.0).fill(out)
    }

    fn reset(&mut self) {
        crate::lock(&self.0).reset();
    }
}
