//! Blind estimators for signals the sync bank doesn't know: the modulation
//! family and its parameters from the signal's own second-order structure.
//! These are what the inspector tools (suscan's symbol-rate detectors,
//! gr-inspector's OFDM estimator, URH's auto-interpretation) and the
//! textbook blind-recognition features do, reduced to what a channel
//! capture of a few hundred ms can support:
//!
//! - **Cyclic line of the envelope** (`envelope_rate`): the spectrum of
//!   |x|² has a line at the symbol rate of any linearly modulated signal
//!   whose pulse shaping is not perfectly flat (PSK, QAM, ASK, filtered
//!   FSK), and at the pulse rate of keyed signals. Constant-envelope FSK
//!   has none; its rate comes from the discriminator (`features`).
//! - **Power-law lines** (`psk_order`): raising M-PSK to the M-th power
//!   wipes the modulation and leaves a carrier line at M times the
//!   frequency offset. BPSK shows a line at x², QPSK only from x⁴, 8PSK
//!   only from x⁸. A plain carrier or AM shows a line already at x¹ and is
//!   excluded.
//! - **Cyclic-prefix autocorrelation** (`ofdm`): an OFDM symbol repeats
//!   its last T_cp at its start, so x correlates with itself at a lag of
//!   exactly the useful symbol length T_u; the lag gives the subcarrier
//!   spacing 1/T_u and the peak's height the guard fraction.
//! - **Envelope periodicity** (`burst_period`): a slotted or framed
//!   signal's envelope repeats at the frame period (TDMA slots, beacons,
//!   pagers' batches); the autocorrelation of the envelope shows it.
//! - **FM broadcast** (`fm_broadcast`): a broadcast station's discriminator
//!   carries a 19 kHz stereo pilot or an RDS subcarrier at 57 kHz (a mono
//!   station has only RDS), either as decisive as a sync word.
//!
//! Every test compares a line against the periodogram's own background
//! and asks for it in both halves of the window, since the maximum of a
//! flat periodogram over N bins is already ln N times its mean.

use num_complex::Complex32;
use rustfft::FftPlanner;

/// Spectral line found by one of the tests: its frequency and how far it
/// stands over the periodogram's median, in dB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    pub hz: f64,
    pub over_db: f32,
}

/// Least height over the median for a line in a single periodogram, dB.
/// The peak of an exponential (chi-squared, 2 dof) periodogram over N bins
/// sits about ln N / ln 2 times the median (15× ≈ 12 dB at N = 32768),
/// and the test asks for it in both halves, so 13 dB is already a rare
/// false alarm; 15 dB leaves margin for coloured noise.
const LINE_DB: f32 = 15.0;

/// Largest FFT used by the tests (samples). A longer window is split.
const MAX_FFT: usize = 1 << 16;

/// Periodogram of a real or complex sequence, Hann-windowed, mean removed:
/// power per bin, bin 0 = DC, bins above n/2 are negative frequencies.
fn periodogram(x: &[Complex32]) -> Vec<f32> {
    let n = x.len();
    let mean = x.iter().sum::<Complex32>() / n as f32;
    let mut buf: Vec<Complex32> = x
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
            (v - mean) * w
        })
        .collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    buf.iter().map(|c| c.norm_sqr()).collect()
}

/// The strongest line in `pw[lo..hi]` and its height over the median of
/// the same range (the median of an exponential periodogram is 0.69 × its
/// mean; a line's own bins hardly move it).
fn strongest(pw: &[f32], lo: usize, hi: usize) -> Option<(usize, f32)> {
    if hi <= lo + 8 || hi > pw.len() {
        return None;
    }
    let (k, pk) = pw[lo..hi].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, v)| (i + lo, *v))?;
    let mut s: Vec<f32> = pw[lo..hi].to_vec();
    let mid = s.len() / 2;
    let med = *s.select_nth_unstable_by(mid, f32::total_cmp).1;
    if med <= 0.0 || pk <= 0.0 {
        return None;
    }
    Some((k, 10.0 * (pk / med).log10()))
}

