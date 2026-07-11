# bevy_tono

A [Bevy](https://bevyengine.org) plugin for the [tono](https://github.com/marmikshah/tono)
sound engine — fire deterministic SFX and drive adaptive music straight from
your game systems. No cpal/threads to wire up yourself.

```toml
[dependencies]
bevy_tono = "0.1"
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
| `play(sound) -> Voice` | fire a one-shot; the engine reclaims it when done |
| `play_looping(sound)` / `stop(voice)` / `set_gain(voice, g)` | looping voices + control |
| `music_layer(&doc, fade_in_at)` | add an intensity-gated music stem |
| `set_intensity(0..1)` | cross-fade the music bed with the action |
| `stinger(&doc)` | fire a one-shot over the music |
| `set_master_gain(0..1)` | global volume — scales the whole bus (SFX + music) |
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

Audio runs on a dedicated thread that owns the `cpal` stream; the callback only
`try_lock`s, so a game-thread poke never blocks or clicks the output. With no
audio device the plugin degrades to silence rather than failing to start.

Sounds are authored as `SoundDoc` JSON — see the
[tono cookbook](https://github.com/marmikshah/tono/blob/master/docs/cookbook.md)
for the vocabulary.

## License

Released under the [MIT License](LICENSE).
