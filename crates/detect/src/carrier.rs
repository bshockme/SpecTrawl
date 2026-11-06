//! Narrow signals: is it a transmitter's carrier, or a spur / noise spike?
//!
//! Measured on a channel capture centred near the signal:
//! - **purity**: fraction of the on-signal power within ±`PURE_HZ` of the
//!   strongest line. A spur or birdie is a pure line; voice, data and noise
//!   spread out.
//! - **drift**: how much that line's frequency wanders between 85 ms blocks.
//!   Spurs from the radio's own clocks don't move; transmitters' oscillators
//!   wander a little, especially at key-up.
//! - **duty / keying**: on-off structure. A spur is always there (or
//!   flickers at the detection threshold); a carrier keys up and down.
//! - **AM / FM content**: envelope variation in the voice band; FM deviation
//!   and sub-audible CTCSS tones in the discriminator output.

use num_complex::Complex32;
use rustfft::FftPlanner;

/// Line purity window, Hz either side of the peak.
const PURE_HZ: f64 = 30.0;

#[derive(Debug, Clone, Default)]
pub struct CarrierFeatures {
    pub seconds: f64,
    /// Fraction of blocks where the signal is on.
    pub duty: f32,
    /// On/off level difference, dB (p90 − p10 of block powers).
    pub keying_db: f32,
    /// Fraction of on-power above the noise floor within ±30 Hz of the
    /// strongest line.
    pub purity: f32,
    /// Strongest line over the median bin of the summed spectrum (11.7 Hz
    /// bins at 48 kHz), dB.
    pub line_snr_db: f32,
    /// Width of the line where it is within 6 dB of its peak, Hz.
    pub line_width_hz: f32,
    /// Fraction of on-blocks where the line stood 9 dB clear (drift uses those).
    pub clear_blocks: f32,
    /// Offset of the strongest line from the channel centre, Hz.
    pub line_hz: f64,
    /// Std of the per-block peak frequency, Hz.
    pub drift_hz: f32,
    /// Envelope std/mean over on-blocks.
    pub envelope_cv: f32,
    /// Envelope power at 300-3000 Hz relative to its mean (dB): AM voice.
    pub am_voice_db: f32,
    /// RMS deviation of the discriminator over on-blocks, Hz.
    pub fm_dev_hz: f32,
    /// Strongest 60-260 Hz tone in the discriminator, if it stands out, Hz.
    pub tone_hz: Option<f32>,
    /// That tone, if it is a standard CTCSS tone.
    pub ctcss_hz: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// A pure, steady line: the radio's own spur or a nearby birdie.
    Spur,
    /// An FM transmitter with a CTCSS tone (squelch tone), voice or not.
    FmCtcss(f32),
    /// FM with voice-like deviation.
    FmVoice,
    /// AM with voice-band envelope modulation.
    AmVoice,
    /// A transmitter carrier with no modulation found.
    Carrier,
    /// Nothing consistent: a noise spike.
    Noise,
}

impl Verdict {
    pub fn name(&self) -> String {
        match self {
            Verdict::Spur => "Spur".into(),
            Verdict::FmCtcss(t) => format!("FM carrier, CTCSS {t:.1} Hz"),
            Verdict::FmVoice => "FM voice".into(),
            Verdict::AmVoice => "AM voice".into(),
            Verdict::Carrier => "Carrier".into(),
            Verdict::Noise => "Noise".into(),
        }
    }
}

/// Standard CTCSS tones (EIA), Hz.
const CTCSS: [f32; 50] = [
    67.0, 69.3, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2, 110.9, 114.8, 118.8, 123.0, 127.3, 131.8,
    136.5, 141.3, 146.2, 150.0, 151.4, 156.7, 159.8, 162.2, 165.5, 167.9, 171.3, 173.8, 177.3, 179.9, 183.5, 186.2, 189.9, 192.8, 196.6,
    199.5, 203.5, 206.5, 210.7, 218.1, 225.7, 229.1, 233.6, 241.8, 250.3,
];

fn fft_power(x: &[Complex32]) -> Vec<f32> {
    let mut b = x.to_vec();
    FftPlanner::new().plan_fft_forward(b.len()).process(&mut b);
    b.iter().map(|c| c.norm_sqr()).collect()
}