/// Parabolic interpolation of a peak at bin `k`, in bins.
fn refine(pw: &[f32], k: usize) -> f64 {
    if k == 0 || k + 1 >= pw.len() {
        return k as f64;
    }
    let (a, b, c) = (pw[k - 1], pw[k], pw[k + 1]);
    let den = a - 2.0 * b + c;
    if den.abs() < 1e-12 { k as f64 } else { k as f64 + (0.5 * (a - c) / den) as f64 }
}

/// A line that stands `LINE_DB` over the median in both halves of `x`,
/// in the same place: `f(x)` maps a half to the sequence whose
/// periodogram is searched, `lo_hz..hi_hz` is the search range (positive
/// frequencies only), `fs` the sample rate of that sequence.
fn stable_line<F: Fn(&[Complex32]) -> Vec<Complex32>>(x: &[Complex32], fs: f64, lo_hz: f64, hi_hz: f64, f: F) -> Option<Line> {
    let half = (x.len() / 2).min(MAX_FFT);
    if half < 1024 {
        return None;
    }
    let n = 1usize << half.ilog2();
    let bin_hz = fs / n as f64;
    let lo = ((lo_hz / bin_hz).ceil() as usize).max(1);
    let hi = ((hi_hz / bin_hz) as usize).min(n / 2);
    let mut found: Option<(f64, f32)> = None;
    for part in [&x[..n], &x[x.len() - n..]] {
        let y = f(part);
        let pw = periodogram(&y[..n]);
        let (k, over) = strongest(&pw, lo, hi)?;
        if over < LINE_DB {
            return None;
        }
        let hz = refine(&pw, k) * bin_hz;
        match found {
            None => found = Some((hz, over)),
            // Same line in both halves: within 3 bins or 1 %.
            Some((h0, o0)) => {
                if (hz - h0).abs() > (3.0 * bin_hz).max(0.01 * h0) {
                    return None;
                }
                return Some(Line { hz: (hz + h0) / 2.0, over_db: o0.min(over) });
            }
        }
    }
    None
}

/// The symbol (or pulse) rate from the cyclic line of the squared
/// envelope, searched between `lo_hz` and `hi_hz`. Lines at multiples of
/// the rate are folded down to the lowest one that is itself a line.
pub fn envelope_rate(x: &[Complex32], fs: f64, lo_hz: f64, hi_hz: f64) -> Option<Line> {
    let env = |p: &[Complex32]| -> Vec<Complex32> { p.iter().map(|c| Complex32::new(c.norm_sqr(), 0.0)).collect() };
    let line = stable_line(x, fs, lo_hz, hi_hz.min(fs / 2.0), env)?;
    // Prefer a sub-harmonic that is also a clear line (rectangular pulses
    // put most of the line power at the rate, but a square-ish keying at
    // half rate reads at twice the rate when the fundamental is weak).
    for div in [3.0, 2.0] {
        let sub = line.hz / div;
        if sub < lo_hz {
            continue;
        }
        if let Some(l) = stable_line(x, fs, sub * 0.97, sub * 1.03, env)
            && l.over_db >= LINE_DB
        {
            return Some(l);
        }
    }
    Some(line)
}

/// M-PSK found by the power-law test, with the carrier line's offset from
/// the channel centre (the line sits at M × offset).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psk {
    /// 2, 4 or 8.
    pub order: u8,
    pub offset_hz: f64,
    pub over_db: f32,
}

