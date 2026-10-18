# CLAUDE.md — bevy_tono

Commands, layout and releasing are in the README's Contributing section. These
are the rules an edit must not break:

- The audio callback only ever `try_lock`s the bus — a game-thread poke must
  never block or click the output, so nothing renders while holding the bus
  lock (`Engine::prepare`/`spawn_prepared`, `Stinger`, `LoopBuffer` all render
  first). A missing audio device degrades to silence, never a startup failure.
- Determinism is tono's contract: the same `SoundDoc` sounds identical every
  run. Don't add nondeterminism (wall-clock, RNG) between the doc and the bus.
- The lib takes the minimal bevy surface (`default-features = false`) so the
  game picks its own features; full `bevy` stays in `[dev-dependencies]`.
- Never commit a `[patch]` section: `cargo publish` ignores it, so the crate
  would ship untested, and `release.yml` refuses it.
- Open source: keep examples/docs free of any personal or company identifiers.