pub fn measure(x: &[Complex32], fs: f64) -> CarrierFeatures {
    let mut f = CarrierFeatures { seconds: x.len() as f64 / fs, ..Default::default() };
    let blk = 4096usize;
    let nb = x.len() / blk;
    if nb < 4 {
        return f;
    }
    // Block powers → on/off.
    let pw: Vec<f32> = (0..nb).map(|k| x[k * blk..(k + 1) * blk].iter().map(|c| c.norm_sqr()).sum::<f32>() / blk as f32).collect();
    let mut s: Vec<f32> = pw.iter().map(|p| 10.0 * p.max(1e-30).log10()).collect();
    let db = s.clone();
    s.sort_by(f32::total_cmp);
    let (p10, p90) = (s[nb / 10], s[nb * 9 / 10]);
    f.keying_db = p90 - p10;
    let on: Vec<usize> = if f.keying_db < 3.0 { (0..nb).collect() } else { (0..nb).filter(|&k| db[k] > p10 + f.keying_db / 2.0).collect() };
    f.duty = on.len() as f32 / nb as f32;

    // Summed spectra of the on-blocks → the line, its SNR, width and purity
    // (power above the noise floor, so a weak line in a wide channel counts).
    let bin = fs / blk as f64;
    let spectra: Vec<Vec<f32>> = on.iter().map(|&k| fft_power(&x[k * blk..(k + 1) * blk])).collect();
    let mut sum = vec![0.0f32; blk];
    for p in &spectra {
        for (a, v) in sum.iter_mut().zip(p) {
            *a += v;
        }
    }
    let at = |i: isize| sum[i.rem_euclid(blk as isize) as usize];
    let (pi, &pk) = sum.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
    let to_hz = |i: f64| if i > blk as f64 / 2.0 { i - blk as f64 } else { i } * bin;
    f.line_hz = to_hz(pi as f64);
    // Noise from 300 Hz-2 kHz either side of the line: inside any channel
    // filter, outside the line itself.
    let lo_b = (300.0 / bin) as isize;
    let hi_b = (2000.0 / bin) as isize;
    let mut near_bins: Vec<f32> = (lo_b..hi_b).flat_map(|d| [at(pi as isize - d), at(pi as isize + d)]).collect();
    let mid = near_bins.len() / 2;
    let noise = *near_bins.select_nth_unstable_by(mid, f32::total_cmp).1;
    f.line_snr_db = 10.0 * (pk / noise.max(1e-30)).log10();
    let mut w = 0;
    while w < 50 && at(pi as isize - w - 1).max(at(pi as isize + w + 1)) > pk / 4.0 {
        w += 1;
    }
    f.line_width_hz = ((2 * w + 1) as f64 * bin) as f32;
    let above = |v: f32| (v - noise).max(0.0);
    let total: f32 = sum.iter().map(|v| above(*v)).sum();
    let r = (PURE_HZ / bin).ceil() as isize;
    let near: f32 = (-r..=r).map(|d| above(at(pi as isize + d))).sum();
    f.purity = near / total.max(1e-30);
    // Drift: the line's position per block, only where it stands clear.
    let mut peaks = Vec::new();
    for p in &spectra {
        let ip = (pi as isize - 8..=pi as isize + 8)
            .max_by(|a, b| p[a.rem_euclid(blk as isize) as usize].total_cmp(&p[b.rem_euclid(blk as isize) as usize]))
            .unwrap();
        let v = |d: isize| p[(ip + d).rem_euclid(blk as isize) as usize];
        let mut q: Vec<f32> = p.clone();
        let nb = *q.select_nth_unstable_by(blk / 2, f32::total_cmp).1;
        if v(0) < 8.0 * nb {
            continue;
        }
        let (a, b, c) = (v(-1), v(0), v(1));
        let d = 0.5 * (a - c) / (a - 2.0 * b + c);
        peaks.push(to_hz(ip.rem_euclid(blk as isize) as f64) + d as f64 * bin);
    }
    if peaks.len() >= 3 {
        let mean = peaks.iter().sum::<f64>() / peaks.len() as f64;
        f.drift_hz = (peaks.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / peaks.len() as f64).sqrt() as f32;
        f.clear_blocks = peaks.len() as f32 / on.len().max(1) as f32;
    } else {
        f.drift_hz = f32::NAN;
    }

    // Envelope and discriminator over on-blocks.
    let on_x: Vec<Complex32> = on.iter().flat_map(|&k| x[k * blk..(k + 1) * blk].iter().copied()).collect();
    let mags: Vec<f32> = on_x.iter().map(|c| c.norm()).collect();
    let mm = mags.iter().sum::<f32>() / mags.len() as f32;
    f.envelope_cv = (mags.iter().map(|m| (m - mm).powi(2)).sum::<f32>() / mags.len() as f32).sqrt() / mm.max(1e-30);
    let disc = crate::features::discriminator(&on_x, fs);
    let dm = disc.iter().sum::<f32>() / disc.len() as f32;
    f.fm_dev_hz = (disc.iter().map(|d| (d - dm).powi(2)).sum::<f32>() / disc.len() as f32).sqrt();

    // Voice-band envelope and sub-audible tones: one long FFT of each,
    // decimated to ~2 kHz for the tone search.
    let n = 1usize << (on_x.len().min(1 << 18)).ilog2();
    if n >= 8192 {
        let env: Vec<Complex32> = mags[..n].iter().map(|m| Complex32::new(m - mm, 0.0)).collect();
        let pe = fft_power(&env);
        let eb = fs / n as f64;
        let band = |lo: f64, hi: f64| (lo / eb) as usize..(hi / eb) as usize;
        let voice: f32 = pe[band(300.0, 3000.0)].iter().sum::<f32>();
        f.am_voice_db = 10.0 * (voice / (mm * mm * n as f32 * n as f32)).max(1e-30).log10();

        let dec = (fs / 2000.0).floor().max(1.0) as usize;
        let d2: Vec<Complex32> = disc.chunks(dec).map(|c| Complex32::new(c.iter().sum::<f32>() / c.len() as f32 - dm, 0.0)).collect();
        let m = 1usize << d2.len().ilog2();
        if m >= 1024 {
            let pd = fft_power(&d2[..m]);
            let db2 = fs / dec as f64 / m as f64;
            let lo = (60.0 / db2) as usize;
            let hi = (260.0 / db2) as usize;
            let mut around: Vec<f32> = pd[lo..hi].to_vec();
            let med = *around.select_nth_unstable_by((hi - lo) / 2, f32::total_cmp).1;
            let (k, &pk) = pd[lo..hi].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
            let tone = ((k + lo) as f64 * db2) as f32;
            if pk > 30.0 * med {
                f.tone_hz = Some(tone);
                // Tight match: the Airspy's USB transfer rate (152.6 Hz at
                // 10 MS/s) modulates its own spurs and sits 1.2 Hz from 151.4.
                f.ctcss_hz = CTCSS.iter().copied().find(|t| (t - tone).abs() < 0.6 * db2.max(0.5) as f32 + 0.3);
            }
        }
    }
    f
}

