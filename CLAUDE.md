# CLAUDE.md — bevy_tono

Commands, layout and releasing are in the README's Contributing section. These
are the rules an edit must not break:

- The audio callback owns the mixer (`src/mixer.rs`). The game side only
  sends `Command`s down the queue and reads atomics back, so a game-thread
  call never blocks or drops output. Nothing renders in the mixer: every
  render (one-shots, stingers, stems, sections) happens before the send. A
  missing audio device degrades to a real-time silent drain, never a startup
  failure.
- Determinism is tono's contract: the same `SoundDoc` sounds identical every
  run. Don't add nondeterminism (wall-clock, unseeded RNG) between the doc and
  the bus; `Variation` draws from a seeded generator.
- The lib takes the minimal bevy surface (`default-features = false`, plus
  `bevy_window` for focus messages) so the game picks its own features; full
  `bevy` stays in `[dev-dependencies]`.
- Never commit a `[patch]` section: `cargo publish` ignores it, so the crate
  would ship untested, and `release.yml` refuses it.
- Open source: keep examples/docs free of any personal or company identifiers.
