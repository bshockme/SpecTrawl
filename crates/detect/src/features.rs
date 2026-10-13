//! Signal features from a channelised complex baseband signal.

use num_complex::Complex32;
use rustfft::FftPlanner;

#[derive(Debug, Clone)]
pub struct Features {
    /// Envelope coefficient of variation (std/mean of |x|). Low for
    /// constant-envelope FM/FSK, high for AM/OOK.
    pub envelope_cv: f32,
    /// Robust peak frequency deviation (98th percentile of |f − median|), Hz.
    pub deviation_hz: f32,
    /// Median instantaneous frequency (residual offset from the channel centre), Hz.
    pub offset_hz: f32,
    /// Distinct levels in the frequency histogram (2, 4, or 0 = not FSK-like).
    pub fsk_levels: u8,
    /// Symbol rate from the transition spectral line, snapped to a standard
    /// rate when within 3%.
    pub symbol_rate: Option<f64>,
    pub symbol_rate_raw: Option<f64>,
    /// Signal-to-noise ratio of the window, dB: the channel's power density
    /// against the noise floor beside it, measured by the caller on the
    /// wideband spectrum (`classify` itself only sees the channel). A window
    /// of floor alone (an empty stretch, a raised hump, a neighbour's skirt)
    /// reads near 0 dB. NaN when the caller did not measure it.
    pub snr_db: f32,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            envelope_cv: 0.0,
            deviation_hz: 0.0,
            offset_hz: 0.0,
            fsk_levels: 0,
            symbol_rate: None,
            symbol_rate_raw: None,
            snr_db: f32::NAN,
        }
    }
}

/// True where the envelope is clearly above the noise: above a third of the
/// 90th-percentile magnitude. Lightly widened so symbol edges survive.
pub fn envelope_gate(x: &[Complex32]) -> Vec<bool> {
    if x.is_empty() {
        return Vec::new();
    }
    let mut m: Vec<f32> = x.iter().map(|c| c.norm()).collect();
    let mut s = m.clone();
    let i90 = ((s.len() as f32 * 0.9) as usize).min(s.len() - 1);
    let p90 = *s.select_nth_unstable_by(i90, f32::total_cmp).1;
    let thr = p90 / 3.0;
    // Smooth over ~8 samples before thresholding.
    let mut acc = 0.0;
    for i in 0..m.len() {
        acc += m[i];
        if i >= 8 {
            acc -= x[i - 8].norm();
        }
        m[i] = acc / 8.0f32.min(i as f32 + 1.0);
    }
    m.iter().map(|v| *v > thr).collect()
}

