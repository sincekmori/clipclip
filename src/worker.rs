//! The single worker/consumer thread: drains the capture ring buffer(s),
//! resamples to the target rate, mixes (in `Both` mode), cuts fixed-length
//! segments, runs the activity filter, encodes, and hands each kept segment to
//! the user's handler.
//!
//! Memory is flat for the whole recording: fixed-capacity ring buffers + a small
//! set of reused scratch/segment buffers. Nothing accumulates.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ringbuf::traits::{Consumer, Split};
use ringbuf::{HeapCons, HeapRb};

use crate::capture::{open_mic, open_system, CaptureHandle, Fault};
use crate::config::{Config, Source};
use crate::encode;
use crate::error::{Error, Result};
use crate::resample::StreamResampler;
use crate::segment::Segment;
use crate::segmenter::Segmenter;
use crate::vad::{make_detector, ActivityDetector};

/// ~4 s of headroom at 48 kHz mono. Fixed at startup, never grows.
const RING_CAPACITY: usize = 48_000 * 4;
/// Samples popped from a ring buffer per step.
const POP_SCRATCH: usize = 4096;
/// Worker poll interval. Segment latency is seconds, so 20 ms is plenty.
const TICK: Duration = Duration::from_millis(20);
/// Upper bound for user gain (~ +18 dB).
const MAX_GAIN: f32 = 8.0;

/// Live control messages sent to the worker.
pub(crate) enum WorkerCommand {
    /// Switch the active source(s) on the fly.
    SetSource(Source),
    /// Set mic / system-audio linear gains.
    SetGains { mic: f32, system: f32 },
}

/// One capture source: stream (kept alive), ring-buffer consumer, resampler.
struct SourceState {
    handle: Option<CaptureHandle>,
    consumer: HeapCons<f32>,
    resampler: StreamResampler,
    pop_scratch: Vec<f32>,
}

impl SourceState {
    fn drain_into(&mut self, out: &mut Vec<f32>) {
        loop {
            let n = self.consumer.pop_slice(&mut self.pop_scratch);
            if n == 0 {
                break;
            }
            if let Err(e) = self.resampler.process(&self.pop_scratch[..n], out) {
                log::error!("resample error: {e}");
            }
            if n < self.pop_scratch.len() {
                break;
            }
        }
    }

    fn flush_into(&mut self, out: &mut Vec<f32>) {
        self.drain_into(out);
        if let Err(e) = self.resampler.flush(out) {
            log::error!("resample flush error: {e}");
        }
    }

    fn stop_stream(&mut self) {
        self.handle = None;
    }
}

fn open_source(
    is_mic: bool,
    target_rate: u32,
    overruns: Arc<AtomicU64>,
    fault: Fault,
) -> Result<SourceState> {
    let rb = HeapRb::<f32>::new(RING_CAPACITY);
    let (prod, cons) = rb.split();
    let handle = if is_mic {
        open_mic(prod, overruns, fault)?
    } else {
        open_system(prod, overruns, fault)?
    };
    let resampler = StreamResampler::new(handle.sample_rate, target_rate)?;
    Ok(SourceState {
        handle: Some(handle),
        consumer: cons,
        resampler,
        pop_scratch: vec![0.0f32; POP_SCRATCH],
    })
}

/// Mix `Both` mode: mic is the clock master; add the next system sample (zero if
/// behind), clamping. The system residual is bounded (`cap`) so drift can't grow
/// memory.
fn mix_into(mic: &[f32], sys: &[f32], sys_residual: &mut Vec<f32>, out: &mut Vec<f32>, cap: usize) {
    out.clear();
    sys_residual.extend_from_slice(sys);
    for (i, &m) in mic.iter().enumerate() {
        let s = sys_residual.get(i).copied().unwrap_or(0.0);
        out.push((m + s).clamp(-1.0, 1.0));
    }
    let consumed = mic.len().min(sys_residual.len());
    sys_residual.drain(0..consumed);
    if sys_residual.len() > cap {
        let excess = sys_residual.len() - cap;
        sys_residual.drain(0..excess);
        log::warn!("system audio ahead of mic; dropped {excess} samples to bound memory");
    }
}

/// Apply a linear gain in place (clamped). No-op for unity gain.
fn scale_in_place(buf: &mut [f32], gain: f32) {
    if (gain - 1.0).abs() > f32::EPSILON {
        for s in buf.iter_mut() {
            *s = (*s * gain).clamp(-1.0, 1.0);
        }
    }
}

