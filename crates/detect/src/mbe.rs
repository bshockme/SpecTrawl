//! Multi-band excitation speech synthesis, shared by the IMBE (P25 Phase 1)
//! and half-rate (Phase 2, DMR) vocoders: spectral amplitude enhancement,
//! adaptive smoothing and the voiced/unvoiced synthesis of TIA-102.BABA
//! sections 8, 9 and 11. A frame of model parameters in, 20 ms of 8 kHz
//! speech out.

use std::f64::consts::PI;
use std::sync::Arc;

use num_complex::Complex64;
use rustfft::{Fft, FftPlanner};

/// Samples per 20 ms frame at 8 kHz.
pub const N: usize = 160;
/// Most harmonics a frame can have, plus one (harmonics count from 1).
pub const MAX_L: usize = 57;

/// One frame's model parameters, after decoding.
#[derive(Debug, Clone)]
pub struct Model {
    /// Fundamental frequency, radians per sample.
    pub w0: f64,
    /// Harmonics, 1..=L used.
    pub l: usize,
    /// Voiced per harmonic (index 1..=L).
    pub voiced: [bool; MAX_L],
    /// Spectral amplitudes (index 1..=L).
    pub m: [f64; MAX_L],
}

impl Model {
    /// Annex A's initial state.
    pub fn initial() -> Self {
        Model { w0: 0.02985 * PI, l: 30, voiced: [false; MAX_L], m: [0.0; MAX_L] }
    }
}

/// How bad the channel is, for smoothing (BABA 7.6).
#[derive(Debug, Clone, Copy)]
pub struct Errors {
    /// εT, bits corrected in the current frame.
    pub total: u32,
    /// ε4, bits corrected in the first Hamming word.
    pub e4: u32,
    /// εR, the running error rate.
    pub rate: f64,
}

/// Section 8 and 9 state.
pub struct Enhancer {
    /// SE, the local energy.
    se: f64,
    /// τM, the amplitude threshold.
    tau: f64,
}

impl Default for Enhancer {
    fn default() -> Self {
        Enhancer { se: 75000.0, tau: 20480.0 }
    }
}

impl Enhancer {
    /// Enhances and smooths the decoded amplitudes `m.m` (unenhanced, as
    /// the predictor needs them) into what synthesis uses.
    pub fn apply(&mut self, m: &Model, err: Errors) -> Model {
        let (w0, l) = (m.w0, m.l);
        let mut out = m.clone();
        // Section 8: spectral weights from the frame's autocorrelation.
        let (mut r0, mut r1) = (0.0, 0.0);
        for k in 1..=l {
            r0 += m.m[k] * m.m[k];
            r1 += m.m[k] * m.m[k] * (w0 * k as f64).cos();
        }
        let den = w0 * r0 * (r0 * r0 - r1 * r1);
        for k in 1..=l {
            if 8 * k <= l || den <= 0.0 {
                continue;
            }
            let num = 0.96 * PI * (r0 * r0 + r1 * r1 - 2.0 * r0 * r1 * (w0 * k as f64).cos());
            let w = m.m[k].sqrt() * (num / den).max(0.0).powf(0.25);
            out.m[k] = m.m[k] * w.clamp(0.5, 1.2);
        }
        let sum2: f64 = (1..=l).map(|k| out.m[k] * out.m[k]).sum();
        if sum2 > 0.0 {
            let g = (r0 / sum2).sqrt();
            for k in 1..=l {
                out.m[k] *= g;
            }
        }
        self.se = (0.95 * self.se + 0.05 * r0).max(10000.0);
        // Section 9: V/UV smoothing...
        let vm = if err.rate <= 0.005 && err.total <= 4 {
            f64::INFINITY
        } else if err.rate <= 0.0125 && err.e4 == 0 {
            45.255 * self.se.powf(0.375) / (277.26 * err.rate).exp()
        } else {
            1.414 * self.se.powf(0.375)
        };
        for k in 1..=l {
            if out.m[k] > vm {
                out.voiced[k] = true;
            }
        }
        // ...and amplitude smoothing.
        let am: f64 = (1..=l).map(|k| out.m[k]).sum();
        self.tau = if err.rate <= 0.005 && err.total <= 6 { 20480.0 } else { 6000.0 - 300.0 * err.total as f64 + self.tau };
        if self.tau <= am && am > 0.0 {
            let g = self.tau / am;
            for k in 1..=l {
                out.m[k] *= g;
            }
        }
        out
    }
}

