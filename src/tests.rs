use super::*;
use tono_core::adaptive::AdaptiveMusic;
use tono_core::runtime::{AudioSource, InstanceHandle, Tween};

fn doc(json: &str) -> SoundDoc {
    serde_json::from_str(json).unwrap()
}

fn blip() -> SoundDoc {
    doc(
        r#"{ "name":"blip", "duration":0.1, "engine":2, "root": { "type":"mul", "inputs": [
            { "type":"sine", "freq":880 },
            { "type":"env", "a":0.002, "d":0.05, "s":0.0, "r":0.02 } ] } }"#,
    )
}

fn pad() -> SoundDoc {
    doc(
        r#"{ "name":"pad", "duration":0.5, "engine":2, "root": { "type":"chain", "stages": [
            { "type":"sawtooth", "freq":110 },
            { "type":"lowpass", "cutoff":900, "q":0.5 } ] } }"#,
    )
}

fn long_phrase() -> SoundDoc {
    doc(
        r#"{ "name":"phrase", "duration":12.0, "engine":2, "root": { "type":"mul", "inputs": [
            { "type":"sawtooth", "freq":220 },
            { "type":"env", "a":0.01, "d":2.0, "s":0.6, "r":1.0 } ] } }"#,
    )
}

fn peak(out: &[f32]) -> f32 {
    out.iter().fold(0.0f32, |m, &x| m.max(x.abs()))
}

/// A resource and its mixer with no device or thread: the test pulls blocks.
fn manual() -> (TonoAudio, Mixer) {
    TonoAudio::with_mixer(48_000, &TonoConfig::default())
}

/// The plugin's messages and systems over a manual mixer.
fn audio_app() -> (App, TonoAudio, Mixer) {
    let (audio, mixer) = manual();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    install(&mut app, audio.clone(), &TonoConfig::default());
    (app, audio, mixer)
}

/// Mix `frames` frames in one block.
fn pull(mixer: &mut Mixer, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * 2];
    mixer.fill(&mut out);
    out
}

fn bits(out: &[f32]) -> Vec<u32> {
    out.iter().map(|x| x.to_bits()).collect()
}

/// The 0.6 bus — engine, music and instruments summed under a lock — kept
/// here as the reference the mixer must match sample for sample.
struct OldBus {
    engine: Engine,
    music: AdaptiveMusic,
    scratch: Vec<f32>,
    master_gain: f32,
}

impl OldBus {
    fn new() -> Self {
        OldBus {
            engine: Engine::new(48_000),
            music: AdaptiveMusic::new(48_000),
            scratch: Vec::new(),
            master_gain: 1.0,
        }
    }

    fn at_rate(doc: &SoundDoc) -> SoundDoc {
        let mut d = doc.clone();
        d.sample_rate = 48_000;
        d
    }

    fn play(&mut self, patch: PatchId, how: Spawn, gain: f32) -> InstanceHandle {
        let mut voice = self.engine.prepare(patch).unwrap();
        voice.render();
        let h = self.engine.spawn_prepared(voice, how);
        if gain != 1.0 {
            self.engine.set_gain(h, gain, Tween::IMMEDIATE);
        }
        h
    }

    fn fill(&mut self, out: &mut [f32]) {
        self.engine.fill(out);
        let len = out.len();
        if self.scratch.len() < len {
            self.scratch.resize(len, 0.0);
        }
        self.music.fill(&mut self.scratch[..len]);
        for (o, &s) in out.iter_mut().zip(self.scratch[..len].iter()) {
            *o += s;
        }
        let g = self.master_gain;
        for o in out.iter_mut() {
            *o = (*o * g).clamp(-1.0, 1.0);
        }
    }
}

