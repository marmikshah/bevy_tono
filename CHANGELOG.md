# Changelog

Notable changes to `bevy_tono`, newest first. Follows [SemVer](https://semver.org);
format loosely [Keep a Changelog](https://keepachangelog.com). Entries begin
from the point this file was added — earlier releases live in the crates.io
version history and the git log.

## 0.5.0 — 2026-10-15

### Changed
- **tono-core 1.10 → 1.11** — the engine's trimmed 1.11 line: one DSP
  evaluator shared by the bounce and streaming, modulated filters that stream,
  stereo reverb tails on live buses and instrument masters, and a smaller node
  vocabulary. No bevy_tono API changes: the plugin builds, lints and tests
  clean against 1.11.0.
- Games that author SoundDocs should check the tono 1.11.0 changelog: the
  `wavetable`, `bitcrush`, `downsample`, `ringmod`, `flanger`, `phaser`,
  `duck`, `convolve` and `granular` node types were removed upstream, so a
  document using one now fails to parse with the type named.

## 0.4.0 — 2026-08-12

### Changed
- **tono-core 1.7 → 1.10** — the engine's 1.10 line: typed `Song` → `Program`
  compilation, engine revision 5 (cross-platform byte-identity), the streaming
  mixer, and the new catalog voices/nodes. No bevy_tono API changes — the
  1.7-era surface builds warning-free against 1.10.1.

## 0.3.1 — 2026-07-11

Review follow-ups on the 0.3.0 work; requires **tono-core 1.7**.

### Added
- `play_with_gain(sound, gain)` — a one-shot that starts at the requested level
  (via `Tween::IMMEDIATE`) instead of ramping up from unity, so a short/transient
  SFX honours its gain from the first sample; `PlaySfx` now routes through it.

### Fixed
- `add_section` / `stinger` / `stinger_at` render **off the bus lock** (via
  tono-core 1.7's buffer/stereo entry points), so a runtime section-add or
  stinger no longer drops an audio buffer against the cpal callback's `try_lock`.
- `review::grade` guards `sample_rate == 0` (→ NaN); `note_on`/`note_off`/
  `set_bend` `debug_assert` on an unknown instrument id instead of silently
  no-op'ing.
