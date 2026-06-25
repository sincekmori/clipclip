//! cpal capture for microphone and system audio (loopback).
//!
//! The data callback runs on a realtime audio thread, so it does the *minimum*:
//! convert each frame to a single mono `f32` and push into the SPSC ring buffer.
//! No allocation in steady state (a scratch buffer is reused), no locks, no I/O.
//! Resampling and everything heavier happens on the worker/consumer thread.
//!
//! ## System audio per OS
//! * **Windows** – open the default *output* device and build an *input* stream
//!   on it; cpal sets the WASAPI loopback flag automatically. No special perms.
//! * **macOS** – same output-device + input-stream pattern (cpal Core Audio
//!   process tap, macOS 14.2+). Requires Screen Recording permission (and
//!   Microphone permission for the mic).
//! * **Linux** – capture from a PulseAudio/PipeWire *monitor* source, which shows
//!   up as a normal input device whose name contains "monitor".

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use ringbuf::traits::Producer;

use crate::error::{Error, Result};

/// Shared slot holding the first stream-error message, if any source faults
/// mid-recording. Set from cpal's (rare) error callback, polled by the worker.
pub(crate) type Fault = Arc<Mutex<Option<String>>>;

/// A live capture stream plus its native sample rate (needed to build the
/// matching resampler). `cpal::Stream` is `!Send`, so this lives on the worker
/// thread that created it and stops capture when dropped.
pub(crate) struct CaptureHandle {
    // Held only to keep capture alive; dropping it stops the stream.
    #[allow(dead_code)]
    stream: cpal::Stream,
    pub sample_rate: u32,
}

/// Open the default microphone. `fault` is set if the stream later faults.
pub(crate) fn open_mic<P>(
    producer: P,
    overruns: Arc<AtomicU64>,
    fault: Fault,
) -> Result<CaptureHandle>
where
    P: Producer<Item = f32> + Send + 'static,
{
    let host = cpal::default_host();
    // NOTE (macOS): if a system-audio tap is ever exposed as an input device,
    // it should be excluded here so the mic stream doesn't capture loopback.
    let device = host
        .default_input_device()
        .ok_or(Error::NoDevice("microphone"))?;
    let supported = device
        .default_input_config()
        .map_err(|e| Error::Device(format!("mic config: {e}")))?;
    build_stream(&device, supported, producer, overruns, fault, "mic")
}

/// Open system / speaker audio (loopback) — OS-specific device selection.
pub(crate) fn open_system<P>(
    producer: P,
    overruns: Arc<AtomicU64>,
    fault: Fault,
) -> Result<CaptureHandle>
where
    P: Producer<Item = f32> + Send + 'static,
{
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        // Build an INPUT stream on the default OUTPUT device → loopback.
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(Error::NoDevice("system audio (output device)"))?;
        let supported = device
            .default_output_config()
            .map_err(|e| Error::Device(format!("output config: {e}")))?;
        build_stream(&device, supported, producer, overruns, fault, "system")
    }

    #[cfg(target_os = "linux")]
    {
        // cpal's default Linux host (ALSA) does NOT expose PipeWire/PulseAudio
        // monitor sources, so use the PulseAudio host (pure-Rust client over the
        // PA / pipewire-pulse socket) and capture the output's monitor source.
        let host = cpal::host_from_id(cpal::HostId::PulseAudio)
            .map_err(|e| Error::Device(format!("PulseAudio host unavailable: {e}")))?;
        let device = host
            .input_devices()
            .map_err(|e| Error::Device(e.to_string()))?
            .find(|d| d.to_string().to_lowercase().contains("monitor"))
            .ok_or(Error::NoDevice(
                "system audio (PulseAudio/PipeWire monitor source)",
            ))?;
        let supported = device
            .default_input_config()
            .map_err(|e| Error::Device(format!("monitor config: {e}")))?;
        build_stream(&device, supported, producer, overruns, fault, "system")
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = (producer, overruns, fault);
        Err(Error::NoDevice("system audio (unsupported platform)"))
    }
}

fn build_stream<P>(
    device: &cpal::Device,
    supported: cpal::SupportedStreamConfig,
    producer: P,
    overruns: Arc<AtomicU64>,
    fault: Fault,
    label: &'static str,
) -> Result<CaptureHandle>
where
    P: Producer<Item = f32> + Send + 'static,
{
    let sample_format = supported.sample_format();
    let channels: u16 = supported.channels();
    let sample_rate: u32 = supported.sample_rate();
    let config: cpal::StreamConfig = supported.config();

    // cpal 0.18: Device implements Display for its name (no `name()` method).
    log::info!("[{label}] device='{device}' {sample_rate} Hz, {channels} ch, {sample_format:?}");

    let err_fn = move |e: cpal::Error| {
        // Record the first fault so the worker can stop gracefully and report it
        // to the user, rather than silently going dead (e.g. device unplugged).
        // This is the error callback (rare), not the realtime data path, so a
        // brief lock here is fine.
        if let Ok(mut slot) = fault.lock() {
            slot.get_or_insert_with(|| format!("[{label}] {e}"));
        }
        log::error!("[{label}] stream error: {e}");
    };
    let ch = channels.max(1) as usize;

    let stream = match sample_format {
        SampleFormat::F32 => build_typed::<f32, P>(device, config, producer, ch, overruns, err_fn),
        SampleFormat::I16 => build_typed::<i16, P>(device, config, producer, ch, overruns, err_fn),
        SampleFormat::U16 => build_typed::<u16, P>(device, config, producer, ch, overruns, err_fn),
        SampleFormat::I32 => build_typed::<i32, P>(device, config, producer, ch, overruns, err_fn),
        SampleFormat::F64 => build_typed::<f64, P>(device, config, producer, ch, overruns, err_fn),
        other => {
            return Err(Error::Stream(format!(
                "unsupported sample format {other:?}"
            )))
        }
    }?;

    stream
        .play()
        .map_err(|e| Error::Stream(format!("play {label}: {e}")))?;

    Ok(CaptureHandle {
        stream,
        sample_rate,
    })
}

fn build_typed<T, P>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut producer: P,
    channels: usize,
    overruns: Arc<AtomicU64>,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
    P: Producer<Item = f32> + Send + 'static,
{
    // Reused across callbacks; grows once then stays put (RAM stays flat).
    let mut scratch: Vec<f32> = Vec::with_capacity(8192);

    let data_fn = move |data: &[T], _: &cpal::InputCallbackInfo| {
        scratch.clear();
        // Downmix each interleaved frame to one mono f32 sample.
        for frame in data.chunks(channels) {
            let mut acc = 0.0f32;
            for &s in frame {
                // cpal/dasp conversion trait method is `from_sample_`.
                acc += f32::from_sample_(s);
            }
            scratch.push(acc / channels as f32);
        }
        let pushed = producer.push_slice(&scratch);
        if pushed < scratch.len() {
            // Consumer fell behind: drop the overflow (bounded RAM) and count it.
            overruns.fetch_add((scratch.len() - pushed) as u64, Ordering::Relaxed);
        }
    };

    device
        .build_input_stream::<T, _, _>(config, data_fn, err_fn, None)
        .map_err(|e| Error::Stream(e.to_string()))
}