/// Lowest M in {2, 4, 8} whose M-th power of `x` shows a stable carrier
/// line; None when `x` itself already has one (a carrier, AM, a tone) or
/// no power shows any. The line is searched over ±`max_offset_hz` × M.
pub fn psk_order(x: &[Complex32], fs: f64, max_offset_hz: f64) -> Option<Psk> {
    if x.len() < 2048 {
        return None;
    }
    // A line in x itself: not PSK (or a carrier with PSK, which the power
    // test can't tell from AM). Searched over the whole band, and again
    // below at the found line's own frequency with half the bar, since
    // squaring doubles a weak carrier's decibels over the noise.
    if stable_two_sided(x, fs, fs / 2.0, |p| p.to_vec()).is_some() {
        return None;
    }
    let half = (x.len() / 2).min(MAX_FFT);
    let n = 1usize << half.ilog2();
    let halves = [periodogram(&x[..n]), periodogram(&x[x.len() - n..])];
    // x's own line at a frequency, the weaker of the two halves against
    // its neighbourhood (one chance peak of a flat periodogram reaches 7
    // dB over its median often; two in the same place don't).
    let own_line_db = |hz: f64| -> f32 {
        let k = ((hz / fs * n as f64).round() as isize).rem_euclid(n as isize) as usize;
        halves.iter().map(|pw| local_line_db(pw, k)).fold(f32::MAX, f32::min)
    };
    for order in [2u8, 4, 8] {
        let pow = |p: &[Complex32]| -> Vec<Complex32> {
            p.iter()
                .map(|c| {
                    let mut y = *c;
                    for _ in 1..order {
                        y *= c;
                    }
                    y
                })
                .collect()
        };
        let range = (max_offset_hz * order as f64).min(fs / 2.0);
        if let Some(l) = stable_two_sided(x, fs, range, pow) {
            let offset_hz = l.hz / order as f64;
            if own_line_db(offset_hz) >= OWN_LINE_DB {
                return None;
            }
            // One carrier line, not a pair: FSK raised to a power leaves
            // lines at M × (offset ± deviation), of equal strength.
            let pw = periodogram(&pow(&x[..n]));
            let k = ((l.hz / fs * n as f64).round() as isize).rem_euclid(n as isize) as usize;
            let r = ((range / fs * n as f64) as usize).min(n / 2 - 1);
            let guard = ((0.01 * k.min(n - k) as f64) as usize).max(5);
            let second = (0..=2 * r)
                .map(|i| (i + n - r) % n)
                .filter(|&j| j.abs_diff(k).min(n - j.abs_diff(k)) > guard)
                .map(|j| pw[j])
                .fold(0.0, f32::max);
            if order <= 4 && second > 0.0 && 10.0 * (pw[k] / second).log10() < PSK_PARTNER_DB {
                return None;
            }
            return Some(Psk { order, offset_hz, over_db: l.over_db });
        }
    }
    None
}

/// A line already in x at the carrier frequency, this far over its
/// neighbourhood in both halves, says the signal carries a carrier (AM, a
/// keyed carrier, an FSK tone) rather than suppressing one.
const OWN_LINE_DB: f32 = 10.0;

/// A PSK carrier line must stand this far over the next strongest line in
/// the same power spectrum (for x² and x⁴; the eighth power of anything
/// is too noisy for the comparison).
const PSK_PARTNER_DB: f32 = 6.0;

/// A line at bin `k` of a periodogram against the median of its
/// neighbourhood (±`LOCAL_BINS`, the line's own bins left out), dB.
const LOCAL_BINS: usize = 64;
fn local_line_db(pw: &[f32], k: usize) -> f32 {
    let n = pw.len();
    let pk = (k.saturating_sub(1)..(k + 2).min(n)).map(|i| pw[i]).fold(0.0, f32::max);
    let mut near: Vec<f32> =
        (k.saturating_sub(LOCAL_BINS)..(k + LOCAL_BINS + 1).min(n)).filter(|i| i.abs_diff(k) > 2).map(|i| pw[i]).collect();
    if near.len() < 8 || pk <= 0.0 {
        return 0.0;
    }
    let m = near.len() / 2;
    let med = *near.select_nth_unstable_by(m, f32::total_cmp).1;
    if med <= 0.0 { 0.0 } else { 10.0 * (pk / med).log10() }
}

