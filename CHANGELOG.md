# Changelog

Notable changes to `bevy_tono`, newest first. Follows [SemVer](https://semver.org);
format loosely [Keep a Changelog](https://keepachangelog.com). Entries begin
from the point this file was added — earlier releases live in the crates.io
version history and the git log.

## Unreleased

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