#[test]
fn the_mixer_matches_the_old_bus_sample_for_sample() {
    let (audio, mut mixer) = manual();
    let mut old = OldBus::new();
    let compare = |mixer: &mut Mixer, old: &mut OldBus, blocks: usize| {
        for n in 0..blocks {
            let frames = 128 + 61 * (n % 5); // uneven block sizes
            let new = pull(mixer, frames);
            let mut reference = vec![0.0f32; frames * 2];
            old.fill(&mut reference);
            assert_eq!(bits(&new), bits(&reference), "block {n}");
        }
    };

    let sound = audio.register(&blip());
    let patch = old.engine.load(&OldBus::at_rate(&blip()));
    audio.music_layer(&pad(), 0.0);
    old.music
        .add_layer(LoopBuffer::from_doc(&OldBus::at_rate(&pad())), 0.0);
    audio.music_layer(&blip(), 0.5);
    old.music
        .add_layer(LoopBuffer::from_doc(&OldBus::at_rate(&blip())), 0.5);
    audio.set_music_tempo(150.0, 4);
    old.music.set_tempo(150.0, 4);
    compare(&mut mixer, &mut old, 4);

    audio.play(sound);
    old.play(patch, Spawn::default(), 1.0);
    audio.play_with_gain(sound, 0.4);
    old.play(patch, Spawn::default(), 0.4);
    let looping = Spawn {
        looping: true,
        ..Spawn::default()
    };
    let bed = audio.play_looping(sound);
    let h = old.play(patch, looping, 1.0);
    audio.set_intensity(0.8);
    old.music.set_intensity(0.8);
    audio.stinger_at(&blip(), Quantize::Beat);
    old.music
        .stinger_at(&OldBus::at_rate(&blip()), Quantize::Beat);
    compare(&mut mixer, &mut old, 40);

    audio.add_section("a", &pad());
    old.music
        .add_section_buffer("a", LoopBuffer::from_doc(&OldBus::at_rate(&pad())));
    audio.add_section("b", &blip());
    old.music
        .add_section_buffer("b", LoopBuffer::from_doc(&OldBus::at_rate(&blip())));
    audio.transition_to_section(1, Quantize::Bar);
    old.music.transition_to(1, Quantize::Bar);
    audio.music_duck(0.5, std::time::Duration::from_millis(120));
    old.music.duck(0.5, std::time::Duration::from_millis(120));
    audio.set_gain(bed, 0.3);
    old.engine.set_gain(h, 0.3, Tween::ms(20.0, 48_000));
    compare(&mut mixer, &mut old, 80);

    audio.stop(bed);
    old.engine.stop(h, Tween::ms(8.0, 48_000));
    audio.music_pause();
    old.music.pause();
    compare(&mut mixer, &mut old, 10);
    audio.music_resume();
    old.music.resume();
    audio.music_reset();
    old.music.reset();
    compare(&mut mixer, &mut old, 20);
}

#[test]
fn bus_plays_a_registered_sfx() {
    let (audio, mut mixer) = manual();
    let sound = audio.register(&blip());
    audio.play(sound);
    assert!(peak(&pull(&mut mixer, 512)) > 0.0, "the SFX sounds");
}

#[test]
fn master_gain_ramps_instead_of_stepping() {
    let (audio, mut mixer) = manual();
    audio.music_layer(&pad(), 0.0);
    let before = pull(&mut mixer, 512);
    audio.set_master_gain(0.0);
    let ramp = pull(&mut mixer, 480); // 10 ms
    assert!(peak(&ramp[..96]) > 0.0, "it does not drop at once");
    for (f, pair) in ramp.as_chunks::<2>().0.iter().enumerate().skip(1) {
        let jump = (pair[0] - ramp[(f - 1) * 2]).abs();
        assert!(jump < 0.1, "no step at frame {f}: {jump}");
    }
    assert!(peak(&before) > 0.1);
    assert_eq!(peak(&pull(&mut mixer, 512)), 0.0, "then the bus is silent");
}

#[test]
fn music_gain_ramps_the_music_only() {
    let (audio, mut mixer) = manual();
    audio.music_layer(&pad(), 0.0);
    audio.set_music_gain(0.0, std::time::Duration::from_millis(50));
    let fading = pull(&mut mixer, 2_400);
    assert!(peak(&fading[..480]) > 0.05, "it ramps");
    assert_eq!(peak(&pull(&mut mixer, 512)), 0.0, "the music is down");
    let sound = audio.register(&blip());
    audio.play(sound);
    assert!(peak(&pull(&mut mixer, 512)) > 0.1, "SFX are not music");
    audio.set_music_gain(1.0, std::time::Duration::ZERO);
    audio.music_duck(0.5, std::time::Duration::from_millis(100));
    pull(&mut mixer, 6_000);
    assert!(
        peak(&pull(&mut mixer, 512)) > 0.05,
        "back up, the duck on top"
    );
}

#[test]
fn the_limiter_holds_its_ceiling_and_is_off_by_default() {
    let loud = |config: &TonoConfig| {
        let (audio, mut mixer) = TonoAudio::with_mixer(48_000, config);
        let sound = audio.register(&pad());
        for _ in 0..4 {
            audio.play_with_gain(sound, 3.0);
        }
        pull(&mut mixer, 12_000)
    };
    let raw = loud(&TonoConfig::default());
    assert_eq!(peak(&raw), 1.0, "unlimited, it hits the clamp");
    let limited = loud(&TonoConfig {
        limiter: Some(Limiter {
            ceiling: 0.8,
            release: std::time::Duration::from_millis(50),
        }),
        ..TonoConfig::default()
    });
    assert!(peak(&limited) <= 0.8 + 1e-6, "{}", peak(&limited));
    assert!(peak(&limited) > 0.7);
}

#[test]
fn ecs_layer_plays_sfx_and_beds_a_song() {
    use tono_core::dsl::{Adsr, SeqWave};
    use tono_core::song::{Song, note};

    let (mut app, audio, mut mixer) = audio_app();

    // PlaySfx: a registered sound fired via the message queue is drained and
    // played by the plugin's system on update.
    let sound = audio.register(&blip());
    app.world_mut()
        .write_message(PlaySfx::new(sound).with_gain(0.7));
    app.update();
    pull(&mut mixer, 64);
    assert_eq!(mixer.engine.active(), 1, "the message reached the mixer");

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
    assert!(audio.play_song(&song).is_ok(), "the song compiles and beds");
}

