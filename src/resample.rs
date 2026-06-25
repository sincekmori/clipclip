//! Streaming mono resampler: native capture rate (44.1/48 kHz, …) → 16 kHz.
//!
//! Capture callbacks deliver arbitrary-sized chunks, but `rubato`'s fixed-input
//! FFT resampler wants exactly `chunk` input frames per call. We buffer a small
//! residual and feed full chunks; leftover frames (< one chunk) wait for the
//! next call. RAM is bounded: the residual is drained back below one chunk on
//! every `process`.

use rubato::{FftFixedIn, Resampler};

use crate::error::{Error, Result};

/// Input frames consumed per resampler step. ~21 ms at 48 kHz — small enough to
/// keep latency low, large enough for the FFT resampler to be efficient.
const CHUNK_IN: usize = 1024;

pub struct StreamResampler {
    // `None` means input rate already equals output rate → pass-through.
    inner: Option<FftFixedIn<f32>>,
    residual: Vec<f32>,
    out_scratch: Vec<Vec<f32>>, // one channel (mono)
    chunk_in: usize,
}

impl StreamResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Result<Self> {
        if in_rate == out_rate {
            return Ok(Self {
                inner: None,
                residual: Vec::new(),
                out_scratch: vec![Vec::new()],
                chunk_in: CHUNK_IN,
            });
        }
        let inner = FftFixedIn::<f32>::new(
            in_rate as usize,
            out_rate as usize,
            CHUNK_IN,
            2, // sub_chunks
            1, // mono
        )
        .map_err(|e| Error::Resample(format!("{in_rate}->{out_rate}: {e}")))?;

        let out_max = inner.output_frames_max();
        let mut residual = Vec::with_capacity(CHUNK_IN * 4);
        residual.clear();

        Ok(Self {
            inner: Some(inner),
            residual,
            out_scratch: vec![vec![0.0f32; out_max]],
            chunk_in: CHUNK_IN,
        })
    }

    /// Resample `input` (native-rate mono f32), appending 16 kHz mono samples to
    /// `out`. Pre-size `out` if you want to avoid growth; we only append.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        let Some(resampler) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return Ok(());
        };

        self.residual.extend_from_slice(input);
        while self.residual.len() >= self.chunk_in {
            let wave_in = [&self.residual[..self.chunk_in]];
            let (_consumed, produced) = resampler
                .process_into_buffer(&wave_in, &mut self.out_scratch, None)
                .map_err(|e| Error::Resample(e.to_string()))?;
            out.extend_from_slice(&self.out_scratch[0][..produced]);
            self.residual.drain(0..self.chunk_in);
        }
        Ok(())
    }

    /// Flush the tail (< one chunk) at stop time by zero-padding to a full chunk.
    /// A few milliseconds of trailing silence is acceptable and avoids dropping
    /// the very end of speech.
    pub fn flush(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let Some(resampler) = self.inner.as_mut() else {
            return Ok(());
        };
        if self.residual.is_empty() {
            return Ok(());
        }
        self.residual.resize(self.chunk_in, 0.0);
        let wave_in = [&self.residual[..self.chunk_in]];
        let (_c, produced) = resampler
            .process_into_buffer(&wave_in, &mut self.out_scratch, None)
            .map_err(|e| Error::Resample(e.to_string()))?;
        out.extend_from_slice(&self.out_scratch[0][..produced]);
        self.residual.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_48k_to_16k_about_one_third() {
        let mut r = StreamResampler::new(48_000, 16_000).unwrap();
        let input = vec![0.0f32; 48_000]; // 1 s @ 48 kHz
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        r.flush(&mut out).unwrap();
        // ~1 s @ 16 kHz = ~16000 samples (allow resampler edge slack).
        assert!(
            (15_000..=17_000).contains(&out.len()),
            "got {} samples",
            out.len()
        );
    }

    #[test]
    fn passthrough_when_rates_match() {
        let mut r = StreamResampler::new(16_000, 16_000).unwrap();
        let input = vec![0.5f32; 1000];
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        assert_eq!(out, input);
    }
}
