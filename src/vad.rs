//! Activity detection: decide whether a finished segment is kept or dropped.
//!
//! Pluggable via [`Activity`](crate::Activity). The dependency-free energy gate
//! is always available; Silero V5 neural VAD is behind the `silero` feature.
//!
//! A detector scores the segment window-by-window and keeps it only when the
//! fraction of *active* windows reaches `min_active_ratio` (`0.0` = keep if any
//! window is active; e.g. `0.05` = drop a segment that is >= 95% silent).

use crate::config::Activity;
#[cfg_attr(not(feature = "silero"), allow(unused_imports))]
use crate::error::{Error, Result};

/// A segment is kept iff `is_active` returns true.
pub(crate) trait ActivityDetector: Send {
    fn is_active(&mut self, samples: &[f32]) -> bool;
}

/// Keep iff there is >= 1 active window AND the active fraction meets the ratio.
fn keep(active: usize, total: usize, ratio: f32) -> bool {
    active > 0 && total > 0 && (active as f32 / total as f32) >= ratio
}

/// Keeps every segment.
struct KeepAll;
impl ActivityDetector for KeepAll {
    fn is_active(&mut self, _samples: &[f32]) -> bool {
        true
    }
}

/// Dependency-free RMS energy gate (20 ms windows). Lets steady noise through.
struct EnergyGate {
    threshold_lin: f32,
    min_active_ratio: f32,
    window: usize,
}

impl EnergyGate {
    fn new(threshold_dbfs: f32, min_active_ratio: f32, sample_rate: u32) -> Self {
        Self {
            threshold_lin: 10f32.powf(threshold_dbfs / 20.0),
            min_active_ratio,
            window: ((sample_rate / 50).max(1)) as usize, // 20 ms
        }
    }
}

impl ActivityDetector for EnergyGate {
    fn is_active(&mut self, samples: &[f32]) -> bool {
        let mut active = 0usize;
        let mut total = 0usize;
        for w in samples.chunks(self.window) {
            total += 1;
            let sum_sq: f32 = w.iter().map(|x| x * x).sum();
            let rms = (sum_sq / w.len() as f32).sqrt();
            if rms >= self.threshold_lin {
                active += 1;
            }
        }
        keep(active, total, self.min_active_ratio)
    }
}

/// Silero V5 neural VAD. 16 kHz only, fixed 512-sample window.
#[cfg(feature = "silero")]
struct SileroVad {
    vad: voice_activity_detector::VoiceActivityDetector,
    threshold: f32,
    min_active_ratio: f32,
}

#[cfg(feature = "silero")]
impl SileroVad {
    const WINDOW: usize = 512;

    fn new(threshold: f32, min_active_ratio: f32) -> Result<Self> {
        let vad = voice_activity_detector::VoiceActivityDetector::builder()
            .sample_rate(16_000_i64)
            .chunk_size(Self::WINDOW)
            .build()
            .map_err(|e| Error::Vad(e.to_string()))?;
        Ok(Self {
            vad,
            threshold,
            min_active_ratio,
        })
    }
}

#[cfg(feature = "silero")]
impl ActivityDetector for SileroVad {
    fn is_active(&mut self, samples: &[f32]) -> bool {
        let mut active = 0usize;
        let mut total = 0usize;
        let mut chunks = samples.chunks_exact(Self::WINDOW);
        for w in &mut chunks {
            total += 1;
            if self.vad.predict(w.to_vec()) > self.threshold {
                active += 1;
            }
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            total += 1;
            let mut padded = vec![0.0f32; Self::WINDOW];
            padded[..rem.len()].copy_from_slice(rem);
            if self.vad.predict(padded) > self.threshold {
                active += 1;
            }
        }
        keep(active, total, self.min_active_ratio)
    }
}

/// Build the configured detector for the given output sample rate.
pub(crate) fn make_detector(
    activity: &Activity,
    sample_rate: u32,
) -> Result<Box<dyn ActivityDetector>> {
    Ok(match *activity {
        Activity::KeepAll => Box::new(KeepAll),
        Activity::Energy {
            threshold_dbfs,
            min_active_ratio,
        } => Box::new(EnergyGate::new(
            threshold_dbfs,
            min_active_ratio,
            sample_rate,
        )),
        #[cfg(feature = "silero")]
        Activity::Silero {
            threshold,
            min_active_ratio,
        } => Box::new(SileroVad::new(threshold, min_active_ratio)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin())
            .collect()
    }

    #[test]
    fn energy_gate_rejects_silence_keeps_loud() {
        let mut g = EnergyGate::new(-50.0, 0.0, 16_000);
        assert!(!g.is_active(&vec![0.0f32; 16_000]));
        assert!(g.is_active(&tone(16_000, 0.3)));
    }

    #[test]
    fn min_active_ratio_drops_mostly_silent() {
        let total = 16_000 * 30;
        let mut buf = vec![0.0f32; total];
        let t = tone(total / 50, 0.3); // ~2% active
        buf[..t.len()].copy_from_slice(&t);
        assert!(EnergyGate::new(-50.0, 0.0, 16_000).is_active(&buf));
        assert!(!EnergyGate::new(-50.0, 0.05, 16_000).is_active(&buf));
    }

    #[cfg(feature = "silero")]
    #[test]
    fn silero_loads_and_scores_silence_inactive() {
        let mut vad = SileroVad::new(0.5, 0.0).expect("silero init");
        assert!(!vad.is_active(&vec![0.0f32; 16_000]));
    }
}
