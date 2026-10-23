//! One shared forward FFT per source, plus Blackman-Harris-windowed power
//! derived from the same unwindowed bins.

use std::sync::Arc;

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Coefficients of the periodic 4-term Blackman-Harris window (the ones
/// `blackman_harris` uses).
pub const BH: [f64; 4] = [0.35875, 0.48829, 0.14128, 0.01168];

/// A forward FFT planned once, with its scratch, so `forward` never allocates.
pub struct SharedFft {
    size: usize,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl SharedFft {
    pub fn new(n: usize) -> SharedFft {
        let fft = FftPlanner::new().plan_fft_forward(n);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        SharedFft { size: n, fft, scratch }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Unwindowed, unnormalised FFT of `input` into `bins` (natural order,
    /// DC at index 0). Both slices must be exactly `size()` long.
    pub fn forward(&mut self, input: &[Complex32], bins: &mut [Complex32]) {
        assert_eq!(input.len(), self.size, "forward: input length must equal FFT size");
        assert_eq!(bins.len(), self.size, "forward: bins length must equal FFT size");
        bins.copy_from_slice(input);
        self.fft.process_with_scratch(bins, &mut self.scratch);
    }
}

/// Linear power of the periodic Blackman-Harris-windowed spectrum, computed
/// from unwindowed `bins` by a 7-tap convolution (indices mod N), DC-centred
/// (negative frequencies first), a full-scale complex sinusoid reading 1.0.
pub fn bh_power(bins: &[Complex32], out: &mut [f32]) {
    let n = bins.len();
    assert_eq!(out.len(), n, "bh_power: out length must equal bins length");
    if n == 0 {
        return;
    }
    let [a0, a1, a2, a3] = BH.map(|a| a as f32);
    let (h1, h2, h3) = (-a1 / 2.0, a2 / 2.0, -a3 / 2.0);
    let norm = 1.0 / (n as f32 * a0).powi(2);
    let half = n / 2;
    for (i, o) in out.iter_mut().enumerate() {
        let k = (i + half) % n;
        // Offsets are reduced mod n so tiny n still wraps correctly.
        let at = |d: usize, sign: bool| {
            let d = d % n;
            bins[if sign { (k + d) % n } else { (k + n - d) % n }]
        };
        let xw = bins[k] * a0 + (at(1, false) + at(1, true)) * h1 + (at(2, false) + at(2, true)) * h2 + (at(3, false) + at(3, true)) * h3;
        *o = xw.norm_sqr() * norm;
    }
}

#[cfg(test)]
mod tests {
    use num_complex::Complex32;

    use crate::SpectrumAnalyzer;

    use super::*;

    #[test]
    fn bh_power_matches_a_windowed_fft_bin_for_bin() {
        let n = 4096;
        let mut rng = 0x1234_5678_9abc_def0u64;
        let noise = |rng: &mut u64| {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            (*rng >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        let input: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f32 / n as f32;
                Complex32::from_polar(0.5, 2.0 * std::f32::consts::PI * 100.0 * t)
                    + Complex32::from_polar(0.1, 2.0 * std::f32::consts::PI * (-700.3) * t)
                    + Complex32::new(noise(&mut rng), noise(&mut rng)) * 1e-3
            })
            .collect();
        let mut reference = SpectrumAnalyzer::new(n);
        reference.push(&input);
        let mut want = Vec::new();
        reference.take_power(&mut want);
        let mut fft = SharedFft::new(n);
        let mut bins = vec![Complex32::default(); n];
        fft.forward(&input, &mut bins);
        let mut got = vec![0.0f32; n];
        bh_power(&bins, &mut got);
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            if *w > 1e-12 {
                assert!((g - w).abs() < 1e-3 * w + 1e-9, "bin {i}: {g} vs {w}");
            }
        }
        assert!((got[n / 2 + 100] - 0.25).abs() < 0.01, "0.5 amplitude tone reads 0.25 power");
    }
}
