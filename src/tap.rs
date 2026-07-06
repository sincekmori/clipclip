//! Live frame tap: raw resampled audio delivered as it is captured, alongside
//! (not instead of) the normal segment pipeline. Built for consumers that need
//! the stream in realtime — e.g. feeding a streaming speech-to-text API while
//! segments keep being recorded as the durable fallback.

use std::time::SystemTime;

/// One tick's worth of post-gain mono samples, handed to a [`FrameTap`] as
/// soon as they are drained from the capture device(s).
///
/// Samples are `f32` in `[-1.0, 1.0]` at the configured
/// [`Config::sample_rate`](crate::Config::sample_rate). When more than one
/// source is active the tap receives their sum (mic as the clock master — the
/// same policy as [`Source::Mixed`](crate::Source::Mixed)) regardless of the
/// configured delivery [`Source`](crate::Source), so the tap is always exactly
/// one continuous mono stream.
#[derive(Debug)]
pub struct Frames<'a> {
    /// Mono samples for this tick, oldest first.
    pub samples: &'a [f32],
    /// Sample rate of `samples` (the configured output rate).
    pub sample_rate: u32,
    /// Approximate wall-clock capture time of the **last** sample in
    /// `samples`; the first one was captured `samples.len() / sample_rate`
    /// seconds earlier. Every delivery re-anchors the stream to the real
    /// clock, so consumers can map sample offsets to wall-clock time without
    /// accumulating drift.
    pub captured_at: SystemTime,
}

/// Receives [`Frames`] on the worker thread, between device drains. Keep it
/// fast: hand the data off (channel, ring buffer) and return — blocking here
/// stalls segmenting and risks capture overruns.
pub type FrameTap = Box<dyn FnMut(Frames<'_>) + Send + 'static>;
