//! A tiny game: press SPACE to fire a blip; the music bed swells and ebbs.
//!
//!     cargo run --example play
//!
//! Shows the whole surface — register a sound once, fire it on input with a
//! `PlaySfx` message (no audio resource needed in the game system), and drive an
//! adaptive music bed with `set_intensity`.

use bevy::prelude::*;
use bevy_tono::tono_core::dsl::SoundDoc;
use bevy_tono::{PlaySfx, Sound, TonoAudio, TonoPlugin};

#[derive(Resource)]
struct Blip(Sound);

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(TonoPlugin)
        .add_systems(Startup, setup)
        .add_systems(Update, (play_on_space, breathe_music))
        .run();
}

fn doc(json: &str) -> SoundDoc {
    serde_json::from_str(json).expect("valid SoundDoc")
}

fn setup(mut commands: Commands, audio: Res<TonoAudio>) {
    // A one-shot SFX.
    let blip = audio.register(&doc(
        r#"{ "name":"blip", "duration":0.25, "engine":2, "root": { "type":"mul", "inputs": [
            { "type":"fm", "freq":660, "ratio":2.0, "index":4 },
            { "type":"env", "a":0.002, "d":0.18, "s":0.0, "r":0.05 } ] } }"#,
    ));
    commands.insert_resource(Blip(blip));

    // A two-layer adaptive bed: a pad always underneath, a brighter layer that
    // swells in as intensity rises.
    audio.music_layer(
        &doc(
            r#"{ "name":"pad", "duration":2.0, "engine":2, "root": { "type":"chain", "stages": [
            { "type":"sawtooth", "freq":110 },
            { "type":"lowpass", "cutoff":900, "q":0.5 } ] } }"#,
        ),
        0.0,
    );
    audio.music_layer(
        &doc(
            r#"{ "name":"shimmer", "duration":2.0, "engine":2, "root": { "type":"chain", "stages": [
            { "type":"super", "wave":"sawtooth", "freq":440, "voices":5, "detune_cents":18 },
            { "type":"lowpass", "cutoff":3000, "q":0.4 } ] } }"#,
        ),
        0.55,
    );

    println!("bevy_tono example — press SPACE to blip; the music breathes on its own.");
}

fn play_on_space(
    keys: Res<ButtonInput<KeyCode>>,
    mut sfx: MessageWriter<PlaySfx>,
    blip: Res<Blip>,
) {
    if keys.just_pressed(KeyCode::Space) {
        // Fire the sound by message — this system never touches the audio bus.
        sfx.write(PlaySfx::new(blip.0));
    }
}

/// Oscillate the intensity so you can hear the shimmer layer fade in and out.
fn breathe_music(time: Res<Time>, audio: Res<TonoAudio>) {
    let x = 0.5 + 0.5 * (time.elapsed_secs() * 0.25).sin();
    audio.set_intensity(x);
}
