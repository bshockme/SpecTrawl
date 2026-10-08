//! Streaming digital down-converter: shift a channel to DC, low-pass it, and
//! decimate. Pulls one narrowband channel out of the look-back ring.
//!
//! Two stages when the decimation is large: a 2nd-order CIC (integrate/comb,
//! a few adds per input sample) brings the rate down to ~8x the output, then a
//! short windowed-sinc FIR finishes the filtering and decimates by 8. The FIR
//! is evaluated only at output points.

use num_complex::{Complex, Complex32};

/// FIR decimation in the second stage.
const STAGE2: usize = 8;

pub struct Ddc {
    taps: Vec<f32>,
    /// FIR decimation (second stage, or the only stage).
    decim: usize,
    /// FIR input not yet fully consumed.
    buf: Vec<Complex32>,
    next: usize,
    osc: Complex32,
    step: Complex32,
    mixed: u64,
    cic: Option<Cic>,
    pub fs_out: f64,
}

/// 2nd-order CIC decimator. Integrators run in f64: their sums grow without
/// bound and only differences matter.
struct Cic {
    r: usize,
    phase: usize,
    i1: Complex<f64>,
    i2: Complex<f64>,
    c1: Complex<f64>,
    c2: Complex<f64>,
    gain: f64,
}

impl Cic {
    fn new(r: usize) -> Self {
        let z = Complex::new(0.0, 0.0);
        Self { r, phase: 0, i1: z, i2: z, c1: z, c2: z, gain: 1.0 / (r * r) as f64 }
    }

    #[inline]
    fn push(&mut self, x: Complex32) -> Option<Complex32> {
        self.i1 += Complex::new(x.re as f64, x.im as f64);
        self.i2 += self.i1;
        self.phase += 1;
        if self.phase < self.r {
            return None;
        }
        self.phase = 0;
        let d1 = self.i2 - self.c1;
        self.c1 = self.i2;
        let y = d1 - self.c2;
        self.c2 = d1;
        let y = y * self.gain;
        Some(Complex32::new(y.re as f32, y.im as f32))
    }
}

impl Ddc {
    /// `offset_hz`: channel centre relative to the input's centre.
    /// `cutoff_hz`: one-sided low-pass cutoff. `target_fs`: desired output
    /// rate (actual = fs / integer decimation, at least `target_fs`).
    pub fn new(fs: f64, offset_hz: f64, cutoff_hz: f64, target_fs: f64) -> Self {
        let total = ((fs / target_fs).floor() as usize).max(1);
        // Use the CIC when it can take at least a factor of 4 off first.
        let (cic, fir_decim) = if total >= STAGE2 * 4 { (Some(total / STAGE2), STAGE2) } else { (None, total) };
        let fs_fir = fs / cic.unwrap_or(1) as f64;
        let fs_out = fs_fir / fir_decim as f64;
        let cutoff = cutoff_hz.min(fs_out * 0.45);
        // After the CIC the FIR runs at only ~8x the output rate, so it can
        // afford a long, sharp filter (adjacent channels can be 12.5 kHz away).
        let n = if cic.is_some() { 24 * fir_decim + 1 } else { (8 * fir_decim + 1).max(31) | 1 };
        let fc = cutoff / fs_fir;
        let m = (n - 1) as f64;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let x = i as f64 - m / 2.0;
                let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
                // Blackman window.
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos() + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / m).cos();
                (sinc * w) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        let w = -2.0 * std::f64::consts::PI * offset_hz / fs;
        Self {
            taps,
            decim: fir_decim,
            buf: Vec::new(),
            next: 0,
            osc: Complex32::new(1.0, 0.0),
            step: Complex32::new(w.cos() as f32, w.sin() as f32),
            mixed: 0,
            cic: cic.map(Cic::new),
            fs_out,
        }
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.buf.reserve(input.len() / self.cic.as_ref().map_or(1, |c| c.r) + 1);
        for &x in input {
            let m = x * self.osc;
            self.osc *= self.step;
            self.mixed += 1;
            if self.mixed % 4096 == 0 {
                self.osc /= self.osc.norm();
            }
            match &mut self.cic {
                Some(c) => {
                    if let Some(y) = c.push(m) {
                        self.buf.push(y);
                    }
                }
                None => self.buf.push(m),
            }
        }
        let n = self.taps.len();
        while self.next + n <= self.buf.len() {
            let win = &self.buf[self.next..self.next + n];
            let mut acc = Complex32::default();
            for (x, &t) in win.iter().zip(&self.taps) {
                acc += x * t;
            }
            out.push(acc);
            self.next += self.decim;
        }
        // Drop what no future output needs.
        if self.next > 0 {
            self.buf.drain(..self.next);
            self.next = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, fs: f64, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let p = 2.0 * std::f64::consts::PI * f * i as f64 / fs;
                Complex32::new(p.cos() as f32, p.sin() as f32)
            })
            .collect()
    }

    fn check(fs: f64, wanted_hz: f64, other_hz: f64, cutoff: f64, target: f64, n: usize) {
        let input: Vec<Complex32> = tone(wanted_hz, fs, n).iter().zip(&tone(other_hz, fs, n)).map(|(a, b)| a + b).collect();
        let mut d = Ddc::new(fs, wanted_hz, cutoff, target);
        let mut out = Vec::new();
        for chunk in input.chunks(7777) {
            d.process(chunk, &mut out);
        }
        let tail = &out[out.len() / 2..];
        let mean: Complex32 = tail.iter().sum::<Complex32>() / tail.len() as f32;
        // The wanted tone lands at DC with unit amplitude; the neighbour is gone.
        assert!((mean.norm() - 1.0).abs() < 0.02, "gain {}", mean.norm());
        let ripple = tail.iter().map(|x| (x - mean).norm()).fold(0.0f32, f32::max);
        assert!(ripple < 0.01, "neighbour leaks: {ripple}");
    }

    #[test]
    fn single_stage_shifts_and_rejects() {
        check(1e6, 100e3, 150e3, 8e3, 25e3, 200_000);
    }

    #[test]
    fn two_stage_shifts_and_rejects_including_cic_alias_region() {
        // 10 MS/s → 48 kS/s uses the CIC (decimation 208 = 26 × 8). The
        // neighbour sits right where the CIC's first alias would fold
        // (fs/26 ≈ 385 kHz away), so this exercises CIC + FIR rejection.
        let fs = 10e6;
        check(fs, 1.0e6, 1.0e6 + fs / 26.0 + 3e3, 8e3, 48e3, 2_000_000);
        check(fs, -2.0e6, -2.0e6 + 20e3, 8e3, 48e3, 2_000_000);
    }
}