pub fn verdict(f: &CarrierFeatures) -> Verdict {
    if let Some(t) = f.ctcss_hz.filter(|t| CTCSS.contains(t)) {
        return Verdict::FmCtcss(t);
    }
    // A pure, narrow line that holds its frequency (airband birdies here:
    // purity 0.65-0.7, 12 Hz wide, 3-6 Hz drift; transmitters drift 15-50 Hz
    // or spread with modulation).
    let steady = f.drift_hz.is_nan() || f.drift_hz < 10.0;
    if f.purity >= 0.5 && f.line_width_hz <= 40.0 && steady {
        return Verdict::Spur;
    }
    // Envelope and deviation only mean something when the signal dominates
    // the channel: a weak line leaves noise statistics (envelope CV ≈ 0.52).
    if f.envelope_cv < 0.45 {
        if f.am_voice_db > -20.0 && f.envelope_cv > 0.15 {
            return Verdict::AmVoice;
        }
        if f.fm_dev_hz > 800.0 && f.envelope_cv < 0.3 {
            return Verdict::FmVoice;
        }
    }
    if f.purity >= 0.3 {
        return Verdict::Carrier;
    }
    Verdict::Noise
}

/// Amplitude modulation measured against the carrier's own phase: the voice-band content in phase with the carrier
/// (AM) and in quadrature (FM, phase noise). Noise splits evenly between the two, so this tells weak AM from a line with
/// noise around it, which the envelope cannot (a weak line's envelope is the noise's).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoherentAm {
    /// The carrier's offset from the channel centre, Hz.
    pub line_hz: f64,
    /// The carrier over the median bin 1.5–5 kHz from the centre (11.7 Hz bins at 48 kHz), dB.
    pub line_snr_db: f32,
    /// In-phase power at 300–2500 Hz over in-phase power at 3.5–4.8 kHz (the noise): dB.
    pub in_phase_db: f32,
    /// In-phase over quadrature power at 300–2500 Hz: dB.
    pub i_over_q_db: f32,
}

