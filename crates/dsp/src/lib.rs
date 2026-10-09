//! DSP building blocks. For now: a windowed power-spectrum analyzer that
//! accumulates many FFTs per output frame (linear-power averaging).

use std::sync::Arc;

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// 4-term Blackman-Harris: ~92 dB sidelobes, good for wide dynamic range.
pub fn blackman_harris(n: usize) -> Vec<f32> {
    let (a0, a1, a2, a3) = (0.35875f64, 0.48829, 0.14128, 0.01168);
    let m = n as f64;
    (0..n)
        .map(|i| {
            let x = 2.0 * std::f64::consts::PI * i as f64 / m;
            (a0 - a1 * x.cos() + a2 * (2.0 * x).cos() - a3 * (3.0 * x).cos()) as f32
        })
        .collect()
}

pub struct SpectrumAnalyzer {
    size: usize,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    acc: Vec<f32>,
    count: u32,
    /// dB offset so a full-scale complex sinusoid reads ~0 dBFS.
    norm_db: f32,
}

impl SpectrumAnalyzer {
    pub fn new(size: usize) -> Self {
        let window = blackman_harris(size);
        let coherent_gain: f32 = window.iter().sum::<f32>();
        let fft = FftPlanner::new().plan_fft_forward(size);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Self {
            size,
            fft,
            buf: vec![Complex32::default(); size],
            scratch,
            acc: vec![0.0; size],
            count: 0,
            norm_db: -20.0 * coherent_gain.log10(),
            window,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Adds one FFT of `input` (exactly `size` samples) to the accumulator.
    pub fn push(&mut self, input: &[Complex32]) {
        assert_eq!(input.len(), self.size);
        for ((b, x), w) in self.buf.iter_mut().zip(input).zip(&self.window) {
            *b = x * w;
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        for (a, b) in self.acc.iter_mut().zip(&self.buf) {
            *a += b.norm_sqr();
        }
        self.count += 1;
    }

    pub fn pending(&self) -> u32 {
        self.count
    }

    /// Writes the averaged spectrum in dBFS, DC-centred (negative freqs first),
    /// and resets the accumulator. Returns false if nothing was accumulated.
    pub fn take_db(&mut self, out: &mut Vec<f32>) -> bool {
        if self.count == 0 {
            return false;
        }
        out.resize(self.size, 0.0);
        let half = self.size / 2;
        let inv = 1.0 / self.count as f32;
        for (i, o) in out.iter_mut().enumerate() {
            let p = self.acc[(i + half) % self.size] * inv;
            *o = 10.0 * (p + 1e-20).log10() + self.norm_db;
        }
        self.acc.iter_mut().for_each(|a| *a = 0.0);
        self.count = 0;
        true
    }
}

/// Max-per-bucket decimation so narrow peaks survive when there are more bins
/// than output columns.
pub fn decimate_max(input: &[f32], out: &mut [f32]) {
    let n = input.len();
    let m = out.len();
    if m == 0 || n == 0 {
        return;
    }
    for (j, o) in out.iter_mut().enumerate() {
        let a = j * n / m;
        let b = ((j + 1) * n / m).max(a + 1).min(n);
        *o = input[a..b].iter().copied().fold(f32::MIN, f32::max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_lands_in_right_bin_at_0dbfs() {
        let n = 1024;
        let k = 100; // bin offset from DC
        let tone: Vec<Complex32> =
            (0..n).map(|i| Complex32::from_polar(1.0, 2.0 * std::f32::consts::PI * k as f32 * i as f32 / n as f32)).collect();
        let mut sa = SpectrumAnalyzer::new(n);
        sa.push(&tone);
        let mut out = Vec::new();
        assert!(sa.take_db(&mut out));
        let (peak, val) = out.iter().enumerate().fold((0, f32::MIN), |m, (i, &v)| if v > m.1 { (i, v) } else { m });
        assert_eq!(peak, n / 2 + k);
        assert!(val.abs() < 0.5, "peak {val} dBFS");
    }

    #[test]
    fn decimate_keeps_peaks() {
        let mut input = vec![-100.0; 1000];
        input[537] = 0.0;
        let mut out = vec![0.0; 100];
        decimate_max(&input, &mut out);
        assert_eq!(out[53], 0.0);
    }
}
