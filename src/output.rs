//! The audio thread: open the device and hand the [`Mixer`] to its callback,
//! or, with no device, drain the mixer in real time so the music clock and the
//! command queue keep moving.

use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::mixer::Mixer;
use crate::{FALLBACK_SAMPLE_RATE, TonoAudio, TonoConfig};

/// Frames the callback mixes per pass. The scratch is fixed at this size, so
/// any device buffer is served in chunks without allocating.
const CHUNK: usize = 512;

/// How often the headless drain wakes.
const DRAIN_TICK: Duration = Duration::from_millis(10);

/// Spawn the audio thread and return the resource that talks to its mixer.
/// Never fails: a missing device, or a format it cannot open, degrades to a
/// silent drain at [`FALLBACK_SAMPLE_RATE`].
pub(crate) fn spawn_audio(config: &TonoConfig) -> TonoAudio {
    let (ready, result) = std::sync::mpsc::sync_channel(1);
    let thread_config = config.clone();
    let spawned = std::thread::Builder::new()
        .name("bevy_tono-audio".into())
        .spawn(move || {
            // The stream stays on the thread that built it: some hosts need that.
            match open_stream(&thread_config) {
                Ok((stream, audio)) => {
                    ready.send(audio).ok();
                    let _stream = stream;
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => eprintln!("bevy_tono: audio disabled ({e})"),
            }
            let (audio, mixer) = TonoAudio::with_mixer(FALLBACK_SAMPLE_RATE, &thread_config);
            ready.send(audio).ok();
            drain(mixer);
        });
    let fallback = || TonoAudio::with_mixer(FALLBACK_SAMPLE_RATE, config).0;
    if spawned.is_err() {
        return fallback();
    }
    result.recv().unwrap_or_else(|_| fallback())
}

/// Open the default output device and start a stream whose callback owns the
/// mixer.
fn open_stream(config: &TonoConfig) -> anyhow::Result<(cpal::Stream, TonoAudio)> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let stream_config: cpal::StreamConfig = supported.into();
    let (audio, mixer) = TonoAudio::with_mixer(stream_config.sample_rate.0, config);
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &stream_config, mixer)?,
        cpal::SampleFormat::I16 => build::<i16>(&device, &stream_config, mixer)?,
        cpal::SampleFormat::U16 => build::<u16>(&device, &stream_config, mixer)?,
        other => anyhow::bail!("unsupported output sample format {other:?}"),
    };
    stream.play()?;
    Ok((stream, audio))
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut mixer: Mixer,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = (config.channels as usize).max(1);
    let mut stereo = vec![0.0f32; CHUNK * 2];
    device.build_output_stream(
        config,
        move |data: &mut [T], _| write_device(&mut mixer, data, channels, &mut stereo),
        |e| eprintln!("bevy_tono: stream error: {e}"),
        None,
    )
}

/// The callback body: mix in fixed chunks and lay each one into the device's
/// channel layout (mono gets the mid, extra channels are zeroed).
pub(crate) fn write_device<T>(
    mixer: &mut Mixer,
    data: &mut [T],
    channels: usize,
    stereo: &mut [f32],
) where
    T: cpal::Sample + cpal::FromSample<f32>,
{
    let chunk_frames = stereo.len() / 2;
    for chunk in data.chunks_mut(chunk_frames * channels) {
        let frames = chunk.len() / channels;
        let mixed = &mut stereo[..frames * 2];
        mixer.fill(mixed);
        for (frame, lr) in chunk
            .chunks_exact_mut(channels)
            .zip(mixed.as_chunks::<2>().0)
        {
            let (l, r) = (lr[0], lr[1]);
            if channels == 1 {
                frame[0] = T::from_sample(0.5 * (l + r));
            } else {
                frame[0] = T::from_sample(l);
                frame[1] = T::from_sample(r);
                for c in &mut frame[2..] {
                    *c = T::from_sample(0.0f32);
                }
            }
        }
    }
}

/// With no device, mix into nothing at the device rate, forever. The music
/// clock advances as if a device were pulling, so beats and quantized
/// changes still land, and the command queue cannot grow without bound.
fn drain(mut mixer: Mixer) -> ! {
    let mut drained = Drain::new(mixer.sample_rate());
    let mut buffer = vec![0.0f32; CHUNK * 2];
    loop {
        drained.catch_up(Instant::now(), &mut mixer, &mut buffer);
        std::thread::sleep(DRAIN_TICK);
    }
}

/// Real-time pacing for the headless drain.
pub(crate) struct Drain {
    start: Option<Instant>,
    frames: u64,
    sample_rate: u32,
}

impl Drain {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Drain {
            start: None,
            frames: 0,
            sample_rate,
        }
    }

    /// Mix every frame due by `now`. After a long stall (a suspended process)
    /// it skips ahead rather than mixing more than a second at once.
    pub(crate) fn catch_up(&mut self, now: Instant, mixer: &mut Mixer, buffer: &mut [f32]) {
        let start = *self.start.get_or_insert(now);
        let rate = self.sample_rate as u64;
        let due = (now.duration_since(start).as_secs_f64() * rate as f64) as u64;
        self.frames = self.frames.max(due.saturating_sub(rate));
        let chunk = (buffer.len() / 2) as u64;
        while self.frames < due {
            let n = (due - self.frames).min(chunk) as usize;
            mixer.fill(&mut buffer[..n * 2]);
            self.frames += n as u64;
        }
    }
}