#[test]
fn live_instrument_plays_a_held_note() {
    let (mut app, audio, mut mixer) = audio_app();

    // Register a preset instrument and play a note by message.
    let inst = audio.preset("vibrato_lead").expect("the preset exists");
    app.world_mut()
        .write_message(PlayNote::new(inst, Note::C4).with_velocity(0.9));
    app.update(); // drain_notes fires note_on

    // A held note sustains — pull a few buffers and confirm it sounds.
    let loudest = (0..8)
        .map(|_| peak(&pull(&mut mixer, 512)))
        .fold(0.0, f32::max);
    assert!(loudest > 0.0, "the held instrument note sounds");
}

#[test]
fn review_grades_a_rendered_doc_offline() {
    use crate::review::{self, Archetype, AuditItem, Status};

    let mut doc = blip();
    doc.sample_rate = 48_000;

    // A single doc renders + grades (no device) to a Review with findings.
    let r = review::grade(&doc, Some(Archetype::Ui));
    assert!(!r.findings.is_empty(), "the review produced findings");

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

#[test]
fn transport_position_pause_and_reset() {
    let (audio, mut mixer) = manual();
    audio.music_layer(&blip(), 0.0);

    assert_eq!(audio.music_position_frames(), 0);
    assert!(!audio.music_is_paused());

    pull(&mut mixer, 256);
    assert_eq!(audio.music_position_frames(), 256, "the clock advances");

    audio.music_pause();
    assert!(audio.music_is_paused(), "the pause reads back at once");
    pull(&mut mixer, 256);
    assert_eq!(
        audio.music_position_frames(),
        256,
        "the clock holds while paused"
    );

    audio.music_resume();
    audio.music_reset();
    assert_eq!(audio.music_position_frames(), 0, "reset reads back at once");
    pull(&mut mixer, 128);
    assert_eq!(audio.music_position_frames(), 128, "and counts from zero");
    audio.music_duck(0.5, std::time::Duration::from_millis(180));
}

#[test]
fn tempo_beats_and_sections() {
    let (audio, mut mixer) = manual();

    audio.set_music_tempo(120.0, 4);
    assert_eq!(audio.music_beats(), 0.0);
    assert_eq!(audio.music_bars(), 0.0);
    pull(&mut mixer, 48_000); // a second at 120 bpm: two beats
    assert_eq!(audio.music_beats(), 2.0);
    assert_eq!(audio.music_bars(), 0.5);

    // Sections: first added plays; lookups resolve by name and index.
    let explore = audio.add_section("explore", &blip());
    let battle = audio.add_section("battle", &blip());
    assert_eq!((explore, battle), (0, 1));
    assert_eq!(audio.current_section(), Some(0), "the first section plays");
    assert_eq!(audio.section_named("battle"), Some(1));
    assert_eq!(audio.section_named("nope"), None);

    audio.transition_to_section(battle, Quantize::Bar);
    audio.set_intensity_at(1.0, Quantize::Beat);
    audio.stinger_at(&blip(), Quantize::Beat);
    pull(&mut mixer, 512);
    assert_eq!(audio.current_section(), Some(0), "waits for the bar");
    pull(&mut mixer, 48_000 + 4_000); // past the bar line and the fade
    assert_eq!(audio.current_section(), Some(1), "the swap reads back");
}

#[test]
fn play_renders_outside_the_mixer() {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    let (audio, mut mixer) = manual();
    let phrase = audio.register(&long_phrase());

    // Stand in for the audio callback: keep mixing while the game thread
    // renders a 12 s phrase, timing the longest block.
    let done = Arc::new(AtomicBool::new(false));
    let callback = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut longest = Duration::ZERO;
            while !done.load(Ordering::Relaxed) {
                let t = Instant::now();
                pull(&mut mixer, 256);
                longest = longest.max(t.elapsed());
            }
            (longest, mixer)
        })
    };

    let start = Instant::now();
    audio.play(phrase); // first play: renders 12 s of audio
    let render = start.elapsed();
    std::thread::sleep(Duration::from_millis(20));
    done.store(true, Ordering::Relaxed);
    let (longest, mut mixer) = callback.join().unwrap();
    pull(&mut mixer, 64);

    assert_eq!(mixer.engine.active(), 1, "the phrase plays");
    assert!(
        longest < render / 2,
        "a block took {longest:?} during a {render:?} render"
    );
}

#[test]
fn the_mixer_never_waits_for_game_side_locks() {
    let (audio, mut mixer) = manual();
    let sound = audio.register(&blip());
    audio.play(sound);
    // Hold every lock the game side has while the "callback" mixes.
    let _registry = lock(&audio.inner.registry);
    let _voices = lock(&audio.inner.voices);
    let mixed = std::thread::scope(|s| s.spawn(|| peak(&pull(&mut mixer, 512))).join());
    assert!(mixed.unwrap() > 0.0, "the block mixed with the locks held");
}

