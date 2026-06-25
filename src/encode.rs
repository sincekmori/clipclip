//! Encode a 16 kHz (configurable) mono f32 segment into in-memory bytes.
//!
//! No files are written here — the bytes are handed to the user's handler.

use crate::config::Format;
#[cfg(feature = "opus")]
use crate::error::Error;
use crate::error::Result;

/// Encode one segment to a complete, standalone file in memory.
#[cfg_attr(not(feature = "opus"), allow(unused_variables))]
pub(crate) fn encode(
    format: Format,
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    opus_bitrate: u32,
) -> Result<Vec<u8>> {
    match format {
        Format::Wav => Ok(encode_wav(samples, sample_rate, channels)),
        #[cfg(feature = "opus")]
        Format::Opus => encode_opus(samples, sample_rate, channels, opus_bitrate),
    }
}

/// 16-bit PCM WAV (RIFF/WAVE) built directly into a `Vec<u8>`.
fn encode_wav(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits: u16 = 16;
    let block_align = channels * (bits / 8);
    let byte_rate = sample_rate * block_align as u32;
    let data_len = (samples.len() * 2) as u32;

    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Ogg Opus via libopusenc in pull mode — pages are pulled into a `Vec<u8>`,
/// so nothing is written to disk.
#[cfg(feature = "opus")]
fn encode_opus(samples: &[f32], sample_rate: u32, channels: u16, bitrate: u32) -> Result<Vec<u8>> {
    use libopusenc::{
        OpusEncApplication, OpusEncBitrate, OpusEncChannelMapping, OpusEncComments,
        OpusEncSampleRate, OpusEncoder,
    };

    let rate = match sample_rate {
        48000 => OpusEncSampleRate::Hz48000,
        24000 => OpusEncSampleRate::Hz24000,
        16000 => OpusEncSampleRate::Hz16000,
        12000 => OpusEncSampleRate::Hz12000,
        8000 => OpusEncSampleRate::Hz8000,
        other => {
            return Err(Error::Encode(format!(
                "unsupported Opus sample rate {other}"
            )))
        }
    };

    let mut comments = OpusEncComments::create().map_err(|e| Error::Encode(e.to_string()))?;
    let _ = comments.add("ENCODER", "clipclip");

    let mut enc = OpusEncoder::create_pull(
        &mut comments,
        rate,
        channels as u8,
        OpusEncChannelMapping::MonoStereo,
    )
    .map_err(|e| Error::Encode(e.to_string()))?;

    enc.set_application(OpusEncApplication::Voip)
        .and_then(|e| e.set_vbr(true))
        .and_then(|e| e.set_bitrate(OpusEncBitrate::Explicit(bitrate)))
        .map_err(|e| Error::Encode(e.to_string()))?;

    enc.write_float(samples, channels as usize)
        .map_err(|e| Error::Encode(e.to_string()))?;

    let mut out = Vec::new();
    // Pull queued pages, finalize, then pull the remaining (flushed) pages.
    while let Some(page) = enc.get_page(false) {
        out.extend_from_slice(page);
    }
    enc.drain().map_err(|e| Error::Encode(e.to_string()))?;
    while let Some(page) = enc.get_page(true) {
        out.extend_from_slice(page);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| 0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin())
            .collect()
    }

    #[test]
    fn wav_has_riff_header_and_expected_size() {
        let pcm = tone(16_000);
        let data = encode(Format::Wav, &pcm, 16_000, 1, 24_000).unwrap();
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");
        assert_eq!(data.len(), 44 + pcm.len() * 2);
    }

    #[cfg(feature = "opus")]
    #[test]
    fn opus_produces_an_ogg_stream() {
        let pcm = tone(16_000);
        let data = encode(Format::Opus, &pcm, 16_000, 1, 24_000).unwrap();
        assert!(data.len() > 100, "opus output too small");
        assert_eq!(&data[0..4], b"OggS", "not an Ogg stream");
    }
}