impl CoherentAm {
    /// A carrier with voice-band AM on it: the line clear of the noise, near the centre, and in-phase content standing
    /// over both the noise and the quadrature.
    pub fn is_am(&self) -> bool {
        self.line_snr_db >= COHERENT_LINE_DB
            && self.line_hz.abs() <= COHERENT_OFFSET_HZ
            && self.in_phase_db >= COHERENT_EXCESS_DB
            && self.i_over_q_db >= COHERENT_EXCESS_DB
    }
}

/// A carrier stands this far over the bins beside it (an air band call the detector saw only as its carrier: 15–30 dB).
const COHERENT_LINE_DB: f32 = 10.0;
/// The window is centred on the record, whose centre is the carrier to within a bin or two of the detector's.
const COHERENT_OFFSET_HZ: f64 = 1500.0;
/// Least in-phase excess, over the noise and over the quadrature. On air (2026-11-06, 118–129 MHz, 53 looks) bare
/// carriers measured −0.3 to +0.6 dB, air band calls 3–6 dB on both (ACARS bursts too: AM data).
const COHERENT_EXCESS_DB: f32 = 2.0;
const COHERENT_BLOCK: usize = 4096;

/// Measures [`CoherentAm`] on `x`, a channel at `fs` that holds at least ±5 kHz around its centre. `None` when the
/// window is too short (under 8 blocks of 4096) or too narrow.
pub fn coherent_am(x: &[Complex32], fs: f64) -> Option<CoherentAm> {
    let nb = x.len() / COHERENT_BLOCK;
    if nb < 8 || fs < 12e3 {
        return None;
    }
    let bin = fs / COHERENT_BLOCK as f64;
    // The line: the strongest bin within ±1.5 kHz of the centre, in the summed spectra.
    let mut sum = vec![0.0f32; COHERENT_BLOCK];
    for k in 0..nb {
        for (a, v) in sum.iter_mut().zip(fft_power(&x[k * COHERENT_BLOCK..(k + 1) * COHERENT_BLOCK])) {
            *a += v;
        }
    }
    let at = |d: isize| sum[d.rem_euclid(COHERENT_BLOCK as isize) as usize];
    let reach = (COHERENT_OFFSET_HZ / bin) as isize;
    let pi = (-reach..=reach).max_by(|a, b| at(*a).total_cmp(&at(*b)))?;
    let (lo, hi) = ((1500.0 / bin) as isize, (5000.0 / bin) as isize);
    let mut beside: Vec<f32> = (lo..hi).flat_map(|d| [at(-d), at(d)]).collect();
    let mid = beside.len() / 2;
    let noise = *beside.select_nth_unstable_by(mid, f32::total_cmp).1;
    let line_hz = pi as f64 * bin;
    let line_snr_db = 10.0 * (at(pi) / noise.max(1e-30)).log10();

    let (i_sig, q_sig) = to_carrier(x, fs, line_hz);
    // Mean power per bin over `f0..f1` Hz of a real signal, its mean (the carrier, in `i_sig`) removed.
    let band = |s: &[f32], f0: f64, f1: f64| -> f32 {
        let mean = s.iter().sum::<f32>() / s.len().max(1) as f32;
        let blocks = s.len() / COHERENT_BLOCK;
        let (b0, b1) = ((f0 / bin).ceil() as usize, (f1 / bin).floor() as usize);
        let mut p = 0.0f32;
        for k in 0..blocks {
            let blk: Vec<Complex32> =
                s[k * COHERENT_BLOCK..(k + 1) * COHERENT_BLOCK].iter().map(|v| Complex32::new(v - mean, 0.0)).collect();
            let ps = fft_power(&blk);
            p += ps[b0..=b1].iter().sum::<f32>();
        }
        p / (b1 + 1 - b0) as f32
    };
    let (i_voice, i_ref, q_voice) = (band(&i_sig, 300.0, 2500.0), band(&i_sig, 3500.0, 4800.0), band(&q_sig, 300.0, 2500.0));
    let db = |a: f32, b: f32| 10.0 * (a.max(1e-30) / b.max(1e-30)).log10();
    Some(CoherentAm { line_hz, line_snr_db, in_phase_db: db(i_voice, i_ref), i_over_q_db: db(i_voice, q_voice) })
}