/// `stable_line` over both signs of frequency, −range..range about DC.
fn stable_two_sided<F: Fn(&[Complex32]) -> Vec<Complex32>>(x: &[Complex32], fs: f64, range_hz: f64, f: F) -> Option<Line> {
    let half = (x.len() / 2).min(MAX_FFT);
    if half < 1024 {
        return None;
    }
    let n = 1usize << half.ilog2();
    let bin_hz = fs / n as f64;
    let r = ((range_hz / bin_hz) as usize).min(n / 2 - 1);
    let mut found: Option<(f64, f32)> = None;
    for part in [&x[..n], &x[x.len() - n..]] {
        let y = f(part);
        let mut pw = periodogram(&y[..n]);
        // Rotate so the searched range is contiguous: bins n−r..n, 0..=r.
        pw.rotate_right(r);
        let (k, over) = strongest(&pw, 0, 2 * r + 1)?;
        if over < LINE_DB {
            return None;
        }
        let hz = (refine(&pw, k) - r as f64) * bin_hz;
        match found {
            None => found = Some((hz, over)),
            Some((h0, o0)) => {
                if (hz - h0).abs() > 3.0 * bin_hz {
                    return None;
                }
                return Some(Line { hz: (hz + h0) / 2.0, over_db: o0.min(over) });
            }
        }
    }
    None
}

/// OFDM parameters from the cyclic-prefix autocorrelation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ofdm {
    /// Useful symbol length, s (1 / subcarrier spacing).
    pub useful_s: f64,
    pub subcarrier_hz: f64,
    /// |R(T_u)| / R(0): about T_cp / (T_u + T_cp) × the signal's share of
    /// the power, so 0.05–0.2 on a clean signal.
    pub peak: f32,
    /// The peak over the autocorrelation's background (its median over the
    /// searched lags), as a ratio.
    pub over: f32,
}

/// Least cyclic-prefix correlation peak taken as OFDM, and how far it
/// must stand over the other lags. Noise correlates at ~1/√N (0.004 for
/// N = 65536); a guard of 1/32 on a signal 10 dB over noise gives 0.028.
const OFDM_MIN_PEAK: f32 = 0.02;
const OFDM_OVER: f32 = 8.0;

/// Looks for an OFDM symbol structure: a correlation peak at one lag
/// between `min_useful_s` and `max_useful_s`, well over every other lag
/// in that range.
pub fn ofdm(x: &[Complex32], fs: f64, min_useful_s: f64, max_useful_s: f64) -> Option<Ofdm> {
    let n = x.len().min(MAX_FFT * 2);
    let x = &x[x.len() - n..];
    let tau_lo = ((min_useful_s * fs) as usize).max(4);
    let tau_hi = ((max_useful_s * fs) as usize).min(n / 4);
    if tau_hi <= tau_lo + 8 {
        return None;
    }
    // Autocorrelation by FFT: zero-pad to 2n so it is linear, not circular.
    let m = (2 * n).next_power_of_two();
    let mut buf: Vec<Complex32> = x.iter().copied().chain(std::iter::repeat(Complex32::default())).take(m).collect();
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(m).process(&mut buf);
    for c in buf.iter_mut() {
        *c = Complex32::new(c.norm_sqr(), 0.0);
    }
    planner.plan_fft_inverse(m).process(&mut buf);
    let r0 = buf[0].re;
    if r0 <= 0.0 {
        return None;
    }
    // Normalise each lag by the samples that overlap at it.
    let r: Vec<f32> = (tau_lo..tau_hi).map(|t| buf[t].norm() / r0 * (n as f32 / (n - t) as f32)).collect();
    let (k, pk) = r.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, v)| (i, *v))?;
    let mut s = r.clone();
    let mid = s.len() / 2;
    let med = *s.select_nth_unstable_by(mid, f32::total_cmp).1;
    if pk < OFDM_MIN_PEAK || pk < OFDM_OVER * med.max(1e-9) {
        return None;
    }
    // A sharp peak: the lag either side must already be well down (a
    // slowly varying autocorrelation, as an FM carrier or a tone gives,
    // has no such peak). The peak's width is about one sample for a
    // signal filling the channel.
    let side = |i: isize| -> f32 {
        let j = k as isize + i;
        if j < 0 || j as usize >= r.len() { f32::MAX } else { r[j as usize] }
    };
    if side(-3).max(side(3)) > 0.5 * pk {
        return None;
    }
    // ...and isolated: the correlation must have fallen away between the
    // short lags (where any band-limited signal correlates with itself,
    // a shaped PSK pulse over several samples) and the peak.
    if k < 3 || r[..k].iter().copied().fold(f32::MAX, f32::min) > 0.3 * pk {
        return None;
    }
    let tau = (tau_lo + k) as f64 + {
        let (a, b, c) = (side(-1), pk, side(1));
        let den = a - 2.0 * b + c;
        if den.abs() < 1e-12 { 0.0 } else { (0.5 * (a - c) / den) as f64 }
    };
    let useful_s = tau / fs;
    Some(Ofdm { useful_s, subcarrier_hz: 1.0 / useful_s, peak: pk, over: pk / med.max(1e-9) })
}

