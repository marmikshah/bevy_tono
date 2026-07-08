# CLAUDE.md — bevy_tono

Agent onboarding. `make` is the entry point; keep this short and current.

## What this is

A Bevy plugin for the [tono](https://github.com/marmikshah/tono) sound engine:
add `TonoPlugin`, reach for the `TonoAudio` resource, and fire deterministic SFX
or drive an adaptive music bed straight from your game systems. It owns the
`cpal` output stream on a dedicated thread so the audio never blocks the game
loop. No retained mixer graph exposed — sounds are authored as `SoundDoc` JSON
and played by handle.

## Entry point

**Everything is a `make` target — never run ad-hoc scripts.** `make help` lists them.

| target | use |
|--------|-----|
| `make run` | run the `play` example (SPACE blips; the music breathes) |
| `make test` | test suite |
| `make pre-commit-checks` | `cargo fmt --check` + clippy `-D warnings` (what the hooks run) |
| `make hooks` | install the git hooks |
| `make release` | tag a clean `master` → CI publishes to crates.io |
| `make clean` | wipe build artifacts |

## Architecture

- `src/lib.rs` — the whole surface:
  - `TonoPlugin` — spawns the audio thread, inserts `TonoAudio`, registers the
    `PlaySfx`/`PlayNote`/`StopNote` messages + their draining systems.
  - `TonoAudio` (a Bevy `Resource`) — `register`/`play`/`play_looping`/`stop`/
    `set_gain` for SFX, `music_layer`/`set_intensity`/`stinger`/`play_song` for
    the bed, `preset`/`add_instrument`/`note_on`/`note_off`/`set_bend` for live
    instruments, and `set_master_gain` for global volume.
  - `PlaySfx` / `PlayNote` / `StopNote` (`Message`s) — make sound from a system
    without the resource; `play_queued_sfx` / `drain_notes` drain each `Update`.
  - `Sound` (a `PatchId`), `Voice` (an `InstanceHandle`), `InstrumentId` (an
    index into the bus's live `Instrument`s). `GameBus` mixes engine + music +
    instruments.
  - `GameBus` — an `AudioSource` summing the SFX `Engine` and the
    `AdaptiveMusic` bed to stereo.
  - `spawn_audio`/`build_stream`/`fill_output` — the dedicated `cpal` thread.
- `examples/play.rs` — a windowed demo of the whole surface.

## Hard constraints

- The audio callback only ever `try_lock`s the bus — a game-thread poke must
  never block or click the output. A missing audio device degrades to silence,
  never a startup failure.
- Determinism is tono's contract: the same `SoundDoc` sounds identical every
  run. Don't add nondeterminism (wall-clock, RNG) between the doc and the bus.
- Open source: keep examples/docs free of any personal or company identifiers.

## Dependency on tono-core

`tono-core` is a **crates.io dependency** (`tono-core = "1.5"`), so the crate is
standalone — it clones, builds, publishes, and CI-tests without the `tono`
workspace checked out beside it. To hack on the engine and the plugin together
locally, add a `[patch.crates-io]` override pointing `tono-core` at a local
checkout — no source change needed.

## Dev notes

- The lib pulls the minimal bevy surface (`default-features = false`); the
  consuming game brings its own bevy and cargo unifies. The example needs a real
  windowed app, so full `bevy` lives in `[dev-dependencies]`.
- Tests exercise `GameBus` directly (no audio device, no window).