/// `x` in phase with its carrier at `line_hz` and in quadrature: shifted to the line, then rotated by the carrier's
/// phasor, a 25 ms moving average (its first null at 40 Hz passes a drifting carrier and stops voice). The in-phase
/// signal is AM's audio riding on the carrier's level; the quadrature, FM's and the phase noise's. Shorter than `x` by
/// the average's length (its first value is `x`'s sample `l / 2`).
pub fn to_carrier(x: &[Complex32], fs: f64, line_hz: f64) -> (Vec<f32>, Vec<f32>) {
    let w = -2.0 * std::f64::consts::PI * line_hz / fs;
    let y: Vec<Complex32> = x
        .iter()
        .enumerate()
        .map(|(n, v)| v * Complex32::from_polar(1.0, (w * n as f64).rem_euclid(std::f64::consts::TAU) as f32))
        .collect();
    let l = ((0.025 * fs) as usize).max(1);
    let mut acc = Complex32::new(0.0, 0.0);
    let mut i_sig = Vec::with_capacity(y.len());
    let mut q_sig = Vec::with_capacity(y.len());
    for n in 0..y.len() {
        acc += y[n];
        if n >= l {
            acc -= y[n - l];
        }
        // The average is centred half a window back.
        if n + 1 >= l {
            let m = n + 1 - l / 2 - 1;
            let c = acc / acc.norm().max(1e-30);
            let r = y[m] * c.conj();
            i_sig.push(r.re);
            q_sig.push(r.im);
        }
    }
    (i_sig, q_sig)
}