/// Envelope coefficient of variation over the whole window.
pub fn envelope_cv(x: &[Complex32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let mags: Vec<f32> = x.iter().map(|c| c.norm()).collect();
    let mean = mags.iter().sum::<f32>() / mags.len() as f32;
    let var = mags.iter().map(|m| (m - mean).powi(2)).sum::<f32>() / mags.len() as f32;
    if mean > 0.0 { var.sqrt() / mean } else { 0.0 }
}

/// FM discriminator: instantaneous frequency in Hz, one sample shorter.
pub fn discriminator(x: &[Complex32], fs: f64) -> Vec<f32> {
    let k = (fs / (2.0 * std::f64::consts::PI)) as f32;
    x.windows(2).map(|w| (w[1] * w[0].conj()).arg() * k).collect()
}

pub fn extract(x: &[Complex32], disc: &[f32], fs: f64) -> Features {
    let mut f = Features::default();
    if x.len() < 64 || disc.len() < 64 {
        return f;
    }
    let mags: Vec<f32> = x.iter().map(|c| c.norm()).collect();
    let mean = mags.iter().sum::<f32>() / mags.len() as f32;
    let var = mags.iter().map(|m| (m - mean).powi(2)).sum::<f32>() / mags.len() as f32;
    f.envelope_cv = if mean > 0.0 { var.sqrt() / mean } else { 0.0 };

    // Centre and deviation from the 2nd/98th percentiles: for symmetric FSK the
    // median lands on a level, not the centre.
    let mut sorted = disc.to_vec();
    sorted.sort_by(f32::total_cmp);
    let q = |p: f32| sorted[((sorted.len() as f32 * p) as usize).min(sorted.len() - 1)];
    let (lo, hi) = (q(0.02), q(0.98));
    let med = (lo + hi) / 2.0;
    f.offset_hz = med;
    f.deviation_hz = (hi - lo) / 2.0;

    f.fsk_levels = count_levels(disc, med, f.deviation_hz);
    let (snapped, raw) = symbol_rate(disc, fs);
    f.symbol_rate = snapped;
    f.symbol_rate_raw = raw;
    f
}

/// Counts peaks in a smoothed histogram of the discriminator output. Clean
/// 2FSK shows two peaks, 4FSK four; voice FM and noise show one broad hump.
fn count_levels(disc: &[f32], centre: f32, dev: f32) -> u8 {
    if dev <= 0.0 {
        return 0;
    }
    const BINS: usize = 60;
    let span = dev * 1.25;
    let mut h = [0.0f32; BINS];
    for &v in disc {
        let t = ((v - centre) / span + 1.0) / 2.0;
        if (0.0..1.0).contains(&t) {
            h[(t * BINS as f32) as usize] += 1.0;
        }
    }
    let mut s = [0.0f32; BINS];
    for i in 0..BINS {
        let (a, b) = (i.saturating_sub(2), (i + 2).min(BINS - 1));
        s[i] = h[a..=b].iter().sum::<f32>() / (b - a + 1) as f32;
    }
    let max = s.iter().copied().fold(0.0, f32::max);
    let mut peaks = 0;
    for i in 1..BINS - 1 {
        if s[i] > 0.25 * max && s[i] >= s[i - 1] && s[i] > s[i + 1] {
            // Require a real dip on both sides so a flat top isn't counted.
            let left = s[..i].iter().rev().take(8).copied().fold(f32::MAX, f32::min);
            let right = s[i + 1..].iter().take(8).copied().fold(f32::MAX, f32::min);
            if left < 0.8 * s[i] && right < 0.8 * s[i] {
                peaks += 1;
            }
        }
    }
    match peaks {
        2 => 2,
        3..=5 => 4,
        _ => 0,
    }
}

const STANDARD_RATES: [f64; 10] = [512.0, 1200.0, 1600.0, 2400.0, 3200.0, 3600.0, 4800.0, 6000.0, 6400.0, 9600.0];

/// Symbol rate from the spectral line of the squared frequency derivative.
fn symbol_rate(disc: &[f32], fs: f64) -> (Option<f64>, Option<f64>) {
    let avail = disc.len().min(1 << 17);
    if avail < 1024 {
        return (None, None);
    }
    // Largest power of two that fits.
    let n = 1usize << avail.ilog2();
    let e: Vec<f32> = disc[..n].windows(2).map(|w| (w[1] - w[0]).powi(2)).collect();
    let mean = e.iter().sum::<f32>() / e.len() as f32;
    let mut buf: Vec<num_complex::Complex32> =
        e.iter().map(|&v| num_complex::Complex32::new(v - mean, 0.0)).chain(std::iter::once(Default::default())).collect();
    buf.truncate(n);
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let bin_hz = fs / n as f64;
    let lo = (400.0 / bin_hz).ceil() as usize;
    let hi = ((12_500.0 / bin_hz) as usize).min(n / 2 - 2);
    if lo + 2 >= hi {
        return (None, None);
    }
    let mag: Vec<f32> = buf[..n / 2].iter().map(|c| c.norm()).collect();
    let (k, &pk) = mag[lo..hi].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, v)| (i + lo, v)).unwrap();
    // A line must stand well above the local average.
    // Median of the neighbourhood: robust to the line itself and its skirts.
    let mut near: Vec<f32> = mag[k.saturating_sub(60)..(k + 60).min(mag.len())].to_vec();
    let mid = near.len() / 2;
    let around = *near.select_nth_unstable_by(mid, f32::total_cmp).1;
    if pk < 5.0 * around {
        return (None, None);
    }
    // Sharp transitions put lines at every multiple of the rate: prefer the
    // lowest sub-harmonic that is itself a clear line.
    let mut k = k;
    for div in [3usize, 2] {
        let kk = (k as f64 / div as f64).round() as usize;
        if kk < lo || kk + 1 >= mag.len() {
            continue;
        }
        let (j, &v) = mag[kk - 1..=kk + 1].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, v)| (kk - 1 + i, v)).unwrap();
        if v > 0.4 * pk && v > 5.0 * around {
            k = j;
            break;
        }
    }
    // Parabolic refinement.
    let (a, b, c) = (mag[k - 1], mag[k], mag[k + 1]);
    let d = 0.5 * (a - c) / (a - 2.0 * b + c);
    let raw = (k as f64 + d as f64) * bin_hz;
    let snapped = STANDARD_RATES.iter().copied().find(|r| (raw - r).abs() / r < 0.03);
    (snapped, Some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fsk(levels: &[f64], baud: f64, dev: f64, fs: f64, n: usize) -> Vec<Complex32> {
        let mut x = 0x1234_5678u64;
        let sps = fs / baud;
        let mut phase = 0.0f64;
        let mut f = 0.0;
        (0..n)
            .map(|i| {
                if (i as f64 % sps) < 1.0 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    f = levels[(x >> 33) as usize % levels.len()] * dev;
                }
                phase += 2.0 * std::f64::consts::PI * f / fs;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn four_fsk_4800() {
        let fs = 50_000.0;
        let x = fsk(&[-1.0, -1.0 / 3.0, 1.0 / 3.0, 1.0], 4800.0, 1800.0, fs, 100_000);
        let d = discriminator(&x, fs);
        let f = extract(&x, &d, fs);
        assert_eq!(f.fsk_levels, 4, "{f:?}");
        assert_eq!(f.symbol_rate, Some(4800.0), "{f:?}");
        assert!(f.envelope_cv < 0.05);
        assert!((f.deviation_hz - 1800.0).abs() < 200.0, "{f:?}");
    }

    #[test]
    fn two_fsk_1200() {
        let fs = 50_000.0;
        let x = fsk(&[-1.0, 1.0], 1200.0, 4500.0, fs, 100_000);
        let d = discriminator(&x, fs);
        let f = extract(&x, &d, fs);
        assert_eq!(f.fsk_levels, 2, "{f:?}");
        assert_eq!(f.symbol_rate, Some(1200.0), "{f:?}");
    }
}