#[test]
fn a_replayed_sound_reuses_its_render() {
    let (audio, mut mixer) = manual();
    let blip_sound = audio.register(&blip());
    assert!(lock(&audio.inner.voices).is_empty());
    audio.play(blip_sound);
    audio.play_with_gain(blip_sound, 0.5);
    audio.play_looping(blip_sound);
    {
        let voices = lock(&audio.inner.voices);
        assert_eq!(voices.len(), 1, "one render per sound");
        assert!(voices[&blip_sound.0].is_rendered());
    }
    pull(&mut mixer, 64);
    assert_eq!(mixer.engine.active(), 3);
}

#[test]
fn a_worker_rendered_stinger_fires_on_its_beat() {
    let (audio, mut mixer) = manual();
    audio.set_music_tempo(120.0, 4); // a beat every 0.5 s
    let beat = audio.music_beats().floor() as u64 + 2;

    let worker = audio.clone();
    let stinger = std::thread::spawn(move || worker.render_stinger(&blip()))
        .join()
        .unwrap();
    audio.play_stinger_at(&stinger, Quantize::AtBeat(beat));
    audio.play_stinger_at(&stinger, Quantize::AtBeat(beat + 1));

    let frames_per_beat = audio.sample_rate() as usize / 2;
    pull(&mut mixer, 64);
    assert_eq!(mixer.music.active_stingers(), 0, "waiting for the beat");
    pull(&mut mixer, beat as usize * frames_per_beat);
    assert_eq!(mixer.music.active_stingers(), 1, "fired on its beat");
    audio.play_stinger(&stinger); // the same render again, now
    pull(&mut mixer, 1);
    assert_eq!(mixer.music.active_stingers(), 2);
}

#[test]
fn the_headless_drain_keeps_the_clock_moving() {
    use std::time::{Duration, Instant};

    let (audio, mut mixer) = manual();
    audio.set_music_tempo(120.0, 4);
    let mut drain = output::Drain::new(48_000);
    let mut buffer = vec![0.0f32; 1024];
    let start = Instant::now();
    drain.catch_up(start, &mut mixer, &mut buffer);
    drain.catch_up(start + Duration::from_millis(250), &mut mixer, &mut buffer);
    assert_eq!(audio.music_position_frames(), 12_000, "a quarter second");
    assert_eq!(audio.music_beats(), 0.5);
    // A long stall skips ahead instead of mixing it all at once.
    drain.catch_up(start + Duration::from_secs(30), &mut mixer, &mut buffer);
    assert_eq!(
        audio.music_position_frames(),
        60_000,
        "at most a second more"
    );
}

#[test]
fn integer_devices_get_the_same_mix() {
    let render = |sample: fn(&mut Mixer, &mut [f32])| {
        let (audio, mut mixer) = manual();
        let sound = audio.register(&blip());
        audio.play(sound);
        let mut stereo = vec![0.0f32; 512 * 2];
        sample(&mut mixer, &mut stereo);
        stereo
    };
    let float = render(|mixer, stereo| mixer.fill(stereo));
    let wide = render(|mixer, stereo| {
        // A three-channel i16 device, served in 64-frame chunks.
        let mut data = vec![0i16; 512 * 3];
        let mut scratch = vec![0.0f32; 64 * 2];
        output::write_device(mixer, &mut data, 3, &mut scratch);
        for (lr, frame) in stereo
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(data.as_chunks::<3>().0)
        {
            assert_eq!(frame[2], 0, "extra channels are silent");
            lr[0] = frame[0] as f32 / i16::MAX as f32;
            lr[1] = frame[1] as f32 / i16::MAX as f32;
        }
    });
    let mono = render(|mixer, stereo| {
        let mut data = vec![0u16; 512];
        let mut scratch = vec![0.0f32; 512 * 2];
        output::write_device(mixer, &mut data, 1, &mut scratch);
        for (lr, &m) in stereo.as_chunks_mut::<2>().0.iter_mut().zip(&data) {
            let v = (m as f32 - 32_768.0) / 32_768.0;
            lr[0] = v;
            lr[1] = v;
        }
    });
    assert!(peak(&float) > 0.1);
    for (a, b) in float.iter().zip(&wide) {
        assert!((a - b).abs() < 1e-3, "i16 tracks f32");
    }
    for (pair, b) in float.as_chunks::<2>().0.iter().zip(mono.as_chunks::<2>().0) {
        assert!(
            (0.5 * (pair[0] + pair[1]) - b[0]).abs() < 1e-3,
            "u16 mono is the mid"
        );
    }
}

#[test]
fn the_plugin_starts_with_or_without_a_device() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(TonoPlugin);
    app.update();
    let audio = app.world().resource::<TonoAudio>().clone();
    let sound = audio.register(&blip());
    audio.play(sound);
    assert!(audio.sample_rate() > 0);
}

/// Mix one play on a fresh mixer for `frames` frames.
fn render_play(play: impl Fn(&TonoAudio, Sound), frames: usize) -> Vec<f32> {
    let (audio, mut mixer) = manual();
    let sound = audio.register(&blip());
    play(&audio, sound);
    pull(&mut mixer, frames)
}

