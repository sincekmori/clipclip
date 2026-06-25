//! The unit handed to your handler.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Format;

/// Which audio source a [`Segment`] carries.
///
/// With [`Source::Separate`](crate::Source::Separate) your handler receives two
/// interleaved streams — one [`Mic`](Track::Mic) and one [`System`](Track::System)
/// — and `track` is how you tell them apart. Single-source modes tag every
/// segment with the matching variant ([`Mixed`](Track::Mixed) for
/// [`Source::Mixed`](crate::Source::Mixed)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// Microphone + system audio summed into one mono track.
    Mixed,
    /// Microphone audio.
    Mic,
    /// System / speaker audio (loopback).
    System,
}

impl Track {
    /// Stable lowercase identifier (`"mixed"`, `"mic"`, `"system"`), handy for
    /// log lines and file names.
    pub fn as_str(self) -> &'static str {
        match self {
            Track::Mixed => "mixed",
            Track::Mic => "mic",
            Track::System => "system",
        }
    }
}

/// One finished audio segment, already encoded and ready to hand downstream.
#[derive(Debug, Clone)]
pub struct Segment {
    /// Which source this segment carries. With
    /// [`Source::Separate`](crate::Source::Separate) it distinguishes the two
    /// parallel streams; otherwise it is always the same for the recording.
    pub track: Track,
    /// 1-based sequence number, counted **per [`track`](Segment::track)** over the
    /// segments actually delivered, so `(track, index)` — not `index` alone — is
    /// unique across the recording.
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
    /// Wall-clock start of this segment's audio, as an ISO 8601 / RFC 3339
    /// timestamp in UTC with millisecond precision
    /// (e.g. `"2026-06-25T01:23:45.678Z"`). Read from the system clock as each
    /// segment completes (not accumulated from the recording start), so it never
    /// drifts over a long recording; expect a few milliseconds of jitter between
    /// one segment's `end_time` and the next's `start_time`.
    pub start_time: String,
    /// Wall-clock end of this segment's audio, same format as
    /// [`start_time`](Segment::start_time).
    pub end_time: String,
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

/// Format a wall-clock instant as an ISO 8601 / RFC 3339 timestamp in UTC with
/// millisecond precision (e.g. `"2026-06-25T01:23:45.678Z"`). Times before the
/// Unix epoch (not expected for a live recording) clamp to the epoch.
pub(crate) fn iso8601_utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Civil date `(year, month, day)` from days since 1970-01-01, via Howard
/// Hinnant's `civil_from_days` algorithm. Inputs here are always non-negative.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(unix_secs: u64, millis: u64) -> String {
        iso8601_utc(UNIX_EPOCH + Duration::from_millis(unix_secs * 1000 + millis))
    }

    #[test]
    fn formats_the_unix_epoch() {
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn formats_known_instants_with_millis() {
        // 2021-01-01T00:00:00Z = 1_609_459_200 s since the epoch.
        assert_eq!(at(1_609_459_200, 0), "2021-01-01T00:00:00.000Z");
        // 2026-06-25T01:23:45.678Z = 1_782_350_625 s since the epoch.
        assert_eq!(at(1_782_350_625, 678), "2026-06-25T01:23:45.678Z");
    }

    #[test]
    fn handles_leap_day() {
        // 2020-02-29T12:00:00Z = 1_582_977_600 s since the epoch.
        assert_eq!(at(1_582_977_600, 0), "2020-02-29T12:00:00.000Z");
    }
}
