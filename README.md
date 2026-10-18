# bevy_tono

A [Bevy](https://bevyengine.org) plugin for the [tono](https://github.com/marmikshah/tono)
sound engine — fire deterministic SFX and drive adaptive music straight from
your game systems. No cpal/threads to wire up yourself.

```toml
[dependencies]
bevy_tono = "0.6"
```

That's the only dependency you need — `bevy_tono` re-exports the engine, so
`SoundDoc` and friends are reachable as `bevy_tono::tono_core::…`.

```rust
use bevy::prelude::*;
use bevy_tono::{Sound, TonoAudio, TonoPlugin};
use bevy_tono::tono_core::dsl::SoundDoc;

#[derive(Resource)]
struct Blip(Sound);

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(TonoPlugin)          // sets up audio + inserts TonoAudio
        .add_systems(Startup, setup)
        .add_systems(Update, on_jump)
        .run();
}

fn setup(mut commands: Commands, audio: Res<TonoAudio>) {
    let doc: SoundDoc = serde_json::from_str(/* a SoundDoc */).unwrap();
    let blip = audio.register(&doc);      // load once
    commands.insert_resource(Blip(blip));

    audio.music_layer(&pad, 0.0);         // always-on stem
    audio.music_layer(&shimmer, 0.6);     // swells in as intensity rises
}

fn on_jump(keys: Res<ButtonInput<KeyCode>>, audio: Res<TonoAudio>, blip: Res<Blip>) {
    if keys.just_pressed(KeyCode::Space) {
        audio.play(blip.0);               // one-shot SFX
    }
    // audio.set_intensity(threat_level); // reactive music
}
```

Run the demo: `cargo run --example play`.

## What you get

`TonoAudio` (a Bevy `Resource`):

| method | what it does |
| --- | --- |
| `register(&doc) -> Sound` | load a `SoundDoc` once (resampled to the device rate) |
| `play(sound) -> Voice` | fire a one-shot; the engine reclaims it when done (the first play renders, later plays reuse it) |
| `play_looping(sound)` / `stop(voice)` / `set_gain(voice, g)` | looping voices + control |
| `music_layer(&doc, fade_in_at)` | add an intensity-gated music stem |
| `set_intensity(0..1)` | cross-fade the music bed with the action |
| `stinger(&doc)` | fire a one-shot over the music |
| `render_stinger(&doc) -> Stinger` / `play_stinger_at(&stinger, q)` | render a stinger on any thread, then fire it — `Quantize::AtBeat(n)` lands it on a beat you computed first |
| `set_master_gain(0..1)` | global volume — scales the whole bus (SFX + music), ramped |
| `play_with(sound, Play)` | a voice with gain, pan, rate, lowpass, priority, looping |
| `register_with(&doc, Variation)` / `prewarm(sound)` | per-play variation; render off the game thread |
| `set_max_voices(n)` | voice cap with `Priority` stealing |
| `set_levels(voice, [l, r], ramp)` / `set_music_gain(g, ramp)` | slewed bed levels; a music fader |
| `transition_to_section_with(s, q, SectionEntry)` | resume a section, or count bars from its entry |
| `play_song(&Song)` | compile a catalog [`Song`] and bed it as looping music |
| `preset(name) -> InstrumentId` | register a live, playable instrument from a factory preset |
| `note_on(inst, note, vel)` / `note_off(inst, note)` / `set_bend(inst, semis)` | play it note-by-note |

Or make sound by **message** — a system that never touches the audio resource:

```rust
fn shoot(mut sfx: MessageWriter<PlaySfx>, laser: Res<Laser>) {
    sfx.write(PlaySfx::new(laser.0).with_gain(0.8));
}

fn play_note(mut on: MessageWriter<PlayNote>, synth: Res<Synth>) {
    on.write(PlayNote::new(synth.0, Note::C4).with_velocity(0.9)); // + StopNote to release
}
```

`TonoPlugin` registers `PlaySfx` / `PlayNote` / `StopNote` and drains them each
frame. Author a whole song with the catalog + `Song` builder and hand it to
`play_song`, or play an instrument live — see `cargo run --example keys`.

The audio callback owns the mixer: `TonoAudio` sends it commands down a
lock-free queue and reads the music clock, section and pause state back from
atomics, so a game-thread call never blocks or drops output. Every render
happens on the calling thread before the command is sent, so a long one-shot
or stinger never gaps the music. A render still costs the calling thread its
time: `prewarm` a sound, or call `render_stinger` on a worker (`TonoAudio` is
`Clone`). f32, i16 and u16 devices are supported. With no audio device the
mixer drains in real time into silence, so beats and quantized changes still
work headless.

## Using it in a game

Options go in a `TonoConfig` resource inserted before the plugin; both are off
by default:

```rust
app.insert_resource(TonoConfig {
    limiter: Some(Limiter::default()),   // a peak limiter on the final mix
    pause_music_on_focus_loss: true,     // hold the music while unfocused
})
.add_plugins(TonoPlugin);
```

Per-play controls, variation and a voice cap:

```rust
let step = audio.register_with(&doc, Variation { pitch: 0.03, pan: 0.2, level: 0.1 });
audio.prewarm(step);                     // render on a worker, not on first play
audio.set_max_voices(24);                // the engine's cap: lowest priority, oldest, goes first

audio.play_with(step, Play {
    rate: 1.1,                           // pitch and length together (a resample)
    pan: -0.4,                           // the engine's balance law
    lowpass_hz: Some(2_500.0),           // a one-pole lowpass on this voice
    priority: Priority::HIGH,
    ..Play::default()                    // Play::default() is exactly `play`
});
sfx.write(PlaySfx::new(step).with_rate(0.9).with_priority(Priority::LOW));
```

Beds, music gain and sections:

```rust
let wind = audio.play_looping(wind_sound);
audio.set_levels(wind, [0.2, 0.6], Duration::from_millis(80)); // left, right, slew
// Beds are voices: music_pause and music_reset leave them playing.

audio.set_music_gain(0.5, Duration::from_millis(300));          // a music fader
audio.transition_to_section_with(cave, Quantize::Bars(4), SectionEntry {
    resume: true,      // pick the section up where it left off
    from_entry: true,  // count the 4 bars from when the sounding section entered
});
```

`set_master_gain` ramps over 10 ms rather than stepping.

Sounds are authored as `SoundDoc` JSON — see the
[tono cookbook](https://github.com/marmikshah/tono/blob/master/docs/cookbook.md)
for the vocabulary.

## Contributing

`make` is the entry point; `make help` lists the targets.

```sh
make test                 # unit tests + doctests; no audio device needed
make pre-commit-checks    # cargo fmt --check + clippy -D warnings (what CI runs)
make hooks                # run those before each commit and push
make run                  # the play example: SPACE blips, the music breathes
cargo run --example keys  # A–K play a live instrument
```

On Linux, Bevy's examples need `libasound2-dev libudev-dev libwayland-dev
libxkbcommon-dev`.

`src/lib.rs` holds the public API: `TonoPlugin`, `TonoConfig`, `TonoAudio`,
`Play`, the messages and the `review` module. `src/mixer.rs` is the mixer the
callback owns (the `Command`s it applies, the sample player for resampled
voices, the limiter), `src/sections.rs` the section player, and
`src/output.rs` the cpal thread and the headless drain. `src/tests.rs` drives a
mixer directly, with no device or thread, including a copy of the 0.6 bus the
mix is checked against sample for sample. The demos are in `examples/`.

To work on tono-core alongside the plugin, point cargo at a local checkout,
and don't commit it (a release refuses a `[patch]`):

```toml
[patch.crates-io]
tono-core = { path = "../tono/crates/tono-core" }
```

**Releasing.** Bump `version` in `Cargo.toml` and date its
`## X.Y.Z — YYYY-MM-DD` section in `CHANGELOG.md` through a PR. Once it is on
`master`, run **Actions → Release → Run workflow**. The workflow checks the
version is new and the changelog dated, reruns CI, then publishes to
crates.io and creates the `vX.Y.Z` tag and GitHub Release from that
changelog section. A failed run can be re-run; finished steps are skipped.

## License

Released under the [MIT License](LICENSE).