#[test]
fn rate_one_is_bit_identical_to_play() {
    let plain = render_play(
        |a, s| {
            a.play(s);
        },
        6_000,
    );
    let with = render_play(
        |a, s| {
            a.play_with(
                s,
                Play {
                    rate: 1.0,
                    ..Play::default()
                },
            );
        },
        6_000,
    );
    assert!(peak(&plain) > 0.1);
    assert_eq!(bits(&plain), bits(&with));
    let gained = render_play(
        |a, s| {
            a.play_with_gain(s, 0.3);
        },
        6_000,
    );
    let with_gain = render_play(
        |a, s| {
            a.play_with(
                s,
                Play {
                    gain: 0.3,
                    ..Play::default()
                },
            );
        },
        6_000,
    );
    assert_eq!(bits(&gained), bits(&with_gain));
}

#[test]
fn rate_resamples_pitch_and_length_together() {
    let (audio, mut mixer) = manual();
    let sound = audio.register(&blip());
    let rendered = audio.samples(sound).unwrap();
    let frames = rendered.len() / 2;
    audio.play_with(
        sound,
        Play {
            rate: 2.0,
            ..Play::default()
        },
    );
    let out = pull(&mut mixer, frames);
    // At rate 2 every output frame is every second rendered frame, exactly.
    for f in 0..frames / 2 {
        assert_eq!(out[f * 2], rendered[f * 4], "frame {f}");
    }
    assert!(
        peak(&out[frames..]) == 0.0,
        "and it ends at half the length"
    );
    assert_eq!(mixer.sampled.len(), 0, "the voice finished");
    pull(&mut mixer, 1_000);
    assert_eq!(mixer.engine.active(), 0, "its engine slot is given back");
}

#[test]
fn lowpass_and_pan_shape_a_voice() {
    let open = render_play(
        |a, s| {
            a.play(s);
        },
        4_000,
    );
    let dark = render_play(
        |a, s| {
            a.play_with(
                s,
                Play {
                    lowpass_hz: Some(150.0),
                    ..Play::default()
                },
            );
        },
        4_000,
    );
    assert!(
        peak(&dark) < peak(&open) * 0.4,
        "880 Hz through a 150 Hz lowpass"
    );

    for rate in [1.0, 1.5] {
        let right = render_play(
            |a, s| {
                a.play_with(
                    s,
                    Play {
                        pan: 1.0,
                        rate,
                        ..Play::default()
                    },
                );
            },
            4_000,
        );
        let left: Vec<f32> = right.iter().step_by(2).copied().collect();
        assert_eq!(
            peak(&left),
            0.0,
            "hard right silences the left at rate {rate}"
        );
        assert!(peak(&right) > 0.1);
    }
}

#[test]
fn a_sampled_voice_stops_and_takes_gain() {
    let (audio, mut mixer) = manual();
    let sound = audio.register(&pad());
    let voice = audio.play_with(
        sound,
        Play {
            rate: 0.5,
            looping: true,
            ..Play::default()
        },
    );
    pull(&mut mixer, 48_000); // a 0.5 s doc at half rate, looped past its end
    assert_eq!(mixer.sampled.len(), 1, "the loop keeps playing");
    audio.set_gain(voice, 0.0);
    pull(&mut mixer, 2_000);
    assert_eq!(peak(&pull(&mut mixer, 256)), 0.0, "gain 0 silences it");
    audio.stop(voice);
    pull(&mut mixer, 2_000);
    assert_eq!(mixer.sampled.len(), 0);
    pull(&mut mixer, 1_000);
    assert_eq!(mixer.engine.active(), 0, "its slot is given back");
}

#[test]
fn play_sfx_builders_reach_the_mixer() {
    let (mut app, audio, mut mixer) = audio_app();
    let sound = audio.register(&blip());
    let sfx = PlaySfx::new(sound)
        .with_gain(0.5)
        .with_pan(-0.5)
        .with_rate(1.25)
        .with_lowpass(4_000.0)
        .with_priority(Priority::HIGH);
    assert_eq!(
        sfx.play(),
        Play {
            gain: 0.5,
            pan: -0.5,
            rate: 1.25,
            lowpass_hz: Some(4_000.0),
            priority: Priority::HIGH,
            looping: false,
        }
    );
    app.world_mut().write_message(sfx);
    app.world_mut().write_message(PlaySfx::new(sound).looping());
    app.update();
    pull(&mut mixer, 64);
    assert_eq!(mixer.sampled.len(), 1, "the resampled one");
    assert_eq!(mixer.engine.active(), 2, "its slot and the looping one");
}

