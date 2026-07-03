//! A playable instrument: your computer keyboard becomes a one-octave keyboard.
//!
//!     cargo run --example keys
//!
//! Press A S D F G H J K (the white keys, C4..C5) — each holds its note until
//! you let go, polyphonically. Input fires `PlayNote` / `StopNote` messages, so
//! this game system never touches the audio resource.

use bevy::prelude::*;
use bevy_tono::{InstrumentId, Note, PlayNote, StopNote, TonoAudio, TonoPlugin};

#[derive(Resource)]
struct Keyboard(InstrumentId);

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(TonoPlugin)
        .add_systems(Startup, setup)
        .add_systems(Update, play_keys)
        .run();
}

fn setup(mut commands: Commands, audio: Res<TonoAudio>) {
    // Any factory preset — try "fm_tine", "sub_bass", "supersaw_pad", …
    let inst = audio.preset("vibrato_lead").expect("the preset exists");
    commands.insert_resource(Keyboard(inst));
    println!("bevy_tono keys — play A S D F G H J K (C4..C5).");
}

/// White keys across one octave: (keyboard key, MIDI note).
const KEYS: [(KeyCode, u8); 8] = [
    (KeyCode::KeyA, 60), // C4
    (KeyCode::KeyS, 62), // D4
    (KeyCode::KeyD, 64), // E4
    (KeyCode::KeyF, 65), // F4
    (KeyCode::KeyG, 67), // G4
    (KeyCode::KeyH, 69), // A4
    (KeyCode::KeyJ, 71), // B4
    (KeyCode::KeyK, 72), // C5
];

fn play_keys(
    keys: Res<ButtonInput<KeyCode>>,
    kb: Res<Keyboard>,
    mut on: MessageWriter<PlayNote>,
    mut off: MessageWriter<StopNote>,
) {
    for (key, midi) in KEYS {
        if keys.just_pressed(key) {
            on.write(PlayNote::new(kb.0, Note(midi)));
        }
        if keys.just_released(key) {
            off.write(StopNote {
                instrument: kb.0,
                note: Note(midi),
            });
        }
    }
}