/// BABA Annex I: the synthesis window, a trapezoid over −105..=105.
fn ws(n: i64) -> f64 {
    ((105 - n.abs()) as f64 / 50.0).clamp(0.0, 1.0)
}

/// γw (BABA eq. 121) from the Annex C and I windows.
const GAMMA_W: f64 = 146.643;

/// Section 11 state: the previous frame and the noise generator.
pub struct Synth {
    prev: Model,
    psi: [f64; MAX_L],
    phi: [f64; MAX_L],
    /// u(n) for n = −105..=105 of the current frame.
    noise: Vec<f64>,
    /// ũw(n, −1) for n = −128..=127, at index n + 128.
    uw_prev: Vec<f64>,
    fft: Arc<dyn Fft<f64>>,
    ifft: Arc<dyn Fft<f64>>,
}

impl Default for Synth {
    fn default() -> Self {
        let mut p = FftPlanner::new();
        let mut noise = vec![3147.0];
        while noise.len() < 211 {
            noise.push(next_noise(*noise.last().unwrap()));
        }
        Synth {
            prev: Model::initial(),
            psi: [0.0; MAX_L],
            phi: [0.0; MAX_L],
            noise,
            uw_prev: vec![0.0; 256],
            fft: p.plan_fft_forward(256),
            ifft: p.plan_fft_inverse(256),
        }
    }
}

/// BABA eq. 117.
fn next_noise(u: f64) -> f64 {
    (171.0 * u + 11213.0) % 53125.0
}

impl Synth {
    /// Moves the noise sequence on one frame.
    fn shift_noise(&mut self) {
        self.shift_noise_by(N);
    }

    /// u(n) for the current frame.
    fn u(&self, n: i64) -> f64 {
        self.noise[(n + 105) as usize]
    }

    /// 20 ms of speech (16-bit scale) for the frame's enhanced parameters.
    pub fn synth(&mut self, cur: &Model) -> [f64; N] {
        self.shift_noise();
        let prev = std::mem::replace(&mut self.prev, cur.clone());
        let phi_prev = self.phi;
        // Phases (eq. 139-141): ψ advances every frame for every harmonic;
        // above L/4, φ is ψ plus noise in proportion to how unvoiced the
        // frame is.
        let luv = (1..=cur.l).filter(|&k| !cur.voiced[k]).count() as f64;
        for k in 1..MAX_L {
            self.psi[k] = wrap(self.psi[k] + (prev.w0 + cur.w0) * (k * N) as f64 / 2.0);
            self.phi[k] = if k <= cur.l / 4 {
                self.psi[k]
            } else {
                let rho = 2.0 * PI / 53125.0 * self.u(k as i64) - PI;
                self.psi[k] + luv * rho / cur.l as f64
            };
        }
        let mut out = self.unvoiced(cur);
        self.voiced(&prev, cur, &phi_prev, &mut out);
        out
    }

    /// Section 11.2: noise shaped to the unvoiced harmonics' amplitudes,
    /// overlap-added with the previous frame's.
    fn unvoiced(&mut self, cur: &Model) -> [f64; N] {
        let mut buf = vec![Complex64::new(0.0, 0.0); 256];
        for n in -104i64..=104 {
            buf[n.rem_euclid(256) as usize] = Complex64::new(self.u(n) * ws(n), 0.0);
        }
        self.fft.process(&mut buf);
        let at = |m: i64| m.rem_euclid(256) as usize;
        let edge = |k: f64| (256.0 / (2.0 * PI) * k * cur.w0).ceil() as i64;
        let mut shaped = vec![Complex64::new(0.0, 0.0); 256];
        for k in 1..=cur.l {
            if cur.voiced[k] {
                continue;
            }
            let (a, b) = (edge(k as f64 - 0.5), edge(k as f64 + 0.5).min(129));
            if b <= a {
                continue;
            }
            let power: f64 = (a..b).map(|m| buf[at(m)].norm_sqr()).sum::<f64>() / (b - a) as f64;
            if power <= 0.0 {
                continue;
            }
            let g = GAMMA_W * cur.m[k] / power.sqrt();
            for m in a..b {
                shaped[at(m)] = buf[at(m)] * g;
                shaped[at(-m)] = buf[at(-m)] * g;
            }
        }
        self.ifft.process(&mut shaped);
        // ũw(n) for n = −128..=127 at index n + 128.
        let uw: Vec<f64> = (-128i64..128).map(|n| shaped[at(n)].re / 256.0).collect();
        let mut out = [0.0; N];
        for (n, o) in out.iter_mut().enumerate() {
            let n = n as i64;
            let (w1, w2) = (ws(n), ws(n - N as i64));
            let prev = if n < 128 { self.uw_prev[(n + 128) as usize] } else { 0.0 };
            let curv = if n - (N as i64) >= -128 { uw[(n - N as i64 + 128) as usize] } else { 0.0 };
            let den = w1 * w1 + w2 * w2;
            if den > 0.0 {
                *o = (w1 * prev + w2 * curv) / den;
            }
        }
        self.uw_prev = uw;
        out
    }

