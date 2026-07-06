//! The single worker/consumer thread: drains the capture ring buffer(s),
//! resamples to the target rate, mixes (in `Mixed` mode) or keeps the sources
//! apart (in `Separate` mode), cuts fixed-length segments, runs the activity
//! filter, encodes, and hands each kept segment to the user's handler.
//!
//! Memory is flat for the whole recording: fixed-capacity ring buffers + a small
//! set of reused scratch/segment buffers. Nothing accumulates.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use ringbuf::traits::{Consumer, Split};
use ringbuf::{HeapCons, HeapRb};

use crate::capture::{open_mic, open_system, CaptureHandle, Fault};
use crate::config::{Config, Source};
use crate::encode;
use crate::error::{Error, Result};
use crate::resample::StreamResampler;
use crate::segment::{iso8601_utc, Segment, Track};
use crate::segmenter::Segmenter;
use crate::tap::{FrameTap, Frames};
use crate::vad::{make_detector, ActivityDetector};

/// ~4 s of headroom at 48 kHz mono. Fixed at startup, never grows.
const RING_CAPACITY: usize = 48_000 * 4;
/// Samples popped from a ring buffer per step.
const POP_SCRATCH: usize = 4096;
/// Worker poll interval. Segment latency is seconds, so 20 ms is plenty.
const TICK: Duration = Duration::from_millis(20);
/// Upper bound for user gain (~ +18 dB).
const MAX_GAIN: f32 = 8.0;

/// Clears the `running` flag when the worker exits — including on an unwinding
/// panic out of the user's handler — so [`Recording::is_running`] can't get
/// stuck reporting `true` after the worker thread has actually died.
///
/// [`Recording::is_running`]: crate::Recording::is_running
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

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

/// One output track's tail of the pipeline: cut into fixed-length segments,
/// gate on activity, number, timestamp, and deliver. There is one in single-track
/// modes (`Mic` / `System` / `Mixed`) and two — mic and system — in `Separate`,
/// each with its own `index` so the two streams number independently.
struct TrackPipeline {
    track: Track,
    segmenter: Segmenter,
    detector: Box<dyn ActivityDetector>,
    index: u64,
    /// Audio frames this track has completed windows for (kept *and* dropped).
    frames_done: u64,
    /// Audio frames ever pushed into this track's segmenter. The newest one was
    /// captured ~`now` (this tick), so it ties the sample timeline to the clock.
    pushed: u64,
}

impl TrackPipeline {
    fn new(track: Track, cfg: &Config) -> Result<Self> {
        Ok(Self {
            track,
            segmenter: Segmenter::new(cfg.segment_samples()),
            detector: make_detector(&cfg.activity, cfg.sample_rate)?,
            index: 0,
            frames_done: 0,
            pushed: 0,
        })
    }

    /// Cut `samples` into segments, delivering each completed one. `now` is this
    /// tick's wall clock (when these samples were captured); `is_final` tags
    /// segments emitted while flushing the tail at stop.
    fn push<H: FnMut(Segment)>(
        &mut self,
        samples: &[f32],
        cfg: &Config,
        handler: &mut H,
        now: SystemTime,
        is_final: bool,
    ) {
        self.pushed += samples.len() as u64;
        // Destructure so the segmenter borrow stays disjoint from the detector /
        // index / clock borrows the delivery closure needs.
        let Self {
            track,
            segmenter,
            detector,
            index,
            frames_done,
            pushed,
        } = self;
        let (track, pushed) = (*track, *pushed);
        segmenter.push(samples, |seg| {
            deliver_segment(
                seg,
                track,
                cfg,
                &mut **detector,
                handler,
                index,
                frames_done,
                pushed,
                now,
                is_final,
            );
        });
    }

    /// Emit whatever partial segment remains (called once per track at stop).
    fn flush<H: FnMut(Segment)>(&mut self, cfg: &Config, handler: &mut H, now: SystemTime) {
        let Self {
            track,
            segmenter,
            detector,
            index,
            frames_done,
            pushed,
        } = self;
        let (track, pushed) = (*track, *pushed);
        segmenter.flush(|seg| {
            deliver_segment(
                seg,
                track,
                cfg,
                &mut **detector,
                handler,
                index,
                frames_done,
                pushed,
                now,
                true,
            );
        });
    }
}

