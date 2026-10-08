//! Streaming digital down-converter: shift a channel to DC, low-pass it, and
//! decimate. Single-stage windowed-sinc FIR evaluated only at output points.
//! Good enough for pulling one narrowband channel out of the look-back ring.

use num_complex::Complex32;

pub struct Ddc {
    taps: Vec<f32>,
    decim: usize,
    /// Mixed input not yet fully consumed (FIR history + pending samples).
    buf: Vec<Complex32>,
    /// Index into `buf` of the next output's first tap.
    next: usize,
    osc: Complex32,
    step: Complex32,
    mixed: u64,
    pub fs_out: f64,
}

impl Ddc {
    /// `offset_hz`: channel centre relative to the input's centre.
    /// `cutoff_hz`: one-sided low-pass cutoff. `target_fs`: desired output
    /// rate (actual = fs / integer decimation, at least `target_fs`).
    pub fn new(fs: f64, offset_hz: f64, cutoff_hz: f64, target_fs: f64) -> Self {
        let decim = ((fs / target_fs).floor() as usize).max(1);
        let fs_out = fs / decim as f64;
        let cutoff = cutoff_hz.min(fs_out * 0.45);
        let n = (8 * decim + 1).max(31) | 1;
        let fc = cutoff / fs;
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
            decim,
            buf: Vec::new(),
            next: 0,
            osc: Complex32::new(1.0, 0.0),
            step: Complex32::new(w.cos() as f32, w.sin() as f32),
            mixed: 0,
            fs_out,
        }
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.buf.reserve(input.len());
        for &x in input {
            self.buf.push(x * self.osc);
            self.osc *= self.step;
            self.mixed += 1;
            if self.mixed % 4096 == 0 {
                self.osc /= self.osc.norm();
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

    #[test]
    fn shifts_wanted_tone_to_dc_and_rejects_neighbour() {
        let fs = 1e6;
        let tone = |f: f64, n: usize| -> Vec<Complex32> {
            (0..n).map(|i| {
                let p = 2.0 * std::f64::consts::PI * f * i as f64 / fs;
                Complex32::new(p.cos() as f32, p.sin() as f32)
            }).collect()
        };
        let n = 200_000;
        let wanted = tone(100e3, n);
        let other = tone(150e3, n);
        let input: Vec<Complex32> = wanted.iter().zip(&other).map(|(a, b)| a + b).collect();
        let mut d = Ddc::new(fs, 100e3, 8e3, 25e3);
        let mut out = Vec::new();
        for chunk in input.chunks(7777) {
            d.process(chunk, &mut out);
        }
        let tail = &out[out.len() / 2..];
        let mean: Complex32 = tail.iter().sum::<Complex32>() / tail.len() as f32;
        // The wanted tone lands at DC with unit amplitude; the neighbour is gone.
        assert!((mean.norm() - 1.0).abs() < 0.02, "{}", mean.norm());
        let ripple = tail.iter().map(|x| (x - mean).norm()).fold(0.0f32, f32::max);
        assert!(ripple < 0.01, "{ripple}");
    }
}