/// Open/close sources so the live set matches `target`. New sources are opened
/// before old ones are closed, and the last remaining source is never dropped,
/// so a failed open can't leave the recording with nothing.
#[allow(clippy::too_many_arguments)]
fn apply_source(
    target: Source,
    target_rate: u32,
    mic_state: &mut Option<SourceState>,
    sys_state: &mut Option<SourceState>,
    mic_overruns: &Arc<AtomicU64>,
    sys_overruns: &Arc<AtomicU64>,
    fault: &Fault,
    sys_residual: &mut Vec<f32>,
) {
    if target.needs_mic() && mic_state.is_none() {
        match open_source(true, target_rate, mic_overruns.clone(), fault.clone()) {
            Ok(s) => {
                *mic_state = Some(s);
                log::info!("added microphone");
            }
            Err(e) => log::warn!("could not add microphone: {e}"),
        }
    }
    if target.needs_system() && sys_state.is_none() {
        match open_source(false, target_rate, sys_overruns.clone(), fault.clone()) {
            Ok(s) => {
                *sys_state = Some(s);
                sys_residual.clear();
                log::info!("added system audio");
            }
            Err(e) => log::warn!("could not add system audio: {e}"),
        }
    }
    if !target.needs_mic() && mic_state.is_some() && sys_state.is_some() {
        *mic_state = None;
        log::info!("removed microphone");
    }
    if !target.needs_system() && sys_state.is_some() && mic_state.is_some() {
        *sys_state = None;
        log::info!("removed system audio");
    }
}

/// Filter, encode, and deliver one finished segment to the handler.
#[allow(clippy::too_many_arguments)]
fn deliver_segment<H: FnMut(Segment)>(
    samples: &[f32],
    cfg: &Config,
    detector: &mut dyn ActivityDetector,
    handler: &mut H,
    index: &mut u64,
    start: Instant,
    is_final: bool,
) {
    if is_final && samples.len() < cfg.min_final_samples() {
        log::debug!(
            "final segment dropped ({} frames < {} min)",
            samples.len(),
            cfg.min_final_samples()
        );
        return;
    }
    if !detector.is_active(samples) {
        log::debug!("segment dropped (no activity)");
        return;
    }
    let data = match encode::encode(cfg.format, samples, cfg.sample_rate, 1, cfg.opus_bitrate) {
        Ok(d) => d,
        Err(e) => {
            log::error!("failed to encode segment: {e}");
            return;
        }
    };
    let frames = samples.len();
    *index += 1;
    let seg = Segment {
        index: *index,
        data,
        format: cfg.format,
        sample_rate: cfg.sample_rate,
        channels: 1,
        frames,
        duration: Duration::from_secs_f64(frames as f64 / cfg.sample_rate as f64),
        offset: start.elapsed(),
        is_final,
    };
    handler(seg);
}

