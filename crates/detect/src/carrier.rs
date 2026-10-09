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
