//! Analog demodulators: channel IQ in, 48 kHz mono audio out.
//!
//! - NFM: discriminator, voice-band low-pass, noise squelch (a carrier
//!   quietens the discriminator's high-frequency noise).
//! - AM: envelope, DC removed, slow AGC, carrier squelch (phase coherence
//!   of the channel across a short lag: a carrier has it, noise does not).
//! - WFM: discriminator, 75 µs de-emphasis (US broadcast), 15 kHz low-pass.
//!
//! Each keeps its state across calls so a stream can be fed in chunks.

use num_complex::Complex32;

pub const AUDIO_RATE: f64 = 48_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Nfm,
    Am,
    Wfm,
    /// P25 Phase 1 digital voice: decoded by `p25_voice`, not `Demod`
    /// (which treats it as NFM).
    P25,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Nfm => "NFM",
            Mode::Am => "AM",
            Mode::Wfm => "WFM",
            Mode::P25 => "P25",
        }
    }

    /// Channel low-pass cutoff (one-sided) and the IQ rate to run at.
    pub fn channel(self) -> (f64, f64) {
        match self {
            Mode::Nfm => (7_000.0, 48_000.0),
            Mode::Am => (5_000.0, 48_000.0),
            Mode::Wfm => (100_000.0, 240_000.0),
            Mode::P25 => (6_500.0, 48_000.0),
        }
    }

    /// Audio bandwidth.
    fn audio_hz(self) -> f64 {
        match self {
            Mode::Nfm | Mode::P25 => 3_500.0,
            Mode::Am => 4_500.0,
            Mode::Wfm => 13_000.0,
        }
    }

    /// Audio high-pass: removes the DC an off-centre carrier puts on the
    /// discriminator, and for NFM the sub-audible CTCSS tones.
    fn highpass_hz(self) -> f64 {
        match self {
            Mode::Nfm | Mode::P25 => 300.0,
            Mode::Am => 30.0,
            Mode::Wfm => 20.0,
        }
    }

    /// Discriminator Hz that maps to full scale.
    fn deviation(self) -> f32 {
        match self {
            Mode::Wfm => 75_000.0,
            _ => 5_000.0,
        }
    }
}

/// One-pole IIR low-pass.
#[derive(Debug, Clone, Copy)]
struct OnePole {
    a: f32,
    y: f32,
}

impl OnePole {
    fn new(cutoff: f64, fs: f64) -> Self {
        Self { a: (1.0 - (-2.0 * std::f64::consts::PI * cutoff / fs).exp()) as f32, y: 0.0 }
    }
    #[inline]
    fn push(&mut self, x: f32) -> f32 {
        self.y += self.a * (x - self.y);
        self.y
    }
}