/// Get the pipeline in `slot`, creating it on first use. Returns `None` only if
/// the (one-time) detector build fails — extremely unlikely once the initial
/// pipelines have been built, so the track is simply skipped and logged rather
/// than aborting a live recording.
fn ensure_pipe<'a>(
    slot: &'a mut Option<TrackPipeline>,
    track: Track,
    cfg: &Config,
) -> Option<&'a mut TrackPipeline> {
    if slot.is_none() {
        match TrackPipeline::new(track, cfg) {
            Ok(p) => *slot = Some(p),
            Err(e) => {
                log::error!("could not build {} activity detector: {e}", track.as_str());
                return None;
            }
        }
    }
    slot.as_mut()
}

/// Mix `Mixed` mode: mic is the clock master; add the next system sample (zero if
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

/// Tap-owned mix state, needed only in `Separate` mode (the delivery path
/// keeps the tracks apart there, but the tap contract is one mono stream).
/// Starts empty — no memory is spent until a tap actually needs the mix.
#[derive(Default)]
struct TapMix {
    residual: Vec<f32>,
    mixed: Vec<f32>,
}

/// `Separate`-mode tap delivery: sum the active sources (mic as clock master,
/// the same policy as `Mixed`) into tap-owned buffers, or pass a single active
/// source through borrowed as-is. In the single-track modes the tap instead
/// reuses the delivery buffer directly — zero extra work, zero extra memory.
#[allow(clippy::too_many_arguments)]
fn tap_separate(
    tap: &mut FrameTap,
    mix: &mut TapMix,
    mic: &[f32],
    sys: &[f32],
    mic_active: bool,
    sys_active: bool,
    mix_cap: usize,
    sample_rate: u32,
    now: SystemTime,
) {
    let samples: &[f32] = match (mic_active, sys_active) {
        (true, true) => {
            mix_into(mic, sys, &mut mix.residual, &mut mix.mixed, mix_cap);
            &mix.mixed
        }
        (true, false) => mic,
        (false, true) => sys,
        (false, false) => &[],
    };
    if samples.is_empty() {
        return;
    }
    tap(Frames {
        samples,
        sample_rate,
        captured_at: now,
    });
}

