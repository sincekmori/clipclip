//! Recording configuration.

use std::time::Duration;

use crate::error::{Error, Result};

/// Which audio source(s) to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Microphone (default input device).
    Mic,
    /// System / speaker audio (loopback).
    System,
    /// Microphone + system audio, mixed into one mono track.
    Both,
}

impl Source {
    pub(crate) fn needs_mic(self) -> bool {
        matches!(self, Source::Mic | Source::Both)
    }
    pub(crate) fn needs_system(self) -> bool {
        matches!(self, Source::System | Source::Both)
    }
}

/// Output container/codec for each segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Ogg Opus (~24 kbps VBR). Requires the `opus` feature.
    #[cfg(feature = "opus")]
    Opus,
    /// 16-bit PCM WAV (lossless, ~10× larger than Opus).
    Wav,
}

/// Which segments to keep. Dropped segments are never handed to your handler.
#[derive(Debug, Clone, Copy, Default)]
pub enum Activity {
    /// Keep every segment (default).
    #[default]
    KeepAll,
    /// Dependency-free RMS energy gate. A segment is kept when the fraction of
    /// active windows (RMS above `threshold_dbfs`) reaches `min_active_ratio`
    /// (`0.0` = keep if any window is active).
    Energy {
        /// A 20 ms window counts as active when its RMS is at or above this (dBFS).
        threshold_dbfs: f32,
        /// Fraction of active windows needed to keep the segment (0.0 = any).
        min_active_ratio: f32,
    },
    /// Silero V5 speech detection. Requires the `silero` feature and a 16 kHz
    /// `sample_rate`. `min_active_ratio` works as for [`Activity::Energy`].
    #[cfg(feature = "silero")]
    Silero {
        /// A window counts as speech when the model's probability exceeds this.
        threshold: f32,
        /// Fraction of speech windows needed to keep the segment (0.0 = any).
        min_active_ratio: f32,
    },
}

impl Activity {
    /// Energy gate with sensible defaults (-50 dBFS, keep-if-any-active).
    pub fn energy() -> Self {
        Activity::Energy {
            threshold_dbfs: -50.0,
            min_active_ratio: 0.0,
        }
    }

    /// Silero VAD with sensible defaults (threshold 0.5, keep-if-any-speech).
    #[cfg(feature = "silero")]
    pub fn silero() -> Self {
        Activity::Silero {
            threshold: 0.5,
            min_active_ratio: 0.0,
        }
    }
}

/// Recording configuration. Build with [`Config::default`] and tweak fields:
///
/// ```no_run
/// use clipclip::{Config, Source};
/// use std::time::Duration;
///
/// let cfg = Config {
///     source: Source::Both,
///     segment: Duration::from_secs(10),
///     ..Config::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct Config {
    /// Source(s) to capture. Default [`Source::Mic`].
    pub source: Source,
    /// Length of each segment. Default 30s.
    pub segment: Duration,
    /// Output format. Default Opus (or WAV if the `opus` feature is off).
    pub format: Format,
    /// Which segments to keep. Default [`Activity::KeepAll`].
    pub activity: Activity,
    /// Output sample rate (mono). Default 16000 (ideal for ASR like Whisper).
    pub sample_rate: u32,
    /// Linear mic gain before mixing (1.0 = unchanged).
    pub mic_gain: f32,
    /// Linear system-audio gain before mixing (1.0 = unchanged).
    pub system_gain: f32,
    /// Opus VBR bitrate in bits/s (only used for [`Format::Opus`]). Default 24000.
    pub opus_bitrate: u32,
    /// Drop the final, partial segment produced at stop when it is shorter than
    /// this. Full segments are never affected. Default [`Duration::ZERO`] (keep
    /// every tail, matching [`Activity::KeepAll`]); set e.g.
    /// `Duration::from_secs(3)` to discard a tiny trailing clip.
    pub min_final_segment: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source: Source::Mic,
            segment: Duration::from_secs(30),
            #[cfg(feature = "opus")]
            format: Format::Opus,
            #[cfg(not(feature = "opus"))]
            format: Format::Wav,
            activity: Activity::KeepAll,
            sample_rate: 16_000,
            mic_gain: 1.0,
            system_gain: 1.0,
            opus_bitrate: 24_000,
            min_final_segment: Duration::ZERO,
        }
    }
}

impl Config {
    /// Number of mono samples in one full segment.
    pub(crate) fn segment_samples(&self) -> usize {
        ((self.sample_rate as f64) * self.segment.as_secs_f64()).round() as usize
    }

    /// Minimum sample count for the final partial segment to be kept. A shorter
    /// tail is dropped instead of handed to the handler.
    pub(crate) fn min_final_samples(&self) -> usize {
        ((self.sample_rate as f64) * self.min_final_segment.as_secs_f64()).round() as usize
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.sample_rate == 0 {
            return Err(Error::Config("sample_rate must be > 0".into()));
        }
        if self.segment_samples() == 0 {
            return Err(Error::Config("segment must be > 0".into()));
        }
        #[cfg(feature = "silero")]
        if matches!(self.activity, Activity::Silero { .. }) && self.sample_rate != 16_000 {
            return Err(Error::Config(
                "Silero VAD requires sample_rate = 16000".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_final_samples_scales_with_rate() {
        let cfg = Config {
            sample_rate: 16_000,
            min_final_segment: Duration::from_secs(3),
            ..Config::default()
        };
        assert_eq!(cfg.min_final_samples(), 48_000);
    }

    #[test]
    fn default_keeps_every_tail() {
        // The default mirrors `Activity::KeepAll`: nothing is dropped unasked.
        assert_eq!(Config::default().min_final_segment, Duration::ZERO);
        assert_eq!(Config::default().min_final_samples(), 0);
    }
}