/// A period at which the envelope repeats: the first clear peak of the
/// envelope's autocorrelation between `min_s` and `max_s`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Period {
    pub period_s: f64,
    /// Normalised autocorrelation of the (mean-removed) envelope at the
    /// period: 1 for a perfectly repeating envelope.
    pub strength: f32,
}

/// Least normalised envelope autocorrelation at a period for it to count.
const PERIOD_MIN: f32 = 0.3;

/// Envelope periodicity (see `Period`). The envelope is decimated to
/// about 2 kHz first so the lags are cheap; periods shorter than 2 ms
/// aren't looked for.
pub fn burst_period(x: &[Complex32], fs: f64, min_s: f64, max_s: f64) -> Option<Period> {
    let dec = ((fs / 2000.0) as usize).max(1);
    let env: Vec<f32> = x.chunks(dec).map(|c| c.iter().map(|v| v.norm_sqr()).sum::<f32>() / c.len() as f32).collect();
    let fs_e = fs / dec as f64;
    let n = env.len();
    if n < 64 {
        return None;
    }
    let mean = env.iter().sum::<f32>() / n as f32;
    let e: Vec<f32> = env.iter().map(|v| v - mean).collect();
    let r0: f32 = e.iter().map(|v| v * v).sum();
    if r0 <= 0.0 {
        return None;
    }
    let lag_lo = ((min_s.max(0.002) * fs_e) as usize).max(2);
    let lag_hi = ((max_s * fs_e) as usize).min(n / 3);
    if lag_hi <= lag_lo + 2 {
        return None;
    }
    let r: Vec<f32> = (lag_lo..lag_hi)
        .map(|t| {
            let s: f32 = e[..n - t].iter().zip(&e[t..]).map(|(a, b)| a * b).sum();
            s / r0 * (n as f32 / (n - t) as f32)
        })
        .collect();
    // First local maximum above the threshold (the fundamental period),
    // rather than the global one (a multiple can be stronger by chance),
    // and only after the autocorrelation has come down from lag zero (a
    // shoulder of the envelope's own correlation width is not a period).
    let dip = (1..r.len() - 1).find(|&i| r[i] <= r[i - 1] && r[i] < r[i + 1] && r[i] < PERIOD_MIN)?;
    for i in dip + 1..r.len() - 1 {
        if r[i] >= PERIOD_MIN && r[i] >= r[i - 1] && r[i] > r[i + 1] {
            let (a, b, c) = (r[i - 1], r[i], r[i + 1]);
            let den = a - 2.0 * b + c;
            let d = if den.abs() < 1e-12 { 0.0 } else { (0.5 * (a - c) / den) as f64 };
            return Some(Period { period_s: ((lag_lo + i) as f64 + d) / fs_e, strength: r[i] });
        }
    }
    None
}