#[test]
fn variation_varies_each_play_and_repeats_each_run() {
    let wide = Variation {
        pitch: 0.05,
        pan: 0.5,
        level: 0.2,
    };
    let run = || {
        let (audio, mut mixer) = manual();
        let sound = audio.register_with(&blip(), wide);
        let plays: Vec<Vec<u32>> = (0..3)
            .map(|_| {
                audio.play(sound);
                let out = bits(&pull(&mut mixer, 6_000));
                pull(&mut mixer, 6_000);
                out
            })
            .collect();
        plays
    };
    let first = run();
    assert_ne!(first[0], first[1], "repeats are not stamped out");
    assert_ne!(first[1], first[2]);
    assert_eq!(first, run(), "the same plays vary the same way every run");

    // Without a variation nothing changes.
    let (audio, mut mixer) = manual();
    let sound = audio.register_with(&blip(), Variation::default());
    audio.play(sound);
    let plain = render_play(
        |a, s| {
            a.play(s);
        },
        6_000,
    );
    assert_eq!(bits(&pull(&mut mixer, 6_000)), bits(&plain));
    assert!(mixer.sampled.is_empty(), "the engine plays it, as before");
}

#[test]
fn variation_stays_in_its_ranges() {
    let (audio, _mixer) = manual();
    let sound = audio.register_with(
        &blip(),
        Variation {
            pitch: 0.04,
            pan: 0.3,
            level: 0.1,
        },
    );
    for _ in 0..500 {
        let p = audio.varied(sound, Play::default());
        assert!((0.96..=1.04).contains(&p.rate), "{p:?}");
        assert!((-0.3..=0.3).contains(&p.pan), "{p:?}");
        assert!((0.9..=1.0).contains(&p.gain), "{p:?}");
    }
}

#[test]
fn the_voice_cap_is_respected_by_both_kinds_of_voice() {
    let (audio, mut mixer) = manual();
    let sound = audio.register(&long_phrase());
    audio.set_max_voices(3);
    let resampled = Play {
        rate: 1.5,
        ..Play::default()
    };
    let low = Play {
        priority: Priority::LOW,
        ..Play::default()
    };
    audio.play_with(sound, low);
    audio.play_with(sound, resampled);
    audio.play_with(
        sound,
        Play {
            priority: Priority::LOW,
            ..resampled
        },
    );
    pull(&mut mixer, 64);
    assert_eq!(mixer.engine.active(), 3);

    // Full: a normal voice steals the oldest low-priority one.
    audio.play(sound);
    pull(&mut mixer, 1_000);
    assert_eq!(mixer.engine.active(), 3, "the cap holds");
    assert_eq!(mixer.sampled.len(), 2, "an engine voice was stolen first");

    // Then a resampled normal voice steals the remaining low one.
    audio.play_with(sound, resampled);
    pull(&mut mixer, 1_000);
    assert_eq!(mixer.engine.active(), 3);
    assert_eq!(mixer.sampled.len(), 2, "the low resampled voice faded out");

    // Everything left is normal: a low voice is dropped.
    let dropped = audio.play_with(sound, low);
    pull(&mut mixer, 1_000);
    assert_eq!(mixer.engine.active(), 3, "the cap holds");
    assert!(
        mixer.handle(dropped.0).is_none(),
        "the low voice never sounded"
    );

    // A high voice always gets in.
    audio.play_with(
        sound,
        Play {
            priority: Priority::HIGH,
            ..resampled
        },
    );
    pull(&mut mixer, 1_000);
    assert_eq!(mixer.engine.active(), 3);
    assert_eq!(mixer.sampled.len(), 2, "one of the earlier ones made way");
}

#[test]
fn beds_keep_playing_through_music_pause_and_reset() {
    use std::time::Duration;

    let (audio, mut mixer) = manual();
    audio.music_layer(&pad(), 0.0);
    let sound = audio.register(&pad());
    let bed = audio.play_looping(sound);
    let wind = audio.play_with(
        sound,
        Play {
            rate: 0.8,
            looping: true,
            ..Play::default()
        },
    );
    audio.set_levels(bed, [0.0, 0.6], Duration::from_millis(20));
    audio.set_levels(wind, [0.0, 0.3], Duration::from_millis(20));
    pull(&mut mixer, 2_000);

    audio.music_pause();
    audio.music_reset();
    for _ in 0..10 {
        let out = pull(&mut mixer, 4_800);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        let right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(peak(&left), 0.0, "the levels put both beds hard right");
        assert!(
            peak(&right) > 0.05,
            "the beds play on with the music paused"
        );
    }
    assert_eq!(audio.music_position_frames(), 0, "the music clock held");

    // Levels slew rather than jump, and both sides can sound.
    audio.set_levels(bed, [0.5, 0.25], Duration::from_millis(100));
    let slewing = pull(&mut mixer, 480);
    let left: Vec<f32> = slewing.iter().step_by(2).copied().collect();
    assert!(
        peak(&left[..48]) < peak(&left[400..]),
        "the left side fades in"
    );
}

#[test]
fn levels_map_onto_gain_and_balance() {
    for [l, r] in [[1.0, 1.0], [0.6, 0.2], [0.1, 0.9], [0.0, 0.4], [0.0, 0.0]] {
        let (g, pan) = mixer::gain_and_pan(l, r);
        let (lg, rg) = mixer::balance(pan);
        assert!(
            (g * lg - l).abs() < 1e-6 && (g * rg - r).abs() < 1e-6,
            "{l} {r}"
        );
    }
}