/// The strongest line within `reach_hz` of the centre: its offset, Hz (summed spectra of 4096-sample blocks). `None`
/// for less than one block.
pub fn line_near_centre(x: &[Complex32], fs: f64, reach_hz: f64) -> Option<f64> {
    let nb = x.len() / COHERENT_BLOCK;
    if nb == 0 {
        return None;
    }
    let mut sum = vec![0.0f32; COHERENT_BLOCK];
    for k in 0..nb {
        for (a, v) in sum.iter_mut().zip(fft_power(&x[k * COHERENT_BLOCK..(k + 1) * COHERENT_BLOCK])) {
            *a += v;
        }
    }
    let bin = fs / COHERENT_BLOCK as f64;
    let reach = (reach_hz / bin) as isize;
    let at = |d: isize| sum[d.rem_euclid(COHERENT_BLOCK as isize) as usize];
    (-reach..=reach).max_by(|a, b| at(*a).total_cmp(&at(*b))).map(|d| d as f64 * bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48e3;
    const SECS: f64 = 1.5;

    /// Complex Gaussian noise of power `p`, from a fixed seed.
    fn noise(n: usize, p: f32, seed: u64) -> Vec<Complex32> {
        let mut s = seed;
        let mut u = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let sd = (p / 2.0).sqrt() as f64;
        (0..n)
            .map(|_| {
                let (a, b) = (u(), u());
                let r = (-2.0 * a.ln()).sqrt() * sd;
                Complex32::new((r * (std::f64::consts::TAU * b).cos()) as f32, (r * (std::f64::consts::TAU * b).sin()) as f32)
            })
            .collect()
    }

    /// A carrier at `off` Hz, amplitude modulated by `am` and phase modulated by `pm` (both functions of time), with
    /// noise of power `p` per sample over the 48 kHz channel.
    fn signal(off: f64, am: impl Fn(f64) -> f64, pm: impl Fn(f64) -> f64, p: f32) -> Vec<Complex32> {
        let n = (SECS * FS) as usize;
        noise(n, p, 7)
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                let t = i as f64 / FS;
                v + Complex32::from_polar(am(t) as f32, (std::f64::consts::TAU * off * t + pm(t)) as f32)
            })
            .collect()
    }

    /// Speech-like: three tones in the voice band, gated on and off at 4 Hz.
    fn voice(t: f64) -> f64 {
        let s = (std::f64::consts::TAU * 440.0 * t).sin()
            + 0.7 * (std::f64::consts::TAU * 1130.0 * t).sin()
            + 0.5 * (std::f64::consts::TAU * 2010.0 * t).sin();
        let gate = if (t * 4.0).fract() < 0.6 { 1.0 } else { 0.2 };
        s * gate / 2.2
    }

    #[test]
    fn am_voice_is_am_even_where_the_detector_saw_only_the_carrier() {
        // The carrier stands `N / p` over a bin's noise: 36 dB at p = 1, 41 dB at 0.3, as the calls that measured 3–6 dB
        // on air did. The sidebands sit at the noise per hertz or under it, which is why the detector missed them.
        for (depth, p) in [(0.8, 1.0f32), (0.5, 0.3)] {
            let x = signal(300.0, |t| 1.0 + depth * voice(t), |_| 0.0, p);
            let m = coherent_am(&x, FS).unwrap();
            assert!(m.is_am(), "depth {depth}, noise {p}: {m:?}");
            assert!((m.line_hz - 300.0).abs() < 12.0, "{m:?}");
        }
        // Lightly modulated under more noise there is nothing to measure, and it says so.
        let m = coherent_am(&signal(300.0, |t| 1.0 + 0.3 * voice(t), |_| 0.0, 13.0), FS).unwrap();
        assert!(!m.is_am(), "{m:?}");
    }

    #[test]
    fn a_bare_carrier_in_noise_is_not_am() {
        for p in [1.0f32, 4.0, 13.0] {
            let m = coherent_am(&signal(-200.0, |_| 1.0, |_| 0.0, p), FS).unwrap();
            assert!(!m.is_am(), "noise {p}: {m:?}");
            assert!(m.in_phase_db.abs() < 1.0 && m.i_over_q_db.abs() < 1.0, "{m:?}");
        }
    }

    #[test]
    fn a_carrier_with_phase_noise_or_fm_is_not_am() {
        // Phase modulation in the voice band (a noisy oscillator, or PM): in quadrature only.
        let wobble = |t: f64| 0.3 * (std::f64::consts::TAU * 600.0 * t).sin() + 0.2 * (std::f64::consts::TAU * 1700.0 * t).sin();
        let m = coherent_am(&signal(0.0, |_| 1.0, wobble, 1.0), FS).unwrap();
        assert!(!m.is_am(), "phase noise: {m:?}");
        assert!(m.i_over_q_db < -2.0, "{m:?}");
        // Narrow FM with voice: 500 Hz peak deviation.
        let mut phase = 0.0;
        let mut last = 0.0;
        let fm: Vec<f64> = (0..(SECS * FS) as usize)
            .map(|i| {
                let t = i as f64 / FS;
                phase += std::f64::consts::TAU * 500.0 * voice(t) * (t - last);
                last = t;
                phase
            })
            .collect();
        let m = coherent_am(&signal(0.0, |_| 1.0, |t| fm[((t * FS) as usize).min(fm.len() - 1)], 1.0), FS).unwrap();
        assert!(!m.is_am(), "FM: {m:?}");
    }

    #[test]
    fn noise_alone_has_no_line() {
        let m = coherent_am(&noise((SECS * FS) as usize, 1.0, 3), FS).unwrap();
        assert!(!m.is_am(), "{m:?}");
        assert!(m.line_snr_db < COHERENT_LINE_DB, "{m:?}");
    }

    #[test]
    fn too_short_says_nothing() {
        assert_eq!(coherent_am(&noise(4096 * 7, 1.0, 1), FS), None);
    }
}