/// FM broadcast signalling found in a discriminator output.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FmBroadcast {
    /// The 19 kHz stereo pilot, dB over the discriminator spectrum's
    /// median around it; `None` for a mono station.
    pub pilot_db: Option<f32>,
    /// RDS at 57 kHz, if the discriminator reaches that far and it is there.
    pub rds: bool,
}

/// A broadcast station's signalling in a discriminator output (`disc` at
/// `fs`). The stereo pilot: a line within ±25 Hz of 19 kHz, `LINE_DB` over
/// the 17–21 kHz median in both halves. RDS: a 57 kHz subcarrier ±2.4 kHz,
/// read as a bump 6 dB over the 50–54 and 60–64 kHz medians. Either names
/// a station: on air a mono station (105.9 MHz, no pilot at any window)
/// carried RDS 25 dB over its baseband's floor.
pub fn fm_broadcast(disc: &[f32], fs: f64) -> Option<FmBroadcast> {
    if fs < 44e3 {
        return None;
    }
    let x: Vec<Complex32> = disc.iter().map(|v| Complex32::new(*v, 0.0)).collect();
    let pilot_db = stable_line(&x, fs, 17e3, 21e3, |p| p.to_vec()).filter(|l| (l.hz - 19e3).abs() <= 25.0).map(|l| l.over_db);
    let mut rds = false;
    if fs > 130e3 {
        let n = 1usize << x.len().min(MAX_FFT).ilog2();
        let pw = periodogram(&x[..n]);
        let bin = fs / n as f64;
        let med = |lo: f64, hi: f64| -> f32 {
            let mut s: Vec<f32> = pw[(lo / bin) as usize..(hi / bin) as usize].to_vec();
            if s.is_empty() {
                return 0.0;
            }
            let m = s.len() / 2;
            *s.select_nth_unstable_by(m, f32::total_cmp).1
        };
        let sub = med(55.5e3, 58.5e3);
        // Beside it: the quieter of the two sides (a heavily processed
        // station's stereo subcarrier reaches up to 53 kHz).
        let beside = med(50e3, 54e3).min(med(60e3, 64e3));
        rds = beside > 0.0 && sub > 4.0 * beside;
    }
    (pilot_db.is_some() || rds).then_some(FmBroadcast { pilot_db, rds })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64, amp: f32) -> Vec<Complex32> {
        let mut seed = seed | 1;
        let mut uni = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
        };
        (0..n)
            .map(|_| {
                let r = (-2.0 * uni().ln()).sqrt() * std::f64::consts::FRAC_1_SQRT_2;
                let th = 2.0 * std::f64::consts::PI * uni();
                Complex32::new((r * th.cos()) as f32, (r * th.sin()) as f32) * amp
            })
            .collect()
    }

    /// Root-raised-cosine pulse, `sps` samples per symbol, `span` symbols.
    fn rrc(sps: usize, span: usize, beta: f64) -> Vec<f32> {
        let n = sps * span + 1;
        (0..n)
            .map(|i| {
                let t = (i as f64 - (n - 1) as f64 / 2.0) / sps as f64;
                let pi = std::f64::consts::PI;
                let v = if t.abs() < 1e-9 {
                    1.0 + beta * (4.0 / pi - 1.0)
                } else if (t.abs() - 1.0 / (4.0 * beta)).abs() < 1e-9 {
                    beta / 2f64.sqrt() * ((1.0 + 2.0 / pi) * (pi / (4.0 * beta)).sin() + (1.0 - 2.0 / pi) * (pi / (4.0 * beta)).cos())
                } else {
                    ((pi * t * (1.0 - beta)).sin() + 4.0 * beta * t * (pi * t * (1.0 + beta)).cos())
                        / (pi * t * (1.0 - (4.0 * beta * t).powi(2)))
                };
                v as f32
            })
            .collect()
    }

    /// M-PSK at `baud` with RRC shaping, offset by `offset_hz`, plus noise.
    fn psk(order: u32, baud: f64, fs: f64, secs: f64, offset_hz: f64, noise_amp: f32, seed: u64) -> Vec<Complex32> {
        let sps = (fs / baud).round() as usize;
        let nsym = (secs * baud) as usize;
        let h = rrc(sps, 8, 0.35);
        let mut x = seed | 1;
        let mut up = vec![Complex32::default(); nsym * sps + h.len()];
        for k in 0..nsym {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let m = (x >> 40) as u32 % order;
            let ph = 2.0 * std::f32::consts::PI * (m as f32 + 0.5) / order as f32;
            up[k * sps] = Complex32::from_polar(1.0, ph);
        }
        let mut y = vec![Complex32::default(); up.len()];
        for (i, out) in y.iter_mut().enumerate() {
            let mut acc = Complex32::default();
            for (j, hj) in h.iter().enumerate() {
                if i >= j {
                    acc += up[i - j] * *hj;
                }
            }
            *out = acc;
        }
        let nz = noise(y.len(), seed + 1, noise_amp);
        y.iter()
            .enumerate()
            .map(|(i, v)| {
                let ph = 2.0 * std::f64::consts::PI * offset_hz * i as f64 / fs;
                v * Complex32::from_polar(1.0, ph as f32) + nz[i]
            })
            .collect()
    }

    #[test]
    fn psk_orders_from_power_lines() {
        let fs = 48_000.0;
        for (order, want) in [(2u32, 2u8), (4, 4), (8, 8)] {
            let x = psk(order, 4800.0, fs, 2.0, 310.0, 0.1, 7);
            let p = psk_order(&x, fs, 2_000.0).unwrap_or_else(|| panic!("no PSK line for order {order}"));
            assert_eq!(p.order, want, "{p:?}");
            assert!((p.offset_hz - 310.0).abs() < 5.0, "{p:?}");
            let r = envelope_rate(&x, fs, 400.0, 12_000.0).expect("rate line");
            assert!((r.hz - 4800.0).abs() < 20.0, "{r:?}");
        }
        // Noise shows neither.
        let nz = noise(96_000, 3, 1.0);
        assert_eq!(psk_order(&nz, fs, 2_000.0), None);
        assert_eq!(envelope_rate(&nz, fs, 400.0, 12_000.0), None);
        // Nor does constant-envelope FSK (the line test on x itself is
        // what keeps a two-tone FSK's x² lines from reading as BPSK: its
        // x² has lines at ±2 dev, which are two lines, not one carrier...
        // and x itself has two lines when the data is slow).
        let mut ph = 0.0f64;
        let mut s = 0x9E37u64;
        let fsk: Vec<Complex32> = (0..96_000)
            .map(|i| {
                if i % 20 == 0 {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                }
                let f = if s >> 63 == 1 { 1200.0 } else { -1200.0 };
                ph += 2.0 * std::f64::consts::PI * f / fs;
                Complex32::from_polar(1.0, ph as f32)
            })
            .collect();
        assert_eq!(envelope_rate(&fsk, fs, 400.0, 12_000.0), None, "constant envelope has no cyclic line");
    }

    #[test]
    fn ofdm_cyclic_prefix() {
        // 64 subcarriers at 1 kHz spacing (T_u = 1 ms), CP 1/8, fs = 64 kHz.
        let fs = 64_000.0;
        let (nfft, cp) = (64usize, 8usize);
        let mut planner = FftPlanner::new();
        let ifft = planner.plan_fft_inverse(nfft);
        let mut seed = 0x1234_5678u64;
        let mut x = Vec::new();
        for _ in 0..1500 {
            let mut sym: Vec<Complex32> = (0..nfft)
                .map(|k| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    // 48 used subcarriers, QPSK.
                    if (8..56).contains(&k) {
                        Complex32::from_polar(1.0, std::f32::consts::FRAC_PI_2 * ((seed >> 40) % 4) as f32 + 0.3)
                    } else {
                        Complex32::default()
                    }
                })
                .collect();
            ifft.process(&mut sym);
            x.extend_from_slice(&sym[nfft - cp..]);
            x.extend_from_slice(&sym);
        }
        let nz = noise(x.len(), 9, 1.5);
        let y: Vec<Complex32> = x.iter().zip(&nz).map(|(a, b)| a + b).collect();
        let o = ofdm(&y, fs, 0.1e-3, 5e-3).expect("ofdm");
        assert!((o.useful_s - 1e-3).abs() < 0.02e-3, "{o:?}");
        assert!((o.subcarrier_hz - 1000.0).abs() < 20.0, "{o:?}");
        assert_eq!(ofdm(&nz, fs, 0.1e-3, 5e-3), None);
        // A PSK signal has no such peak either.
        let p = psk(4, 4800.0, 48_000.0, 2.0, 0.0, 0.3, 5);
        assert_eq!(ofdm(&p, 48_000.0, 0.1e-3, 5e-3), None);
    }

    #[test]
    fn burst_period_of_slots() {
        // A carrier keyed 15 ms on, 15 ms off (one TDMA slot of two).
        let fs = 48_000.0;
        let nz = noise(96_000, 4, 0.2);
        let x: Vec<Complex32> = (0..96_000)
            .map(|i| {
                let on = (i as f64 / fs % 0.030) < 0.015;
                Complex32::new(if on { 1.0 } else { 0.0 }, 0.0) + nz[i]
            })
            .collect();
        let p = burst_period(&x, fs, 0.005, 0.5).expect("period");
        assert!((p.period_s - 0.030).abs() < 0.001, "{p:?}");
        assert_eq!(burst_period(&nz, fs, 0.005, 0.5), None);
    }

    #[test]
    fn stereo_pilot() {
        // Discriminator output of a stereo station: L+R audio, 19 kHz
        // pilot at 9 % deviation, 38 kHz DSB, RDS at 57 kHz.
        let fs = 240_000.0;
        let mut seed = 77u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        // RDS data: 1187.5 bit/s biphase, held per bit so it stays within
        // ±2.4 kHz of its subcarrier.
        let mut bit = 0.0f32;
        let disc: Vec<f32> = (0..240_000)
            .map(|i| {
                let t = i as f64 / fs;
                if i % 101 == 0 {
                    bit = if rnd() > 0.0 { 1.0 } else { -1.0 };
                }
                let audio = (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 30e3 + rnd() * 20e3;
                let pilot = (2.0 * std::f64::consts::PI * 19e3 * t).sin() as f32 * 6.75e3;
                let rds = (2.0 * std::f64::consts::PI * 57e3 * t).cos() as f32 * bit * 4e3;
                audio + pilot + rds
            })
            .collect();
        let p = fm_broadcast(&disc, fs).expect("pilot");
        assert!(p.pilot_db.is_some() && p.rds, "{p:?}");
        // A mono station: the same without its pilot.
        let mono: Vec<f32> =
            disc.iter().enumerate().map(|(i, v)| v - (2.0 * std::f64::consts::PI * 19e3 * i as f64 / fs).sin() as f32 * 6.75e3).collect();
        assert_eq!(fm_broadcast(&mono, fs), Some(FmBroadcast { pilot_db: None, rds: true }));
        // Voice-like FM: neither.
        let plain: Vec<f32> = (0..240_000).map(|_| rnd() * 30e3).collect();
        assert_eq!(fm_broadcast(&plain, fs), None);
    }
}