    /// Section 11.3: a sinusoid per voiced harmonic, matched to the
    /// previous frame's.
    fn voiced(&self, prev: &Model, cur: &Model, phi_prev: &[f64; MAX_L], out: &mut [f64; N]) {
        let nn = N as f64;
        let smooth_pitch = (cur.w0 - prev.w0).abs() < 0.1 * cur.w0;
        for k in 1..=prev.l.max(cur.l) {
            let kf = k as f64;
            let (v0, m0) = if k <= cur.l { (cur.voiced[k], cur.m[k]) } else { (false, 0.0) };
            let (v1, m1) = if k <= prev.l { (prev.voiced[k], prev.m[k]) } else { (false, 0.0) };
            let (p0, p1) = (self.phi[k], phi_prev[k]);
            let fade_out = |n: f64| ws(n as i64) * m1 * (prev.w0 * n * kf + p1).cos();
            let fade_in = |n: f64| ws(n as i64 - N as i64) * m0 * (cur.w0 * (n - nn) * kf + p0).cos();
            match (v1, v0) {
                (false, false) => {}
                (true, false) => (0..N).for_each(|n| out[n] += 2.0 * fade_out(n as f64)),
                (false, true) => (0..N).for_each(|n| out[n] += 2.0 * fade_in(n as f64)),
                (true, true) if k >= 8 || !smooth_pitch => (0..N).for_each(|n| out[n] += 2.0 * (fade_out(n as f64) + fade_in(n as f64))),
                (true, true) => {
                    // Interpolated amplitude and phase (eq. 134-138).
                    let dphi = p0 - p1 - (prev.w0 + cur.w0) * kf * nn / 2.0;
                    let dw = (dphi - 2.0 * PI * ((dphi + PI) / (2.0 * PI)).floor()) / nn;
                    for (n, o) in out.iter_mut().enumerate() {
                        let n = n as f64;
                        let a = m1 + n / nn * (m0 - m1);
                        let th = p1 + (prev.w0 * kf + dw) * n + (cur.w0 - prev.w0) * kf * n * n / (2.0 * nn);
                        *o += 2.0 * a * th.cos();
                    }
                }
            }
        }
    }

    /// Comfort noise for a muted frame (BABA 7.8): uniform in [−5, 5].
    pub fn mute(&mut self) -> [f64; N] {
        let mut out = [0.0; N];
        for o in out.iter_mut() {
            self.shift_noise_by(1);
            *o = self.u(105) / 53125.0 * 10.0 - 5.0;
        }
        out
    }

    fn shift_noise_by(&mut self, k: usize) {
        self.noise.drain(..k);
        while self.noise.len() < 211 {
            let u = next_noise(*self.noise.last().unwrap());
            self.noise.push(u);
        }
    }
}

fn wrap(x: f64) -> f64 {
    x - 2.0 * PI * (x / (2.0 * PI)).floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steady voiced frame synthesizes its harmonics: here a 200 Hz
    /// fundamental with only harmonic 2 (400 Hz) present.
    #[test]
    fn steady_voiced_frame_is_its_harmonic() {
        let mut m = Model { w0: 2.0 * PI * 200.0 / 8000.0, l: 19, voiced: [true; MAX_L], m: [0.0; MAX_L] };
        m.m[2] = 1000.0;
        let mut s = Synth::default();
        let mut out = Vec::new();
        for _ in 0..10 {
            out.extend(s.synth(&m));
        }
        let x = &out[800..];
        let power = |f: f64| {
            let (mut re, mut im) = (0.0, 0.0);
            for (n, v) in x.iter().enumerate() {
                let a = 2.0 * PI * f * n as f64 / 8000.0;
                re += v * a.cos();
                im += v * a.sin();
            }
            (re * re + im * im).sqrt() / x.len() as f64
        };
        // 2·M cos(...) has amplitude 2000, so a one-sided DFT magnitude of 1000.
        assert!((power(400.0) - 1000.0).abs() < 50.0, "{}", power(400.0));
        assert!(power(200.0) < 20.0 && power(600.0) < 20.0);
    }
}