#[test]
fn prewarm_renders_off_the_calling_thread() {
    use std::time::{Duration, Instant};

    let (audio, mut mixer) = manual();
    let phrase = audio.register(&long_phrase());
    let start = Instant::now();
    audio.prewarm(phrase);
    let returned = start.elapsed();
    let ready = |audio: &TonoAudio, sound: Sound| {
        lock(&audio.inner.voices).contains_key(&sound.0)
            && lock(&audio.inner.samples).contains_key(&sound.0)
    };
    while !ready(&audio, phrase) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "prewarm finished"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let rendered = start.elapsed();
    assert!(
        returned < rendered / 4,
        "returned in {returned:?} of {rendered:?}"
    );

    // Both first plays now reuse the render: plain, and at any rate.
    let start = Instant::now();
    audio.play(phrase);
    for rate in [0.5, 1.25, 2.0] {
        audio.play_with(
            phrase,
            Play {
                rate,
                ..Play::default()
            },
        );
    }
    assert!(
        start.elapsed() < rendered / 4,
        "no render on the first plays"
    );
    pull(&mut mixer, 64);
    assert_eq!(mixer.engine.active(), 4);
    assert_eq!(mixer.sampled.len(), 3, "every rate reads the one buffer");
    assert_eq!(lock(&audio.inner.samples).len(), 1);
}

#[test]
fn prewarm_uses_one_worker_for_many_sounds() {
    use std::time::{Duration, Instant};

    let (audio, _mixer) = manual();
    let sounds: Vec<Sound> = (0..45).map(|_| audio.register(&blip())).collect();
    for &s in &sounds {
        audio.prewarm(s);
    }
    let start = Instant::now();
    while lock(&audio.inner.samples).len() < sounds.len() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "prewarm finished"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(lock(&audio.inner.voices).len(), sounds.len());
    assert_eq!(
        audio.inner.prewarm_workers.load(Ordering::Relaxed),
        1,
        "one worker for all 45"
    );
}

#[test]
fn sections_match_the_beds_own_sections_sample_for_sample() {
    use crate::sections::Sections;

    let mut ours = Sections::new(48_000);
    let mut theirs = AdaptiveMusic::new(48_000);
    for f in [220, 330, 440] {
        ours.add(LoopBuffer::from_doc(&tone(f)));
        theirs.add_section_buffer(format!("{f}"), LoopBuffer::from_doc(&tone(f)));
    }
    ours.set_tempo(132.0, 3);
    theirs.set_tempo(132.0, 3);
    let plain = SectionEntry::default();
    let mut block = 0;
    let mut run = |ours: &mut Sections, theirs: &mut AdaptiveMusic, frames: usize| {
        let mut done = 0;
        while done < frames {
            let n = (97 + 211 * (block % 7)).min(frames - done);
            block += 1;
            let (mut a, mut b) = (vec![0.0f32; n * 2], vec![0.0f32; n * 2]);
            ours.fill(&mut a);
            theirs.fill(&mut b);
            assert_eq!(bits(&a), bits(&b), "block {block}");
            done += n;
        }
    };
    type Step = (usize, Quantize, usize);
    let steps: [Step; 9] = [
        (1, Quantize::Bar, 70_000),
        (2, Quantize::Beat, 1_000), // mid-fade: queues behind it
        (1, Quantize::Immediate, 6_000),
        (0, Quantize::Immediate, 900), // back to the source mid-fade: reverses
        (1, Quantize::Immediate, 600),
        (2, Quantize::Bars(2), 200_000),
        (0, Quantize::AtBeat(30), 50_000),
        (0, Quantize::Bar, 10_000),       // already current: a no-op
        (9, Quantize::Immediate, 10_000), // unknown: a no-op
    ];
    for (to, q, frames) in steps {
        ours.transition_to(to, q, plain);
        theirs.transition_to(to, q);
        run(&mut ours, &mut theirs, frames);
        assert_eq!(ours.current(), theirs.current_section());
    }
    ours.transition_to(1, Quantize::Bar, plain);
    theirs.transition_to(1, Quantize::Bar);
    AudioSource::reset(&mut ours);
    theirs.reset();
    run(&mut ours, &mut theirs, 100_000);
}

/// A tone's rendered left channel at `frame` (looped).
fn read_tone(freq: u32, frame: usize) -> f32 {
    let (left, _) = tono_core::player::render_stereo(&tone(freq));
    left[frame % left.len()]
}

