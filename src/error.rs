//! Error type for clipclip.

/// Errors from [`start`](crate::start) and the recording worker.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No capture device for the requested source (e.g. no microphone, or no
    /// PulseAudio monitor source on Linux).
    #[error("no {0} device available")]
    NoDevice(&'static str),
    /// A device query/configuration failed.
    #[error("audio device error: {0}")]
    Device(String),
    /// Building or starting the capture stream failed.
    #[error("could not build audio stream: {0}")]
    Stream(String),
    /// A capture device faulted mid-recording (e.g. the microphone was
    /// unplugged). Recording stops; this is reported by [`Recording::stop`].
    ///
    /// [`Recording::stop`]: crate::Recording::stop
    #[error("audio device lost during recording: {0}")]
    DeviceLost(String),
    /// Sample-rate conversion failed.
    #[error("resampler error: {0}")]
    Resample(String),
    /// Encoding a segment failed.
    #[error("encoding error: {0}")]
    Encode(String),
    /// Voice-activity detector initialisation failed.
    #[error("voice activity detector error: {0}")]
    Vad(String),
    /// The configuration was rejected (see message).
    #[error("invalid configuration: {0}")]
    Config(String),
    /// A live-control call was made but the recording is not running.
    #[error("recording is not running")]
    NotRunning,
    /// Capture did not come up within the start timeout.
    #[error("timed out starting audio capture")]
    StartTimeout,
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
