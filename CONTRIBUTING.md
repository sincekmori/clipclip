# Contributing

Thanks for your interest in contributing to `clipclip`!

## Supported platforms

Linux, Windows, and macOS.

## Prerequisites

- A recent stable Rust toolchain.
- The default `opus` feature builds vendored libopus, so you need **CMake** and a **C compiler** (MSVC on Windows, Xcode Command Line Tools on macOS, `build-essential` on Linux).
  Building WAV-only — `--no-default-features` — needs neither.
- **Linux**: ALSA dev headers (`sudo apt-get install libasound2-dev`).
  The `opus` feature needs the system GNU `ld` (bfd) — this repo's `.cargo/config.toml` sets `linker-features=-lld`, because rust-lld can't link the LTO-compiled vendored archives.
  System-audio capture needs a running PulseAudio / `pipewire-pulse` server.
- **macOS**: the example needs Microphone and (for system audio) Screen Recording permission; see the README.

## Before opening a PR

Please make sure the checks CI runs pass locally:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --no-default-features          # WAV only, no native codec deps
cargo deny check                           # cargo install cargo-deny
typos                                      # cargo install typos-cli
```

## Commit messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, `refactor:`, `chore:`, `ci:`, …).
release-plz derives the changelog and version bumps from them, so please follow the convention.

## Releases

Releases are automated by release-plz.
Merging its "release" PR publishes to crates.io and tags the version, so you don't need to bump versions or edit the changelog by hand.