/// Worker entry point (own thread). Sends one start result on `ready_tx`, sets
/// `running` false when it exits, loops until `stop` is set or a device faults,
/// then flushes the tail.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run<H>(
    cfg: Config,
    mut handler: H,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<Error>>>,
    ready_tx: mpsc::Sender<Result<()>>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
) where
    H: FnMut(Segment) + Send + 'static,
{
    let rate = cfg.sample_rate;
    let mix_cap = rate as usize; // ~1 s of system residual at the output rate

    let mut detector = match make_detector(&cfg.activity, rate) {
        Ok(d) => d,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            running.store(false, Ordering::Relaxed);
            return;
        }
    };

    let fault: Fault = Arc::new(Mutex::new(None));
    let mic_overruns = Arc::new(AtomicU64::new(0));
    let sys_overruns = Arc::new(AtomicU64::new(0));

    let mut mic_state: Option<SourceState> = None;
    let mut sys_state: Option<SourceState> = None;

    if cfg.source.needs_mic() {
        match open_source(true, rate, mic_overruns.clone(), fault.clone()) {
            Ok(s) => mic_state = Some(s),
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                running.store(false, Ordering::Relaxed);
                return;
            }
        }
    }
    if cfg.source.needs_system() {
        match open_source(false, rate, sys_overruns.clone(), fault.clone()) {
            Ok(s) => sys_state = Some(s),
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                running.store(false, Ordering::Relaxed);
                return;
            }
        }
    }

    let _ = ready_tx.send(Ok(()));

    let mut mic16k: Vec<f32> = Vec::with_capacity(rate as usize);
    let mut sys16k: Vec<f32> = Vec::with_capacity(rate as usize);
    let mut mixed: Vec<f32> = Vec::with_capacity(rate as usize);
    let mut sys_residual: Vec<f32> = Vec::with_capacity(mix_cap);
    let mut segmenter = Segmenter::new(cfg.segment_samples());

    let mut mic_gain = cfg.mic_gain.clamp(0.0, MAX_GAIN);
    let mut system_gain = cfg.system_gain.clamp(0.0, MAX_GAIN);

    let mut index: u64 = 0;
    let mut last_overruns: u64 = 0;
    let start = Instant::now();

    loop {
        let should_stop = stop.load(Ordering::Relaxed);
        let device_failed = fault.lock().map_or(true, |g| g.is_some());

        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCommand::SetSource(s) => apply_source(
                    s,
                    rate,
                    &mut mic_state,
                    &mut sys_state,
                    &mic_overruns,
                    &sys_overruns,
                    &fault,
                    &mut sys_residual,
                ),
                WorkerCommand::SetGains { mic, system } => {
                    mic_gain = mic.clamp(0.0, MAX_GAIN);
                    system_gain = system.clamp(0.0, MAX_GAIN);
                }
            }
        }

        mic16k.clear();
        sys16k.clear();
        if let Some(s) = mic_state.as_mut() {
            s.drain_into(&mut mic16k);
        }
        if let Some(s) = sys_state.as_mut() {
            s.drain_into(&mut sys16k);
        }
        scale_in_place(&mut mic16k, mic_gain);
        scale_in_place(&mut sys16k, system_gain);
        let out: &[f32] = match (mic_state.is_some(), sys_state.is_some()) {
            (true, true) => {
                mix_into(&mic16k, &sys16k, &mut sys_residual, &mut mixed, mix_cap);
                &mixed
            }
            (true, false) => &mic16k,
            (false, true) => &sys16k,
            (false, false) => &[],
        };

        segmenter.push(out, |seg| {
            deliver_segment(
                seg,
                &cfg,
                &mut *detector,
                &mut handler,
                &mut index,
                start,
                false,
            )
        });

        let overruns = mic_overruns.load(Ordering::Relaxed) + sys_overruns.load(Ordering::Relaxed);
        if overruns != last_overruns {
            last_overruns = overruns;
            log::warn!("dropped {overruns} captured samples total (consumer fell behind)");
        }

        if device_failed {
            log::error!("audio device error; stopping recording");
            break;
        }
        if should_stop {
            break;
        }
        thread::sleep(TICK);
    }

    // Stop capture, then flush whatever remains as the final segment.
    if let Some(s) = mic_state.as_mut() {
        s.stop_stream();
    }
    if let Some(s) = sys_state.as_mut() {
        s.stop_stream();
    }
    mic16k.clear();
    sys16k.clear();
    if let Some(s) = mic_state.as_mut() {
        s.flush_into(&mut mic16k);
    }
    if let Some(s) = sys_state.as_mut() {
        s.flush_into(&mut sys16k);
    }
    scale_in_place(&mut mic16k, mic_gain);
    scale_in_place(&mut sys16k, system_gain);
    let tail: &[f32] = match (mic_state.is_some(), sys_state.is_some()) {
        (true, true) => {
            mix_into(&mic16k, &sys16k, &mut sys_residual, &mut mixed, mix_cap);
            &mixed
        }
        (true, false) => &mic16k,
        (false, true) => &sys16k,
        (false, false) => &[],
    };
    segmenter.push(tail, |seg| {
        deliver_segment(
            seg,
            &cfg,
            &mut *detector,
            &mut handler,
            &mut index,
            start,
            true,
        )
    });
    segmenter.flush(|seg| {
        deliver_segment(
            seg,
            &cfg,
            &mut *detector,
            &mut handler,
            &mut index,
            start,
            true,
        )
    });

    // If a capture device faulted, report it as the terminal outcome so the
    // caller can tell a device loss apart from a clean stop.
    if let Some(msg) = fault.lock().ok().and_then(|mut g| g.take()) {
        if let Ok(mut slot) = outcome.lock() {
            *slot = Some(Error::DeviceLost(msg));
        }
    }

    log::info!("recording stopped: {index} segment(s) delivered");
    running.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Activity, Format};
    use crate::vad::make_detector;

    /// Count how many segments `deliver_segment` hands to the handler for the
    /// given buffer/`is_final`, using a 3 s `min_final_segment` and the keep-all
    /// detector so only the length gate can drop anything.
    fn delivered(samples: &[f32], is_final: bool) -> usize {
        let cfg = Config {
            format: Format::Wav,
            sample_rate: 16_000,
            min_final_segment: Duration::from_secs(3),
            ..Config::default()
        };
        let mut detector = make_detector(&Activity::KeepAll, cfg.sample_rate).unwrap();
        let mut index = 0u64;
        let mut count = 0usize;
        {
            let mut handler = |_seg: Segment| count += 1;
            deliver_segment(
                samples,
                &cfg,
                &mut *detector,
                &mut handler,
                &mut index,
                Instant::now(),
                is_final,
            );
        }
        count
    }

    #[test]
    fn drops_short_final_tail() {
        // 1 s < 3 s, final -> dropped.
        assert_eq!(delivered(&vec![0.1f32; 16_000], true), 0);
    }

    #[test]
    fn keeps_long_final_tail() {
        // 5 s >= 3 s, final -> kept.
        assert_eq!(delivered(&vec![0.1f32; 16_000 * 5], true), 1);
    }

    #[test]
    fn min_final_only_applies_to_the_tail() {
        // 1 s but not final -> kept (full segments are never length-gated).
        assert_eq!(delivered(&vec![0.1f32; 16_000], false), 1);
    }
}