/// Biquad low-pass (RBJ cookbook, Q = 0.707), in cascade for steeper skirts.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn lowpass(cutoff: f64, fs: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * (cutoff / fs).min(0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b = [(1.0 - c) / 2.0 / a0, (1.0 - c) / a0, (1.0 - c) / 2.0 / a0];
        Self { b: b.map(|v| v as f32), a: [(-2.0 * c / a0) as f32, ((1.0 - alpha) / a0) as f32], z: [0.0; 2] }
    }
    fn highpass(cutoff: f64, fs: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * (cutoff / fs).min(0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b = [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0];
        Self { b: b.map(|v| v as f32), a: [(-2.0 * c / a0) as f32, ((1.0 - alpha) / a0) as f32], z: [0.0; 2] }
    }
    #[inline]
    fn push(&mut self, x: f32) -> f32 {
        // Transposed direct form II.
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

pub struct Demod {
    pub mode: Mode,
    fs: f64,
    prev: Complex32,
    /// Audio low-pass (four biquads) at the IQ rate, before resampling: WFM
    /// must lose its 19 kHz stereo pilot and 23-53 kHz subcarrier before they
    /// fold into the audio.
    lp: [Biquad; 4],
    hp: Biquad,
    deemph: OnePole,
    /// AM: DC (carrier) removal and AGC level.
    dc: OnePole,
    agc: f32,
    /// NFM noise squelch: high-passed discriminator power vs its total.
    noise_hp: Biquad,
    noise: OnePole,
    level: OnePole,
    /// Squelch threshold on noise/level (0 = always open).
    pub squelch: f32,
    open: bool,
    /// AM carrier squelch: the channel `lag.len()` samples ago, and the
    /// smoothed unit phasor of the phase advance over that lag.
    lag: Vec<Complex32>,
    lag_pos: usize,
    coh: [OnePole; 2],
    coherence: f32,
    /// Resampler: position of the next output in input samples.
    pos: f64,
    last: f32,
}

impl Demod {
    /// `fs`: rate of the channel IQ fed to `process`.
    pub fn new(mode: Mode, fs: f64) -> Self {
        Self {
            mode,
            fs,
            prev: Complex32::new(1.0, 0.0),
            lp: [Biquad::lowpass(mode.audio_hz(), fs); 4],
            hp: Biquad::highpass(mode.highpass_hz(), fs),
            deemph: OnePole::new(1.0 / (2.0 * std::f64::consts::PI * 75e-6), fs),
            dc: OnePole::new(30.0, fs),
            agc: 0.0,
            noise_hp: Biquad::highpass(6_000.0, fs),
            noise: OnePole::new(20.0, fs),
            level: OnePole::new(20.0, fs),
            squelch: 0.35,
            open: true,
            // Noise band-limited to the channel cutoff B decorrelates over
            // ~1/(2B); a carrier's phase advance over that lag is steady.
            lag: vec![Complex32::default(); ((fs / (2.0 * mode.channel().0)).round() as usize).max(1)],
            lag_pos: 0,
            coh: [OnePole::new(10.0, fs); 2],
            coherence: 0.0,
            pos: 0.0,
            last: 0.0,
        }
    }

    /// AM carrier squelch statistic: magnitude of the mean unit phasor of
    /// the channel's phase advance over the lag, 0 for noise, →1 for a
    /// carrier (whatever its envelope does). Meaningful in AM mode only.
    pub fn coherence(&self) -> f32 {
        self.coherence
    }

    /// Whether the squelch is open (NFM noise squelch, AM carrier squelch;
    /// WFM is always open).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Demodulates `x` (at the rate given to `new`) and appends 48 kHz audio
    /// in roughly −1..1 to `out`.
    pub fn process(&mut self, x: &[Complex32], out: &mut Vec<f32>) {
        let k = (self.fs / (2.0 * std::f64::consts::PI)) as f32;
        let step = self.fs / AUDIO_RATE;
        for &s in x {
            let a = match self.mode {
                Mode::Nfm | Mode::Wfm | Mode::P25 => {
                    let d = (s * self.prev.conj()).arg() * k;
                    self.prev = s;
                    if self.mode == Mode::Nfm {
                        // Noise squelch: when only noise is present the
                        // discriminator output is broadband and loud above
                        // the voice band.
                        let hp = self.noise_hp.push(d);
                        let n = self.noise.push(hp * hp);
                        let l = self.level.push(d * d);
                        let ratio = if l > 0.0 { n / l } else { 1.0 };
                        // Hysteresis.
                        if self.open && ratio > self.squelch * 1.2 {
                            self.open = false;
                        } else if !self.open && ratio < self.squelch {
                            self.open = true;
                        }
                    }
                    let d = if self.mode == Mode::Wfm { self.deemph.push(d) } else { d };
                    d / self.mode.deviation()
                }
                Mode::Am => {
                    let m = s.norm();
                    let dc = self.dc.push(m);
                    self.agc += 0.0005 * (dc - self.agc);
                    // Carrier squelch on phase coherence. Over a lag the
                    // channel's noise has decorrelated across, noise gives a
                    // random phase advance and a carrier (any AM on it: the
                    // envelope is divided out) a steady one, so the mean
                    // unit phasor is ~0 for noise and ~1 for a carrier,
                    // ~exp(-1/(2·SNR)) in between. Unlike an envelope ripple
                    // measure this does not depend on the shape of the
                    // channel filter or the modulation.
                    if self.squelch > 0.0 {
                        let old = self.lag[self.lag_pos];
                        self.lag[self.lag_pos] = s;
                        self.lag_pos = (self.lag_pos + 1) % self.lag.len();
                        let p = s * old.conj();
                        let n = p.norm();
                        let u = if n > 0.0 { p / n } else { Complex32::default() };
                        let re = self.coh[0].push(u.re);
                        let im = self.coh[1].push(u.im);
                        self.coherence = (re * re + im * im).sqrt();
                        if self.open && self.coherence < AM_CLOSE_COHERENCE {
                            self.open = false;
                        } else if !self.open && self.coherence > AM_OPEN_COHERENCE {
                            self.open = true;
                        }
                    }
                    if self.agc > 0.0 { (m - dc) / (self.agc * 1.5) } else { 0.0 }
                }
            };
            let mut y = self.hp.push(a);
            for f in &mut self.lp {
                y = f.push(y);
            }
            // Linear-interpolating resampler to 48 kHz.
            while self.pos <= 1.0 {
                let v = self.last + (y - self.last) * self.pos as f32;
                let muted = matches!(self.mode, Mode::Nfm | Mode::Am) && !self.open && self.squelch > 0.0;
                out.push(if muted { 0.0 } else { v.clamp(-1.0, 1.0) });
                self.pos += step;
            }
            self.pos -= 1.0;
            self.last = y;
        }
    }
}

/// AM carrier squelch thresholds on the phase coherence (see `process`):
/// noise reads ~0.1 (the estimator's bias), a carrier at 0 dB channel SNR
/// ~0.5 and at 6 dB ~0.8. Hysteresis between the two.
const AM_OPEN_COHERENCE: f32 = 0.5;
const AM_CLOSE_COHERENCE: f32 = 0.35;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn am_squelch_opens_on_a_carrier_and_closes_on_noise() {
        let fs = 48_000.0;
        let n = 48_000;
        let mut seed = 12345u64;
        let mut uni = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
        };
        // Gaussian noise (Box-Muller): the envelope must be Rayleigh for the
        // squelch's statistics to apply.
        let mut rnd = move || ((-2.0 * uni().ln()).sqrt() * (2.0 * std::f64::consts::PI * uni()).cos()) as f32;
        // Noise alone: Rayleigh envelope, squelch closes.
        let noise: Vec<Complex32> = (0..n).map(|_| Complex32::new(rnd(), rnd())).collect();
        let mut d = Demod::new(Mode::Am, fs);
        let mut out = Vec::new();
        d.process(&noise, &mut out);
        assert!(!d.is_open(), "noise should close the AM squelch");
        assert!(out[out.len() / 2..].iter().all(|v| *v == 0.0), "closed squelch mutes");
        // A carrier modulated 80% by a 1 kHz tone over the same noise, 20 dB up: opens.
        let am: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let env = 10.0 * (1.0 + 0.8 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as f32;
                Complex32::new(env, 0.0) + Complex32::new(rnd(), rnd())
            })
            .collect();
        let mut out = Vec::new();
        d.process(&am, &mut out);
        assert!(d.is_open(), "a carrier should open the AM squelch");
        assert!(out[out.len() / 2..].iter().any(|v| v.abs() > 0.05));
    }

    /// Gaussian IQ noise at `fs` plus a carrier of amplitude `a` modulated
    /// `m` × 100 % by a tone, run through the AM channel DDC like the
    /// listener does. Returns the demod after `secs` of signal.
    fn am_through_ddc(fs: f64, a: f32, m: f32, tone_hz: f64, secs: f64, seed: u64) -> Demod {
        let (cutoff, target) = Mode::Am.channel();
        let mut ddc = crate::Ddc::new(fs, 0.0, cutoff, target);
        let mut d = Demod::new(Mode::Am, ddc.fs_out);
        let mut seed = seed | 1;
        let mut uni = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
        };
        let n = (fs * secs) as usize;
        let mut input = Vec::with_capacity(65_536);
        let (mut chan, mut audio) = (Vec::new(), Vec::new());
        let mut i = 0usize;
        while i < n {
            input.clear();
            for _ in 0..65_536.min(n - i) {
                let t = i as f64 / fs;
                let env = a * (1.0 + m * (2.0 * std::f64::consts::PI * tone_hz * t).sin() as f32);
                // Carrier 1.3 kHz off centre: the squelch must not care.
                let ph = (2.0 * std::f64::consts::PI * 1300.0 * t) as f32;
                let r = (-2.0 * uni().ln()).sqrt();
                let th = 2.0 * std::f64::consts::PI * uni();
                input.push(Complex32::from_polar(env, ph) + Complex32::new((r * th.cos()) as f32, (r * th.sin()) as f32));
                i += 1;
            }
            chan.clear();
            ddc.process(&input, &mut chan);
            audio.clear();
            d.process(&chan, &mut audio);
        }
        d
    }

    /// Carrier amplitude for a channel SNR in dB: unit-variance noise per
    /// component (power 2) of which the ±B channel keeps 2B/fs.
    fn amp_for_snr(fs: f64, snr_db: f32) -> f32 {
        let noise = 2.0 * 2.0 * Mode::Am.channel().0 / fs;
        (noise * 10f64.powf(snr_db as f64 / 10.0)).sqrt() as f32
    }

    /// Through the real AM channel filter (the review found the old
    /// envelope-ripple squelch opened on filtered noise alone: the filter
    /// removed the ripple it measured), at both Airspy rates. Measured
    /// coherence: noise 0.14-0.20, carrier 0 dB 0.56-0.62, 6 dB 0.88-0.90,
    /// 100 % tone-modulated AM 0.92+.
    #[test]
    fn am_squelch_through_the_channel_filter() {
        for fs in [10e6, 2.5e6] {
            for seed in [3, 11, 29] {
                let d = am_through_ddc(fs, 0.0, 0.0, 1000.0, 0.6, seed);
                assert!(!d.is_open() && d.coherence() < 0.3, "fs {fs} seed {seed}: noise alone, coherence {}", d.coherence());
            }
            // A weak bare carrier opens from a few dB up...
            for snr in [3.0f32, 6.0, 10.0] {
                let d = am_through_ddc(fs, amp_for_snr(fs, snr), 0.0, 1000.0, 0.6, 5);
                assert!(d.is_open() && d.coherence() > 0.7, "fs {fs}: carrier at {snr} dB, coherence {}", d.coherence());
            }
            // ...and one well below the noise stays shut.
            let d = am_through_ddc(fs, amp_for_snr(fs, -6.0), 0.0, 1000.0, 0.6, 5);
            assert!(!d.is_open(), "fs {fs}: carrier at -6 dB opened, coherence {}", d.coherence());
            // Heavy modulation, including a tone at the top of the audio
            // band, doesn't look like noise.
            for (m, tone) in [(0.8, 1000.0), (1.0, 4500.0), (1.0, 2400.0)] {
                let d = am_through_ddc(fs, amp_for_snr(fs, 20.0), m, tone, 0.6, 7);
                assert!(d.is_open() && d.coherence() > 0.85, "fs {fs}: AM m={m} at {tone} Hz, coherence {}", d.coherence());
            }
        }
    }

    fn fm(fs: f64, n: usize, tone: f64, dev: f64) -> Vec<Complex32> {
        let mut ph = 0.0f64;
        (0..n)
            .map(|i| {
                let f = dev * (2.0 * std::f64::consts::PI * tone * i as f64 / fs).sin();
                ph += 2.0 * std::f64::consts::PI * f / fs;
                Complex32::from_polar(1.0, ph as f32)
            })
            .collect()
    }

    /// Power of `tone` in the audio relative to total (Goertzel-ish via DFT bin).
    fn tone_fraction(a: &[f32], tone: f64) -> f32 {
        let w = 2.0 * std::f64::consts::PI * tone / AUDIO_RATE;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, v) in a.iter().enumerate() {
            re += *v as f64 * (w * i as f64).cos();
            im += *v as f64 * (w * i as f64).sin();
        }
        let p = 2.0 * (re * re + im * im) / a.len() as f64 / a.len() as f64;
        let total = a.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / a.len() as f64;
        (p / total) as f32
    }

    #[test]
    fn nfm_recovers_a_tone_at_48k() {
        let fs = 48_077.0;
        let mut d = Demod::new(Mode::Nfm, fs);
        let mut out = Vec::new();
        for c in fm(fs, 48_077, 1_000.0, 3_000.0).chunks(1000) {
            d.process(c, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() < 10, "{}", out.len());
        let a = &out[4800..];
        assert!(tone_fraction(a, 1_000.0) > 0.9);
        assert!(d.is_open());
    }

    #[test]
    fn nfm_squelch_closes_on_noise() {
        let fs = 48_000.0;
        let mut d = Demod::new(Mode::Nfm, fs);
        let mut x = 0x1234_5678u64;
        let noise: Vec<Complex32> = (0..48_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                Complex32::new((x as u32) as f32 / u32::MAX as f32 - 0.5, (x >> 32) as u32 as f32 / u32::MAX as f32 - 0.5)
            })
            .collect();
        let mut out = Vec::new();
        d.process(&noise, &mut out);
        assert!(!d.is_open());
        assert!(out[24_000..].iter().all(|v| *v == 0.0));
    }

    #[test]
    fn am_recovers_a_tone() {
        let fs = 48_000.0;
        let x: Vec<Complex32> = (0..96_000)
            .map(|i| Complex32::new(1.0 + 0.5 * (2.0 * std::f32::consts::PI * 700.0 * i as f32 / fs as f32).sin(), 0.0))
            .collect();
        let mut d = Demod::new(Mode::Am, fs);
        let mut out = Vec::new();
        d.process(&x, &mut out);
        assert!(tone_fraction(&out[48_000..], 700.0) > 0.9);
    }

    #[test]
    fn wfm_recovers_a_tone() {
        let fs = 240_000.0;
        let mut d = Demod::new(Mode::Wfm, fs);
        let mut out = Vec::new();
        d.process(&fm(fs, 240_000, 1_000.0, 50_000.0), &mut out);
        assert!(tone_fraction(&out[9_600..], 1_000.0) > 0.9);
    }
}