#[test]
fn a_resumed_section_picks_up_where_it_left_off() {
    use crate::sections::Sections;

    let fade = 2_880; // 60 ms
    for resume in [false, true] {
        let mut s = Sections::new(48_000);
        s.add(LoopBuffer::from_doc(&tone(220)));
        s.add(LoopBuffer::from_doc(&tone(330)));
        let mut out = vec![0.0f32; 4_800 * 2];
        s.fill(&mut out); // 220 plays 4 800 frames
        s.transition_to(1, Quantize::Immediate, SectionEntry::default());
        s.fill(&mut vec![0.0f32; (fade + 1_000) * 2]); // 220 left at 4 800 + fade
        let entry = SectionEntry {
            resume,
            ..SectionEntry::default()
        };
        s.transition_to(0, Quantize::Immediate, entry);
        s.fill(&mut vec![0.0f32; fade * 2]);
        assert_eq!(s.current(), Some(0));
        let mut back = vec![0.0f32; 64 * 2];
        s.fill(&mut back);
        let head = if resume { 4_800 + fade + fade } else { fade };
        for f in 0..64 {
            assert_eq!(
                back[f * 2],
                read_tone(220, head + f),
                "resume {resume} frame {f}"
            );
        }
    }
}

#[test]
fn bars_can_count_from_the_sections_entry() {
    use crate::sections::Sections;

    let (bar, fade) = (96_000, 2_880); // 120 bpm, 4/4, 48 kHz; a 60 ms fade
    for (from_entry, lands) in [(false, bar), (true, 30_000 + bar)] {
        let mut s = Sections::new(48_000);
        s.set_tempo(120.0, 4);
        for f in [220, 330, 440] {
            s.add(LoopBuffer::from_doc(&tone(f)));
        }
        s.fill(&mut vec![0.0f32; 30_000 * 2]);
        s.transition_to(1, Quantize::Immediate, SectionEntry::default()); // enters at 30 000
        s.fill(&mut vec![0.0f32; 4_000 * 2]);
        let entry = SectionEntry {
            from_entry,
            ..SectionEntry::default()
        };
        s.transition_to(2, Quantize::Bars(1), entry);
        s.fill(&mut vec![0.0f32; (lands + fade - 34_000 - 1) * 2]);
        assert_eq!(s.current(), Some(1), "still fading in: {from_entry}");
        s.fill(&mut [0.0f32; 2 * 2]);
        assert_eq!(s.current(), Some(2), "landed on frame {lands}");
    }
}

#[test]
fn transition_with_reaches_the_mixer() {
    let (audio, mut mixer) = manual();
    let a = audio.add_section("a", &tone(220));
    let b = audio.add_section("b", &tone(330));
    let entry = SectionEntry {
        resume: true,
        from_entry: true,
    };
    audio.transition_to_section_with(b, Quantize::Immediate, entry);
    pull(&mut mixer, 4_000);
    assert_eq!(audio.current_section(), Some(b));
    audio.transition_to_section_with(a, Quantize::Immediate, entry);
    pull(&mut mixer, 4_000);
    assert_eq!(audio.current_section(), Some(a));
}

#[test]
fn music_holds_while_the_app_is_unfocused_or_suspended() {
    use bevy::window::{AppLifecycle, WindowFocused};

    let (audio, mut mixer) = manual();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_message::<WindowFocused>()
        .add_message::<AppLifecycle>();
    let config = TonoConfig {
        pause_music_on_focus_loss: true,
        ..TonoConfig::default()
    };
    install(&mut app, audio.clone(), &config);
    audio.music_layer(&pad(), 0.0);
    let focus = |app: &mut App, focused: bool| {
        app.world_mut().write_message(WindowFocused {
            window: Entity::PLACEHOLDER,
            focused,
        });
        app.update();
    };

    pull(&mut mixer, 1_000);
    focus(&mut app, false);
    pull(&mut mixer, 1_000);
    assert_eq!(audio.music_position_frames(), 1_000, "held while unfocused");
    assert!(
        !audio.music_is_paused(),
        "the game's own pause is untouched"
    );
    focus(&mut app, true);
    pull(&mut mixer, 1_000);
    assert_eq!(audio.music_position_frames(), 2_000, "and picks up again");

    // A game pause outlasts a focus round trip.
    audio.music_pause();
    focus(&mut app, false);
    focus(&mut app, true);
    pull(&mut mixer, 1_000);
    assert_eq!(
        audio.music_position_frames(),
        2_000,
        "still paused by the game"
    );
    audio.music_resume();

    app.world_mut().write_message(AppLifecycle::Suspended);
    app.update();
    pull(&mut mixer, 1_000);
    assert_eq!(audio.music_position_frames(), 2_000, "held while suspended");
    app.world_mut().write_message(AppLifecycle::Running);
    app.update();
    pull(&mut mixer, 1_000);
    assert_eq!(audio.music_position_frames(), 3_000);
}

#[test]
fn focus_is_ignored_unless_asked_for() {
    use bevy::window::WindowFocused;

    let (mut app, audio, mut mixer) = audio_app();
    app.add_message::<WindowFocused>();
    app.world_mut().write_message(WindowFocused {
        window: Entity::PLACEHOLDER,
        focused: false,
    });
    app.update();
    pull(&mut mixer, 1_000);
    assert_eq!(audio.music_position_frames(), 1_000);
}
fn tone(freq: u32) -> SoundDoc {
    let mut d = doc(&format!(
        r#"{{ "name":"t{freq}", "duration":0.5, "engine":2, "root": {{ "type":"sine", "freq":{freq} }} }}"#
    ));
    d.sample_rate = 48_000;
    d
}
