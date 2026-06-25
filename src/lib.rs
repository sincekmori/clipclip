#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod capture;
mod config;
mod encode;
mod error;
mod resample;
mod segment;
mod segmenter;
mod vad;
mod worker;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub use config::{Activity, Config, Format, Source};
pub use error::{Error, Result};
pub use segment::{Segment, Track};

use worker::WorkerCommand;

/// How long to wait for capture to come up before giving up.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// Start recording with `config`, delivering each kept [`Segment`] to `handler`.
///
/// The handler runs on a dedicated worker thread (not your caller thread), so it
/// may block on slow work. Returns once capture is live, or an [`Error`] if no
/// device is available / the configuration is invalid.
///
/// Hold the returned [`Recording`] for as long as you want to keep recording;
/// dropping it (or calling [`Recording::stop`]) stops capture and flushes the
/// final partial segment to your handler.
pub fn start<H>(config: Config, handler: H) -> Result<Recording>
where
    H: FnMut(Segment) + Send + 'static,
{
    config.validate()?;

    let stop = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));
    let outcome = Arc::new(Mutex::new(None));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();

    let stop_w = stop.clone();
    let running_w = running.clone();
    let outcome_w = outcome.clone();
    let join = std::thread::Builder::new()
        .name("clipclip-worker".into())
        .spawn(move || {
            worker::run(
                config, handler, stop_w, running_w, outcome_w, ready_tx, cmd_rx,
            )
        })
        .map_err(Error::Io)?;

    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(Recording {
            stop,
            running,
            outcome,
            join: Some(join),
            commands: cmd_tx,
        }),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            let _ = join.join();
            Err(Error::StartTimeout)
        }
    }
}

/// A handle to a running recording. **Drop it to stop** — the worker flushes the
/// final partial segment to your handler before this returns.
#[must_use = "dropping the Recording stops recording immediately; bind it (e.g. \
              `let _rec = start(..)?;`) to keep recording"]
pub struct Recording {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<Error>>>,
    join: Option<JoinHandle<()>>,
    commands: mpsc::Sender<WorkerCommand>,
}

impl Recording {
    /// Stop recording and wait for the final segment to be flushed.
    ///
    /// Returns [`Err`] with [`Error::DeviceLost`] if recording had already
    /// stopped because a capture device faulted (e.g. the microphone was
    /// unplugged), or [`Error::WorkerPanicked`] if the worker thread died
    /// unexpectedly (almost always a panic in your handler); otherwise [`Ok`].
    /// Dropping the handle instead stops the same way but discards this outcome.
    pub fn stop(mut self) -> Result<()> {
        self.shutdown();
        match self.outcome.lock() {
            Ok(mut slot) => slot.take().map_or(Ok(()), Err),
            Err(_) => Ok(()),
        }
    }

    /// Whether the worker is still capturing (false after [`stop`](Self::stop),
    /// a fatal device error, or a panic in your handler).
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Switch the active source(s) live, including between
    /// [`Source::Mixed`] and [`Source::Separate`]. Streams are opened/closed on
    /// the fly; the session and segment timing continue uninterrupted. Because
    /// [`Segment`] timestamps are read from the wall clock as each segment
    /// completes, a track added by the switch is timestamped correctly from the
    /// moment its audio arrives.
    pub fn set_source(&self, source: Source) -> Result<()> {
        self.commands
            .send(WorkerCommand::SetSource(source))
            .map_err(|_| Error::NotRunning)
    }

    /// Set the mic / system-audio gain live (linear, 1.0 = unchanged).
    pub fn set_gains(&self, mic: f32, system: f32) -> Result<()> {
        self.commands
            .send(WorkerCommand::SetGains { mic, system })
            .map_err(|_| Error::NotRunning)
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                // The worker thread unwound (most likely the handler panicked).
                // Surface it through `stop()` rather than swallowing it.
                if let Ok(mut slot) = self.outcome.lock() {
                    slot.get_or_insert(Error::WorkerPanicked);
                }
            }
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Recording` whose worker has already exited (no join handle), so
    /// `stop()` just drains `outcome`.
    fn stopped_recording(outcome: Option<Error>) -> Recording {
        let (commands, _rx) = mpsc::channel();
        Recording {
            stop: Arc::new(AtomicBool::new(true)),
            running: Arc::new(AtomicBool::new(false)),
            outcome: Arc::new(Mutex::new(outcome)),
            join: None,
            commands,
        }
    }

    #[test]
    fn stop_is_ok_after_clean_stop() {
        assert!(stopped_recording(None).stop().is_ok());
    }

    #[test]
    fn stop_reports_device_fault() {
        let rec = stopped_recording(Some(Error::DeviceLost("[mic] disconnected".into())));
        assert!(matches!(rec.stop(), Err(Error::DeviceLost(_))));
    }
}
