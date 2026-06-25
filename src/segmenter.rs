//! Splits a continuous 16 kHz mono stream into fixed 30s segments.
//!
//! Memory: a single buffer is reused for every segment (never reallocated, never
//! accumulated). When it fills, the closure is invoked with a borrow of the data
//! and the buffer is cleared (capacity retained) for the next segment.

pub struct Segmenter {
    buf: Vec<f32>,
    segment_samples: usize,
}

impl Segmenter {
    pub fn new(segment_samples: usize) -> Self {
        Self {
            buf: Vec::with_capacity(segment_samples),
            segment_samples,
        }
    }

    /// Append `input`; for every completed segment, call `on_segment` with the
    /// full buffer. The buffer is cleared (not freed) after each call.
    pub fn push<F>(&mut self, input: &[f32], mut on_segment: F)
    where
        F: FnMut(&[f32]),
    {
        let mut idx = 0;
        while idx < input.len() {
            let remaining = self.segment_samples - self.buf.len();
            let take = remaining.min(input.len() - idx);
            self.buf.extend_from_slice(&input[idx..idx + take]);
            idx += take;
            if self.buf.len() == self.segment_samples {
                on_segment(&self.buf);
                self.buf.clear();
            }
        }
    }

    /// Emit whatever partial segment remains (called once at stop). Anything
    /// captured after the last full segment is flushed through the same path so
    /// trailing speech is not lost.
    pub fn flush<F>(&mut self, mut on_segment: F)
    where
        F: FnMut(&[f32]),
    {
        if !self.buf.is_empty() {
            on_segment(&self.buf);
            self.buf.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_full_segments_and_flushes_remainder() {
        let mut seg = Segmenter::new(100);
        let mut lens = Vec::new();
        // 250 samples -> two full 100s during push...
        seg.push(&vec![1.0f32; 250], |s| lens.push(s.len()));
        assert_eq!(lens, vec![100, 100]);
        // ...and a 50-sample remainder on flush.
        seg.flush(|s| lens.push(s.len()));
        assert_eq!(lens, vec![100, 100, 50]);
    }

    #[test]
    fn no_flush_when_empty() {
        let mut seg = Segmenter::new(100);
        let mut called = false;
        seg.flush(|_| called = true);
        assert!(!called);
    }
}
