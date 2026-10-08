use sdr_core::{Complex32, Samples};

/// Fixed-capacity look-back buffer addressed by absolute sample index.
///
/// Stored as interleaved i16 IQ: half the memory of f32, and lossless for the
/// 12-bit Airspy and 8-bit HackRF/RTL formats. Float sources are scaled to
/// full-scale i16. Gaps in the sample counter are zero-filled.
pub struct IqRing {
    buf: Vec<[i16; 2]>,
    /// Absolute index of the next sample to be written.
    head: u64,
}

impl IqRing {
    pub fn new(capacity: usize) -> Self {
        Self { buf: vec![[0, 0]; capacity.max(1)], head: 0 }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn head(&self) -> u64 {
        self.head
    }

    /// Oldest sample index still held.
    pub fn oldest(&self) -> u64 {
        self.head.saturating_sub(self.buf.len() as u64)
    }

    pub fn available(&self) -> u64 {
        self.head - self.oldest()
    }

    pub fn write(&mut self, first_sample: u64, samples: Samples<'_>) {
        let cap = self.buf.len() as u64;
        if first_sample > self.head {
            // Zero-fill the gap (at most one ring's worth).
            let gap = (first_sample - self.head).min(cap);
            for i in 0..gap {
                let idx = ((first_sample - gap + i) % cap) as usize;
                self.buf[idx] = [0, 0];
            }
        } else if first_sample < self.head {
            // Out-of-order or replayed data: ignore what we already hold.
            let skip = (self.head - first_sample) as usize;
            if skip >= samples.len() {
                return;
            }
            return self.write(self.head, slice_from(samples, skip));
        }
        let n = samples.len();
        // Only the last `cap` samples of an oversized block can survive.
        let (start, src_off) = if n as u64 > cap { (first_sample + (n as u64 - cap), n - cap as usize) } else { (first_sample, 0) };
        let mut pos = (start % cap) as usize;
        let mut i = src_off;
        while i < n {
            let run = (n - i).min(self.buf.len() - pos);
            let dst = &mut self.buf[pos..pos + run];
            match samples {
                Samples::Ci16(s) => dst.copy_from_slice(&s[i..i + run]),
                Samples::Ci8(s) => {
                    for (d, x) in dst.iter_mut().zip(&s[i..i + run]) {
                        *d = [(x[0] as i16) << 8, (x[1] as i16) << 8];
                    }
                }
                Samples::Cf32(s) => {
                    for (d, x) in dst.iter_mut().zip(&s[i..i + run]) {
                        *d = [to_i16(x.re), to_i16(x.im)];
                    }
                }
            }
            i += run;
            pos = 0;
        }
        self.head = first_sample + n as u64;
    }

    /// Copies `out.len()` samples starting at absolute index `start`, scaled
    /// to ±1.0. Returns false (leaving `out` untouched) if any are not held.
    pub fn read(&self, start: u64, out: &mut [Complex32]) -> bool {
        let end = start + out.len() as u64;
        if start < self.oldest() || end > self.head {
            return false;
        }
        let cap = self.buf.len();
        let mut pos = (start % cap as u64) as usize;
        let mut i = 0;
        const K: f32 = 1.0 / 32768.0;
        while i < out.len() {
            let run = (out.len() - i).min(cap - pos);
            for (o, s) in out[i..i + run].iter_mut().zip(&self.buf[pos..pos + run]) {
                *o = Complex32::new(s[0] as f32 * K, s[1] as f32 * K);
            }
            i += run;
            pos = 0;
        }
        true
    }
}

fn to_i16(v: f32) -> i16 {
    (v * 32767.0).clamp(-32768.0, 32767.0) as i16
}

fn slice_from(s: Samples<'_>, skip: usize) -> Samples<'_> {
    match s {
        Samples::Ci8(x) => Samples::Ci8(&x[skip..]),
        Samples::Ci16(x) => Samples::Ci16(&x[skip..]),
        Samples::Cf32(x) => Samples::Cf32(&x[skip..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: i16, n: usize) -> Vec<[i16; 2]> {
        (0..n).map(|i| [start + i as i16, 0]).collect()
    }

    #[test]
    fn wraps_and_reads_back() {
        let mut r = IqRing::new(10);
        r.write(0, Samples::Ci16(&ramp(0, 7)));
        r.write(7, Samples::Ci16(&ramp(7, 7)));
        assert_eq!(r.head(), 14);
        assert_eq!(r.oldest(), 4);
        let mut out = vec![Complex32::default(); 10];
        assert!(r.read(4, &mut out));
        let got: Vec<i32> = out.iter().map(|c| (c.re * 32768.0).round() as i32).collect();
        assert_eq!(got, (4..14).collect::<Vec<_>>());
        assert!(!r.read(3, &mut out[..1]));
        assert!(!r.read(13, &mut out[..2]));
    }

    #[test]
    fn gap_is_zero_filled() {
        let mut r = IqRing::new(16);
        r.write(0, Samples::Ci16(&ramp(1, 4)));
        r.write(8, Samples::Ci16(&ramp(100, 4)));
        let mut out = vec![Complex32::default(); 12];
        assert!(r.read(0, &mut out));
        assert_eq!(out[5].re, 0.0);
        assert_eq!((out[8].re * 32768.0).round() as i32, 100);
    }

    #[test]
    fn oversized_block_keeps_tail() {
        let mut r = IqRing::new(4);
        r.write(0, Samples::Ci16(&ramp(0, 10)));
        let mut out = vec![Complex32::default(); 4];
        assert!(r.read(6, &mut out));
        assert_eq!((out[0].re * 32768.0).round() as i32, 6);
    }
}