/// Single-track tap delivery: the delivery buffer already is the one mono
/// stream the tap wants, so it is borrowed as-is.
fn tap_single(tap: &mut Option<FrameTap>, samples: &[f32], sample_rate: u32, now: SystemTime) {
    let Some(tap) = tap.as_mut() else { return };
    if samples.is_empty() {
        return;
    }
    tap(Frames {
        samples,
        sample_rate,
        captured_at: now,
    });
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

/// Filter, encode, timestamp, and deliver one finished segment to the handler.
///
/// Timestamps are read from the wall clock, not accumulated: `now` is the capture
/// time of the newest pushed frame (index `pushed`); this window ends `pushed -
/// (frames_done + n)` frames before it. So every segment re-anchors to the real
/// clock — no drift builds up even if frames were dropped earlier.
#[allow(clippy::too_many_arguments)]
fn deliver_segment<H: FnMut(Segment)>(
    samples: &[f32],
    track: Track,
    cfg: &Config,
    detector: &mut dyn ActivityDetector,
    handler: &mut H,
    index: &mut u64,
    frames_done: &mut u64,
    pushed: u64,
    now: SystemTime,
    is_final: bool,
) {
    let n = samples.len();
    // Advance the per-track frame count for *every* window (kept or dropped) so a
    // dropped window doesn't shift the frame-to-clock mapping of later ones.
    let frames_after = *frames_done + n as u64;
    let pending = pushed.saturating_sub(frames_after); // frames captured after this window's end
    *frames_done = frames_after;

    if is_final && n < cfg.min_final_samples() {
        log::debug!(
            "final segment dropped ({n} frames < {} min)",
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
    let rate = cfg.sample_rate as f64;
    let end = now
        .checked_sub(Duration::from_secs_f64(pending as f64 / rate))
        .unwrap_or(now);
    let start = end
        .checked_sub(Duration::from_secs_f64(n as f64 / rate))
        .unwrap_or(end);
    *index += 1;
    let seg = Segment {
        track,
        index: *index,
        data,
        format: cfg.format,
        sample_rate: cfg.sample_rate,
        channels: 1,
        frames: n,
        start_time: iso8601_utc(start),
        end_time: iso8601_utc(end),
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
    mut tap: Option<FrameTap>,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<Error>>>,
    ready_tx: mpsc::Sender<Result<()>>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
) where
    H: FnMut(Segment) + Send + 'static,
{
    // Clears `running` on every exit path — normal return, early error, or a
    // panic unwinding out of the handler.
    let _running_guard = RunningGuard(running);

    let rate = cfg.sample_rate;
    let mix_cap = rate as usize; // ~1 s of system residual at the output rate

    // The active delivery mode; switchable live via `SetSource`.
    let mut mode = cfg.source;
    // One pipeline per output track. `Mic` / `System` / `Mixed` use exactly one;
    // `Separate` uses `mic_pipe` + `sys_pipe`. The mode's initial pipeline(s) are
    // built up front (below) so a detector-build failure aborts `start` rather
    // than silently disabling the gate; live mode switches build lazily.
    let mut mixed_pipe: Option<TrackPipeline> = None;
    let mut mic_pipe: Option<TrackPipeline> = None;
    let mut sys_pipe: Option<TrackPipeline> = None;

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
                return;
            }
        }
    }
    if cfg.source.needs_system() {
        match open_source(false, rate, sys_overruns.clone(), fault.clone()) {
            Ok(s) => sys_state = Some(s),
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
    }

    // Build the activity gate + segmenter for the track(s) this mode delivers,
    // before signalling ready, so a detector-build failure surfaces from `start`.
    let build = (|| -> Result<()> {
        match mode {
            Source::Mic => mic_pipe = Some(TrackPipeline::new(Track::Mic, &cfg)?),
            Source::System => sys_pipe = Some(TrackPipeline::new(Track::System, &cfg)?),
            Source::Mixed => mixed_pipe = Some(TrackPipeline::new(Track::Mixed, &cfg)?),
            Source::Separate => {
                mic_pipe = Some(TrackPipeline::new(Track::Mic, &cfg)?);
                sys_pipe = Some(TrackPipeline::new(Track::System, &cfg)?);
            }
        }
        Ok(())
    })();
    if let Err(e) = build {
        let _ = ready_tx.send(Err(e));
        return;
    }

    let _ = ready_tx.send(Ok(()));

    let mut mic16k: Vec<f32> = Vec::with_capacity(rate as usize);
    let mut sys16k: Vec<f32> = Vec::with_capacity(rate as usize);
    let mut mixed: Vec<f32> = Vec::with_capacity(rate as usize);
    let mut sys_residual: Vec<f32> = Vec::with_capacity(mix_cap);
    // Empty until a tap actually needs a Separate-mode mix (see TapMix).
    let mut tap_mix = TapMix::default();

    let mut mic_gain = cfg.mic_gain.clamp(0.0, MAX_GAIN);
    let mut system_gain = cfg.system_gain.clamp(0.0, MAX_GAIN);

    let mut last_overruns: u64 = 0;

    loop {
        let should_stop = stop.load(Ordering::Relaxed);
        let device_failed = fault.lock().map_or(true, |g| g.is_some());
        // Anchor for any pipeline created this tick (a track just added live).
        let now = SystemTime::now();

        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCommand::SetSource(s) => {
                    apply_source(
                        s,
                        rate,
                        &mut mic_state,
                        &mut sys_state,
                        &mic_overruns,
                        &sys_overruns,
                        &fault,
                        &mut sys_residual,
                    );
                    mode = s;
                }
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

        if mode.is_separate() {
            // The delivery path keeps the tracks apart, so only here does the
            // tap need its own one-stream mix.
            if let Some(t) = tap.as_mut() {
                tap_separate(
                    t,
                    &mut tap_mix,
                    &mic16k,
                    &sys16k,
                    mic_state.is_some(),
                    sys_state.is_some(),
                    mix_cap,
                    rate,
                    now,
                );
            }
            // Two independent tracks: each source is segmented and gated on its
            // own. No mixing, no clock master — nothing is dropped to bound drift.
            if mic_state.is_some() {
                if let Some(p) = ensure_pipe(&mut mic_pipe, Track::Mic, &cfg) {
                    p.push(&mic16k, &cfg, &mut handler, now, false);
                }
            }
            if sys_state.is_some() {
                if let Some(p) = ensure_pipe(&mut sys_pipe, Track::System, &cfg) {
                    p.push(&sys16k, &cfg, &mut handler, now, false);
                }
            }
        } else {
            let (track, out): (Track, &[f32]) = match (mic_state.is_some(), sys_state.is_some()) {
                (true, true) => {
                    mix_into(&mic16k, &sys16k, &mut sys_residual, &mut mixed, mix_cap);
                    (Track::Mixed, &mixed)
                }
                (true, false) => (Track::Mic, &mic16k),
                (false, true) => (Track::System, &sys16k),
                (false, false) => (Track::Mixed, &[]),
            };
            // The delivery buffer already is the tap's one mono stream.
            tap_single(&mut tap, out, rate, now);
            let slot = match track {
                Track::Mixed => &mut mixed_pipe,
                Track::Mic => &mut mic_pipe,
                Track::System => &mut sys_pipe,
            };
            if let Some(p) = ensure_pipe(slot, track, &cfg) {
                p.push(out, &cfg, &mut handler, now, false);
            }
        }

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

    // Wall clock at stop; the drained tail's newest frame was captured ~now.
    let now = SystemTime::now();
    // Feed the drained tail into the current mode's pipeline(s) as final
    // samples; the tap gets the tail too (the last words matter most live).
    if mode.is_separate() {
        if let Some(t) = tap.as_mut() {
            tap_separate(
                t,
                &mut tap_mix,
                &mic16k,
                &sys16k,
                mic_state.is_some(),
                sys_state.is_some(),
                mix_cap,
                rate,
                now,
            );
        }
        if mic_state.is_some() {
            if let Some(p) = ensure_pipe(&mut mic_pipe, Track::Mic, &cfg) {
                p.push(&mic16k, &cfg, &mut handler, now, true);
            }
        }
        if sys_state.is_some() {
            if let Some(p) = ensure_pipe(&mut sys_pipe, Track::System, &cfg) {
                p.push(&sys16k, &cfg, &mut handler, now, true);
            }
        }
    } else {
        let (track, tail): (Track, &[f32]) = match (mic_state.is_some(), sys_state.is_some()) {
            (true, true) => {
                mix_into(&mic16k, &sys16k, &mut sys_residual, &mut mixed, mix_cap);
                (Track::Mixed, &mixed)
            }
            (true, false) => (Track::Mic, &mic16k),
            (false, true) => (Track::System, &sys16k),
            (false, false) => (Track::Mixed, &[]),
        };
        tap_single(&mut tap, tail, rate, now);
        let slot = match track {
            Track::Mixed => &mut mixed_pipe,
            Track::Mic => &mut mic_pipe,
            Track::System => &mut sys_pipe,
        };
        if let Some(p) = ensure_pipe(slot, track, &cfg) {
            p.push(tail, &cfg, &mut handler, now, true);
        }
    }

    // Flush the partial remainder from every pipeline that was used — including
    // one left from a pre-switch mode — so no buffered tail is lost.
    for slot in [&mut mixed_pipe, &mut mic_pipe, &mut sys_pipe] {
        if let Some(p) = slot.as_mut() {
            p.flush(&cfg, &mut handler, now);
        }
    }

    // If a capture device faulted, report it as the terminal outcome so the
    // caller can tell a device loss apart from a clean stop.
    if let Some(msg) = fault.lock().ok().and_then(|mut g| g.take()) {
        if let Ok(mut slot) = outcome.lock() {
            *slot = Some(Error::DeviceLost(msg));
        }
    }

    let total: u64 = [&mixed_pipe, &mic_pipe, &sys_pipe]
        .into_iter()
        .filter_map(|p| p.as_ref().map(|p| p.index))
        .sum();
    log::info!("recording stopped: {total} segment(s) delivered");
    // `running` is cleared by `_running_guard` on return.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Activity, Format};
    use std::time::UNIX_EPOCH;

    fn cfg_1s() -> Config {
        Config {
            format: Format::Wav,
            sample_rate: 16_000,
            segment: Duration::from_secs(1),
            min_final_segment: Duration::from_secs(3),
            ..Config::default()
        }
    }

    /// Count how many segments `deliver_segment` hands to the handler for the
    /// given buffer/`is_final`, using a 3 s `min_final_segment` and the keep-all
    /// detector so only the length gate can drop anything.
    fn delivered(samples: &[f32], is_final: bool) -> usize {
        let cfg = cfg_1s();
        let mut detector = make_detector(&Activity::KeepAll, cfg.sample_rate).unwrap();
        let (mut index, mut frames_done) = (0u64, 0u64);
        let pushed = samples.len() as u64;
        let mut count = 0usize;
        {
            let mut handler = |_seg: Segment| count += 1;
            deliver_segment(
                samples,
                Track::Mic,
                &cfg,
                &mut *detector,
                &mut handler,
                &mut index,
                &mut frames_done,
                pushed,
                UNIX_EPOCH,
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

    #[test]
    fn pipeline_tags_track_and_numbers_per_instance() {
        let cfg = cfg_1s();
        // Two pipelines stand in for the mic and system tracks.
        let mut mic = TrackPipeline::new(Track::Mic, &cfg).unwrap();
        let mut sys = TrackPipeline::new(Track::System, &cfg).unwrap();

        let mut got: Vec<(Track, u64)> = Vec::new();
        {
            let mut handler = |seg: Segment| got.push((seg.track, seg.index));
            // 2 s of mic -> two full 1 s segments; 1 s of system -> one.
            mic.push(
                &vec![0.2f32; 16_000 * 2],
                &cfg,
                &mut handler,
                UNIX_EPOCH,
                false,
            );
            sys.push(&vec![0.2f32; 16_000], &cfg, &mut handler, UNIX_EPOCH, false);
        }

        // Each track is tagged correctly and numbered from 1 independently.
        assert_eq!(
            got,
            vec![(Track::Mic, 1), (Track::Mic, 2), (Track::System, 1)]
        );
    }

    #[test]
    fn timestamps_are_contiguous_within_a_batch() {
        let cfg = cfg_1s(); // 1 s segments at 16 kHz
        let mut mic = TrackPipeline::new(Track::Mic, &cfg).unwrap();
        let mut got: Vec<(String, String)> = Vec::new();
        {
            let mut handler = |seg: Segment| got.push((seg.start_time, seg.end_time));
            // 2 s captured up to t = epoch + 2 s -> two back-to-back 1 s windows
            // ending at +1 s and +2 s, both read off that one clock value.
            let now = UNIX_EPOCH + Duration::from_secs(2);
            mic.push(&vec![0.2f32; 16_000 * 2], &cfg, &mut handler, now, false);
        }
        assert_eq!(
            got,
            vec![
                (
                    "1970-01-01T00:00:00.000Z".to_string(),
                    "1970-01-01T00:00:01.000Z".to_string()
                ),
                (
                    "1970-01-01T00:00:01.000Z".to_string(),
                    "1970-01-01T00:00:02.000Z".to_string()
                ),
            ]
        );
    }

    #[test]
    fn dropped_silent_window_does_not_shift_later_timestamps() {
        // Energy gate: a silent 1 s window is dropped, but the next (loud) window
        // is still timestamped at +1..+2 s — the drop doesn't pull it earlier.
        let cfg = Config {
            activity: Activity::energy(),
            ..cfg_1s()
        };
        let mut mic = TrackPipeline::new(Track::Mic, &cfg).unwrap();
        let mut got: Vec<(u64, String, String)> = Vec::new();
        {
            let mut handler = |seg: Segment| got.push((seg.index, seg.start_time, seg.end_time));
            let mut buf = vec![0.0f32; 16_000]; // 1 s of silence -> dropped
            buf.extend(vec![0.3f32; 16_000]); // 1 s of tone -> kept
            let now = UNIX_EPOCH + Duration::from_secs(2);
            mic.push(&buf, &cfg, &mut handler, now, false);
        }
        assert_eq!(
            got,
            vec![(
                1,
                "1970-01-01T00:00:01.000Z".to_string(),
                "1970-01-01T00:00:02.000Z".to_string()
            )]
        );
    }

    #[test]
    fn timestamps_come_from_the_capture_clock() {
        // Times track the wall clock passed in, not any fixed recording origin —
        // so a track started live is correct from the moment its audio arrives.
        let cfg = cfg_1s();
        let mut sys = TrackPipeline::new(Track::System, &cfg).unwrap();
        let mut got: Vec<(String, String)> = Vec::new();
        {
            let mut handler = |seg: Segment| got.push((seg.start_time, seg.end_time));
            let now = UNIX_EPOCH + Duration::from_secs(300); // captured 5 min in
            sys.push(&vec![0.2f32; 16_000], &cfg, &mut handler, now, false);
        }
        assert_eq!(
            got,
            vec![(
                "1970-01-01T00:04:59.000Z".to_string(),
                "1970-01-01T00:05:00.000Z".to_string()
            )]
        );
    }

    #[test]
    fn running_guard_clears_flag_on_drop() {
        // However the worker exits (incl. a handler panic unwinding through it),
        // the guard's Drop flips `running` to false.
        let flag = Arc::new(AtomicBool::new(true));
        {
            let _g = RunningGuard(flag.clone());
            assert!(flag.load(Ordering::Relaxed));
        }
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn source_predicates() {
        assert!(Source::Separate.needs_mic() && Source::Separate.needs_system());
        assert!(Source::Mixed.needs_mic() && Source::Mixed.needs_system());
        assert!(Source::Separate.is_separate());
        assert!(!Source::Mixed.is_separate());
        assert!(Source::Mic.needs_mic() && !Source::Mic.needs_system());
    }

    #[test]
    fn separate_tap_mixes_into_one_mono_stream() {
        let got: Arc<Mutex<Vec<Vec<f32>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let mut tap: FrameTap = Box::new(move |f: Frames<'_>| {
            assert_eq!(f.sample_rate, 16_000);
            sink.lock().unwrap().push(f.samples.to_vec());
        });
        let mut mix = TapMix::default();
        let now = SystemTime::now();

        // both sources active: summed, mic as clock master
        tap_separate(
            &mut tap,
            &mut mix,
            &[0.5, 0.5],
            &[0.25, -0.25],
            true,
            true,
            16_000,
            16_000,
            now,
        );
        // only the mic active: passthrough, the mix buffers stay untouched
        tap_separate(
            &mut tap,
            &mut mix,
            &[0.1],
            &[],
            true,
            false,
            16_000,
            16_000,
            now,
        );
        // no samples this tick: no delivery
        tap_separate(
            &mut tap,
            &mut mix,
            &[],
            &[],
            true,
            true,
            16_000,
            16_000,
            now,
        );

        assert_eq!(*got.lock().unwrap(), vec![vec![0.75, 0.25], vec![0.1]]);
    }

    #[test]
    fn single_tap_borrows_the_delivery_buffer_and_none_is_a_noop() {
        let got: Arc<Mutex<Vec<Vec<f32>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let mut tap: Option<FrameTap> = Some(Box::new(move |f: Frames<'_>| {
            sink.lock().unwrap().push(f.samples.to_vec());
        }));
        let now = SystemTime::now();
        tap_single(&mut tap, &[0.5, -0.5], 16_000, now);
        tap_single(&mut tap, &[], 16_000, now); // empty tick: no delivery
        assert_eq!(*got.lock().unwrap(), vec![vec![0.5, -0.5]]);

        let mut none: Option<FrameTap> = None;
        tap_single(&mut none, &[0.5], 16_000, now); // no tap: no-op
    }
}
