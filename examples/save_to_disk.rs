//! Example downstream stub: write each captured segment to `./clipclip-out/`.
//!
//! Saving is NOT part of clipclip — this just shows how a handler consumes the
//! segments it's handed. A real downstream might transcribe with Whisper, send
//! the bytes to an LLM, upload them, etc.
//!
//! Usage:
//!   cargo run --example save_to_disk -- [seconds] [mic|system|both] [opus|wav]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clipclip::{start, Config, Source};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let source = match args.get(2).map(String::as_str) {
        Some("system") => Source::System,
        Some("both") => Source::Both,
        _ => Source::Mic,
    };

    let mut config = Config {
        source,
        segment: Duration::from_secs(10), // short segments so the demo is lively
        ..Config::default()
    };
    if args.get(3).map(String::as_str) == Some("wav") {
        config.format = clipclip::Format::Wav;
    }

    let out = PathBuf::from("clipclip-out");
    fs::create_dir_all(&out).expect("create output dir");
    println!(
        "recording {secs}s ({:?}, {:?}, {}s segments) -> {}/",
        config.source,
        config.format,
        config.segment.as_secs(),
        out.display()
    );

    let recording = start(config, move |segment| {
        let path = out.join(format!(
            "segment_{:04}.{}",
            segment.index,
            segment.extension()
        ));
        match fs::write(&path, &segment.data) {
            Ok(()) => println!(
                "saved {} ({} bytes, {:.1}s{})",
                path.display(),
                segment.data.len(),
                segment.duration.as_secs_f32(),
                if segment.is_final { ", final" } else { "" },
            ),
            Err(e) => eprintln!("failed to write {}: {e}", path.display()),
        }
    })
    .expect("failed to start recording");

    std::thread::sleep(Duration::from_secs(secs));
    match recording.stop() {
        Ok(()) => println!("done."),
        Err(e) => eprintln!("recording stopped due to error: {e}"),
    }
}
