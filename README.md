# clipclip

[![Crates.io](https://img.shields.io/crates/v/clipclip.svg)](https://crates.io/crates/clipclip)
[![Docs.rs](https://docs.rs/clipclip/badge.svg)](https://docs.rs/clipclip)
[![CI](https://github.com/sincekmori/clipclip/actions/workflows/ci.yml/badge.svg)](https://github.com/sincekmori/clipclip/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/clipclip.svg)](https://crates.io/crates/clipclip)

Continuously capture audio and hand fixed-length **Opus**/**WAV** segments to your code.

`clipclip` records the **microphone**, **system audio (loopback)**, or **both mixed**, and — while recording without interruption — cuts the stream into segments of a configurable length (default 30s), encodes each one, and calls your handler with the encoded bytes.
What you do next is up to you: transcribe with Whisper, send to an LLM, upload, save to disk… `clipclip` itself never touches the filesystem.

Audio is captured with [`cpal`](https://docs.rs/cpal), resampled to 16 kHz mono by default (ideal for ASR), and can optionally drop silent / no-speech segments before they reach your handler.

## Quick start

```rust,no_run
use clipclip::{start, Config, Source};
use std::time::Duration;

let recording = start(
    Config {
        source: Source::Mic,
        segment: Duration::from_secs(30),
        ..Config::default()
    },
    |segment| {
        // Hand the encoded bytes downstream (Whisper, an LLM, upload, save…).
        println!(
            "segment #{}: {} bytes (.{})",
            segment.index,
            segment.data.len(),
            segment.extension(),
        );
    },
)?;

// Hold `recording` to keep going; drop it (or call .stop()) to stop and flush
// the final partial segment to your handler. `stop()` returns `Err` if a
// capture device faulted mid-recording (e.g. the mic was unplugged).
std::thread::sleep(Duration::from_secs(120));
recording.stop()?;
# Ok::<(), clipclip::Error>(())
```

The handler runs on a dedicated worker thread, so it may block on slow work.

## What you get

Each `Segment` carries the encoded bytes plus metadata:

```text
pub struct Segment {
    pub index: u64,        // 1-based sequence number
    pub data: Vec<u8>,     // a complete, standalone .opus or .wav file
    pub format: Format,
    pub sample_rate: u32,
    pub channels: u16,     // 1 (mono)
    pub frames: usize,     // samples per channel
    pub duration: Duration,
    pub offset: Duration,  // time from the start of recording
    pub is_final: bool,    // the flushed tail at stop
}
```

## Configuration

```rust,ignore
use clipclip::{Config, Source, Format, Activity};
use std::time::Duration;

let cfg = Config {
    source: Source::Both,                 // Mic | System | Both
    segment: Duration::from_secs(15),     // any length; default 30s
    format: Format::Wav,                  // Format::Opus (default) | Format::Wav
    activity: Activity::silero(),          // KeepAll (default) | energy() | silero()
    sample_rate: 16_000,                  // mono; default 16 kHz
    mic_gain: 1.0,
    system_gain: 1.0,
    opus_bitrate: 24_000,
    min_final_segment: Duration::from_secs(3), // drop the stop-time tail if shorter (default: ZERO = keep all)
};
```

- **Activity filtering** (drop segments before they reach your handler):
  - `Activity::KeepAll` *(default)* — hand off every segment.
  - `Activity::energy()` — dependency-free RMS gate (drops silence; lets steady noise through).
  - `Activity::silero()` — Silero V5 speech detection (requires the `silero` feature; 16 kHz only).
- **Short-tail trimming**: the leftover partial segment at stop is dropped when it is shorter than `min_final_segment`.
  The default `Duration::ZERO` keeps every tail (matching `Activity::KeepAll`); set e.g. `Duration::from_secs(3)` to drop a tiny trailing clip.
  Only the final segment is ever affected — full segments always pass through.
- **Device faults**: when the audio backend reports a stream error (e.g. a device invalidated on Windows/macOS), recording stops and flushes the tail.
  `recording.is_running()` then returns `false`, and `recording.stop()` returns `Err(Error::DeviceLost(..))` so you can tell a fault apart from a clean stop.
  Note: Linux/PulseAudio transparently reroutes an unplugged device to a fallback input, so a mic unplug usually does **not** surface as a fault there — recording continues from the fallback.
- **Live control** while recording, via the `Recording` handle:
  - `recording.set_source(Source::Both)` — add/remove sources on the fly.
  - `recording.set_gains(mic, system)` — adjust levels live.

## Cargo features

| Feature | Default | What it adds | Build needs |
|---|---|---|---|
| `opus`   | ✅ | Ogg Opus output | **CMake + a C compiler** (vendored libopus) |
| `silero` | — | Silero V5 neural VAD | downloads an ONNX runtime at build time |

WAV output and the energy gate are always available and pure-Rust.
To drop the native Opus dependency entirely:

```toml
clipclip = { version = "0.1", default-features = false }   # WAV only, no CMake
```

## Platform notes

- **Microphone**: default input device on all platforms.
- **System audio (loopback)**:
  - **Windows** — WASAPI loopback (no permission needed).
  - **macOS** — Core Audio process tap (macOS 14.2+); needs **Screen Recording** permission.
    The mic needs **Microphone** permission.
  - **Linux** — captured from the default output's PulseAudio/PipeWire **monitor** source (cpal's PulseAudio backend, enabled automatically).
    Needs a running PulseAudio / `pipewire-pulse` server.
- **Linux build**: install ALSA dev headers (`libasound2-dev`).

### Opus on Linux

On Linux the `opus` feature **requires the system GNU `ld` (bfd) linker**.
The vendored libopus/libopusenc archives are compiled with GCC LTO, and Rust's default `rust-lld` linker can't resolve their `ope_*` / `opus_*` symbols (it has no GCC LTO plugin) — so linking fails under rust-lld even though the archives are linked `+whole-archive`.
Add this to your project's `.cargo/config.toml` (this repo already carries it for its own builds, but a dependency's config does not propagate, so your project needs its own):

```toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "linker-features=-lld"]
```

Windows and macOS link out of the box, including from projects that just depend on clipclip.
WAV-only builds — `default-features = false` — have no native dependency and are unaffected on every platform.

## Example

```sh
# Save segments to ./clipclip-out/ for 60s (downstream stub):
cargo run --example save_to_disk -- 60 mic
cargo run --example save_to_disk -- 30 system wav
```
