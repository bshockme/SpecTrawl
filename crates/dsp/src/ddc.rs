//! Streaming digital down-converter: shift a channel to DC, low-pass it, and
//! decimate. Pulls one narrowband channel out of the look-back ring.
//!
//! Two stages when the decimation is large: a 2nd-order CIC (integrate/comb,
//! a few adds per input sample) brings the rate down to ~8x the output, then a
//! short windowed-sinc FIR finishes the filtering and decimates by 8. The FIR
//! is evaluated only at output points.

use num_complex::Complex32;

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

/// 2nd-order CIC decimator in wrapping fixed point.
///
/// The integrators grow without bound, so they run in two's-complement i64
/// and are allowed to wrap: the comb's differences are exact as long as the
/// *output* fits in 64 bits, which the scale guarantees (Hogenauer's
/// register-width argument). A floating-point integrator has no such
/// property: after a few minutes of continuous input it loses the small
/// differences that are the signal and the output decays into noise.
struct Cic {
    r: usize,
    phase: usize,
    i1: [i64; 2],
    i2: [i64; 2],
    c1: [i64; 2],
    c2: [i64; 2],
    /// Input samples are scaled by this before integration.
    scale: f64,
    /// Output: 1 / (scale × R²).
    gain: f64,
}

impl Cic {
    fn new(r: usize) -> Self {
        // The output is at most R² × scale × |x|. Give |x| 16× headroom over
        // full scale and the rest of the 63 bits to the fraction.
        let growth = 2 * (usize::BITS - (r - 1).leading_zeros()) as i32;
        let scale = 2f64.powi(62 - growth - 4);
        Self { r, phase: 0, i1: [0; 2], i2: [0; 2], c1: [0; 2], c2: [0; 2], scale, gain: 1.0 / (scale * (r * r) as f64) }
    }

    #[inline]
    fn push(&mut self, x: Complex32) -> Option<Complex32> {
        let x = [(x.re as f64 * self.scale) as i64, (x.im as f64 * self.scale) as i64];
        for ((i1, i2), x) in self.i1.iter_mut().zip(&mut self.i2).zip(x) {
            *i1 = i1.wrapping_add(x);
            *i2 = i2.wrapping_add(*i1);
        }
        self.phase += 1;
        if self.phase < self.r {
            return None;
        }
        self.phase = 0;
        let mut y = [0f32; 2];
        for (((i2, c1), c2), y) in self.i2.iter().zip(&mut self.c1).zip(&mut self.c2).zip(&mut y) {
            let d1 = i2.wrapping_sub(*c1);
            *c1 = *i2;
            let d2 = d1.wrapping_sub(*c2);
            *c2 = d1;
            *y = (d2 as f64 * self.gain) as f32;
        }
        Some(Complex32::new(y[0], y[1]))
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
        let taps = crate::fir::windowed_sinc(n, cutoff, fs_fir);
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

    /// The FIR's delay in its input samples: output k is centred on FIR input `k × decimation + delay`. Without a CIC
    /// (`target_fs` = `fs`, say) that is input sample `k + delay`.
    pub fn delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }

    /// Forgets the samples in flight (the oscillator keeps its phase):
    /// for a discontinuity in the input, so the filters don't smear the
    /// old stream into the new one.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.next = 0;
        if let Some(c) = &mut self.cic {
            *c = Cic::new(c.r);
        }
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.buf.reserve(input.len() / self.cic.as_ref().map_or(1, |c| c.r) + 1);
        for &x in input {
            let m = x * self.osc;
            self.osc *= self.step;
            self.mixed += 1;
            if self.mixed.is_multiple_of(4096) {
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

    /// The integrators wrap every few hundred samples of a full-scale
    /// input; after 10 s of constant input (thousands of wraps) the output
    /// must still be the input exactly. (The review reproduced the old f64
    /// integrators decaying to noise after ~300 s.)
    #[test]
    fn cic_output_is_exact_after_integrator_wraps() {
        let mut c = Cic::new(26);
        let x = Complex32::new(0.75, -0.25);
        let mut worst = 0f32;
        for i in 0..100_000_000u64 {
            if let Some(y) = c.push(x) {
                // Let the two combs fill (two outputs), then every output is exact.
                if i > 3 * 26 {
                    worst = worst.max((y - x).norm());
                }
            }
        }
        assert!(worst < 1e-6, "worst error {worst}");
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
