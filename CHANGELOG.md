# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.6](https://github.com/sincekmori/clipclip/compare/v0.2.5...v0.2.6) - 2026-09-04

### Fixed

- use as_chunks instead of chunks_exact in Silero VAD to satisfy clippy on Rust 1.98

### Other

- Merge branch 'main' into dependabot/github_actions/actions-d82e01f320
- *(deps)* update rubato requirement in the cargo group

## [0.2.5](https://github.com/sincekmori/clipclip/compare/v0.2.4...v0.2.5) - 2026-08-07

### Other

- *(deps)* bump crate-ci/typos in the actions group

## [0.2.4](https://github.com/sincekmori/clipclip/compare/v0.2.3...v0.2.4) - 2026-08-04

### Fixed

- remove sub_chunks argument from Fft::new for rubato 4.0

### Other

- *(deps)* update rubato requirement in the cargo group

## [0.2.3](https://github.com/sincekmori/clipclip/compare/v0.2.2...v0.2.3) - 2026-07-08

### Other

- Merge pull request #5 from sincekmori/dependabot/github_actions/actions-6775651c8a

## [0.2.2](https://github.com/sincekmori/clipclip/compare/v0.2.1...v0.2.2) - 2026-07-06

### Added

- live frame tap alongside the segment pipeline

## [0.2.1](https://github.com/sincekmori/clipclip/compare/v0.2.0...v0.2.1) - 2026-06-25

### Other

- fix rust-cache key to actually include the runner image version

## [0.2.0](https://github.com/sincekmori/clipclip/compare/v0.1.1...v0.2.0) - 2026-06-25

### Added

- [**breaking**] separate mic/system tracks and ISO 8601 UTC segment timestamps

### Other

- key rust-cache on runner image version

## [0.1.1](https://github.com/sincekmori/clipclip/compare/v0.1.0...v0.1.1) - 2026-06-25

### Fixed

- *(deps)* migrate to rubato 3.0

### Other

- allow CDLA-Permissive-2.0 in cargo-deny
