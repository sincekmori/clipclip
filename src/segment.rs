//! The unit handed to your handler.

use std::time::Duration;

use crate::config::Format;

/// One finished audio segment, already encoded and ready to hand downstream.
#[derive(Debug, Clone)]
pub struct Segment {
    /// 1-based sequence number across the recording.
    pub index: u64,
    /// Encoded bytes (a complete, standalone Ogg Opus or WAV file).
    pub data: Vec<u8>,
    /// The encoding of [`Segment::data`].
    pub format: Format,
    /// Sample rate of the audio (mono).
    pub sample_rate: u32,
    /// Channel count (always 1 — mono — for now).
    pub channels: u16,
    /// Number of PCM frames (samples per channel) in this segment.
    pub frames: usize,
    /// Duration of this segment's audio.
    pub duration: Duration,
    /// Offset of this segment from the start of the recording.
    pub offset: Duration,
    /// True for the final (possibly shorter) segment flushed at stop.
    pub is_final: bool,
}

impl Segment {
    /// File extension for [`Segment::format`] (without the dot).
    pub fn extension(&self) -> &'static str {
        match self.format {
            #[cfg(feature = "opus")]
            Format::Opus => "opus",
            Format::Wav => "wav",
        }
    }
}
