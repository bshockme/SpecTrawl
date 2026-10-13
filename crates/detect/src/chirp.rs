//! Bursts and chirps: splits a channel into bursts, and measures how much of
//! each burst's instantaneous frequency follows straight ramps (chirp spread
//! spectrum, as LoRa sends: a sweep across its bandwidth per symbol) versus
//! holding steady levels (FSK) or neither.

use num_complex::Complex32;

/// Sample ranges where the signal is on: envelope over 10 dB above the
/// channel's quiet level, at least 0.5 ms long, gaps under 1 ms bridged.
pub fn bursts(x: &[Complex32], fs: f64) -> Vec<(usize, usize)> {
    let w = ((fs * 2e-4) as usize).max(1);
    let p: Vec<f32> = x.iter().map(|c| c.norm_sqr()).collect();
    let mut env = vec![0.0f32; p.len()];
    let mut acc = 0.0f32;
    for i in 0..p.len() {
        acc += p[i];
        if i >= w {
            acc -= p[i - w];
        }
        env[i] = acc / w.min(i + 1) as f32;
    }
    let mut s = env.clone();
    if s.is_empty() {
        return Vec::new();
    }
    s.sort_by(f32::total_cmp);
    let (quiet, busy) = (s[s.len() / 20], s[s.len() * 19 / 20]);
    // On nearly all the time (a carrier, a long transmission): one burst.
    if busy < quiet * 10.0 {
        return if busy > 0.0 { vec![(0, x.len())] } else { Vec::new() };
    }
    let thr = quiet.max(busy * 1e-4) * 10.0;
    let (min_len, gap) = ((fs * 5e-4) as usize, (fs * 1e-3) as usize);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < env.len() {
        if env[i] <= thr {
            i += 1;
            continue;
        }
        let a = i;
        while i < env.len() && env[i] > thr {
            i += 1;
        }
        match out.last_mut() {
            Some(l) if a - l.1 < gap => l.1 = i,
            _ => out.push((a, i)),
        }
    }
    out.retain(|b| b.1 - b.0 >= min_len);
    out
}

#[derive(Debug, Clone, Default)]
pub struct ChirpStats {
    /// Instantaneous frequency range (5th-95th percentile), Hz.
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    /// Fraction of 0.25 ms windows whose frequency is a clean ramp.
    pub ramp_fraction: f32,
    /// Median slope of the ramp windows, Hz/s.
    pub slope_hz_per_s: f64,
    /// Fraction of windows whose frequency holds steady.
    pub level_fraction: f32,
}

pub fn analyse(x: &[Complex32], fs: f64) -> ChirpStats {
    let mut st = ChirpStats::default();
    let d = crate::features::discriminator(x, fs);
    if d.len() < 64 {
        return st;
    }
    // Smooth over ~50 µs: keeps chirps, removes most noise.
    let sm = crate::frames::boxcar(&d, ((fs * 5e-5) as usize).max(1));
    let mut s = sm.clone();
    s.sort_by(f32::total_cmp);
    let q = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)] as f64;
    st.f_lo_hz = q(0.05);
    st.f_hi_hz = q(0.95);
    let range = (st.f_hi_hz - st.f_lo_hz).max(1.0);
    let w = ((fs * 2.5e-4) as usize).max(8);
    let (mut ramps, mut levels, mut total) = (Vec::new(), 0usize, 0usize);
    for win in sm.chunks_exact(w) {
        total += 1;
        let n = win.len() as f64;
        let (slope, r2) = fit(win); // Hz per sample
        let sweep = (slope * n).abs();
        if r2 > 0.9 && sweep > 0.1 * range {
            ramps.push(slope * fs);
        } else {
            let my = win.iter().map(|v| *v as f64).sum::<f64>() / n;
            let var = win.iter().map(|v| (*v as f64 - my).powi(2)).sum::<f64>() / n;
            if var.sqrt() < 0.05 * range {
                levels += 1;
            }
        }
    }
    if total > 0 {
        st.ramp_fraction = ramps.len() as f32 / total as f32;
        st.level_fraction = levels as f32 / total as f32;
    }
    if !ramps.is_empty() {
        ramps.sort_by(f64::total_cmp);
        st.slope_hz_per_s = ramps[ramps.len() / 2];
    }
    st
}

/// LoRa: chirps across 125/250/500 kHz at a rate of BW²/2^SF (SF 7-12).
/// Returns (bandwidth Hz, spreading factor) when the burst is made of such
/// chirps.
///
/// Dechirping: multiplied by the conjugate of a candidate's reference
/// up-chirp, every LoRa symbol of that bandwidth and spreading factor
/// becomes a tone (two tones, a bandwidth apart, where the symbol's cyclic
/// shift wraps), so a symbol-length FFT of each frame puts nearly all of
/// its power in a few bins. Any other candidate, or FSK, noise or a
/// carrier, leaves a residual chirp spread over many bins. This judges each
/// mode on its own symbol length, which `analyse`'s fixed 0.25 ms windows
/// could not: a 33 ms SF12 symbol looked like a steady level there and a
/// 0.26 ms SF7/500 kHz symbol wrapped inside nearly every window.
pub fn lora(x: &[Complex32], fs: f64) -> Option<(f64, u8)> {
    if x.len() < 64 {
        return None;
    }
    // Only bandwidths the instantaneous frequency actually spans: a chirp
    // spends equal time at every frequency, so its 5-95 % range is 0.9 BW.
    let d = crate::features::discriminator(x, fs);
    let mut s = crate::frames::boxcar(&d, ((fs * 5e-5) as usize).max(1));
    s.sort_by(f32::total_cmp);
    let q = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)] as f64;
    let range = q(0.95) - q(0.05);
    let mut planner = rustfft::FftPlanner::<f32>::new();
    for b in [125e3, 250e3, 500e3] {
        if (range - b * 0.9).abs() >= 0.25 * b {
            continue;
        }
        for sf in 7u8..=12 {
            let n = (fs * (1u64 << sf) as f64 / b).round() as usize;
            let frames = x.len() / n;
            if n < 16 || frames < 2 {
                continue;
            }
            // Reference up-chirp −BW/2 → +BW/2 over one symbol, conjugated.
            let reference: Vec<Complex32> = (0..n)
                .map(|i| {
                    let t = i as f64 / fs;
                    let ph = 2.0 * std::f64::consts::PI * (-b / 2.0 * t + b * b / (1u64 << sf) as f64 / 2.0 * t * t);
                    Complex32::from_polar(1.0, -ph as f32)
                })
                .collect();
            let fft = planner.plan_fft_forward(n);
            let mut buf = vec![Complex32::default(); n];
            let mut power = vec![0f32; n];
            let concentrated = x
                .chunks_exact(n)
                .filter(|frame| {
                    for ((o, a), r) in buf.iter_mut().zip(*frame).zip(&reference) {
                        *o = a * r;
                    }
                    fft.process(&mut buf);
                    for (p, v) in power.iter_mut().zip(&buf) {
                        *p = v.norm_sqr();
                    }
                    let total: f32 = power.iter().sum();
                    // Up to four tones per frame (two symbols, each possibly
                    // wrapped), each leaking into two bins: the top eight
                    // bins hold most of a LoRa frame's power.
                    let (_, eighth, above) = power.select_nth_unstable_by(n - 8, f32::total_cmp);
                    let peak = *eighth + above.iter().sum::<f32>();
                    total > 0.0 && peak > LORA_CONCENTRATION * total
                })
                .count();
            if concentrated * 2 >= frames {
                return Some((b, sf));
            }
        }
    }
    None
}

/// Fraction of a dechirped frame's power in its top eight bins for the
/// frame to count as a LoRa symbol: ~S/(S+N), 0.9 at 10 dB, 0.5 at 0 dB.
/// A wrong candidate spreads the power over 2^SF/2 bins or more.
const LORA_CONCENTRATION: f32 = 0.4;

/// Least-squares line through `win`: (slope per sample, r²).
fn fit(win: &[f32]) -> (f64, f64) {
    let n = win.len() as f64;
    let mx = (n - 1.0) / 2.0;
    let my = win.iter().map(|v| *v as f64).sum::<f64>() / n;
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (i, v) in win.iter().enumerate() {
        let (dx, dy) = (i as f64 - mx, *v as f64 - my);
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    let slope = sxy / sxx;
    let r2 = if syy > 0.0 { sxy * sxy / (sxx * syy) } else { 0.0 };
    (slope, r2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LoRa-like upchirps: `bw` wide, 2^sf samples per symbol at fs = bw,
    /// upsampled ×4.
    fn lora_signal(bw: f64, sf: u8, symbols: usize) -> (Vec<Complex32>, f64) {
        let fs = bw * 4.0;
        let (x, _) = chirps(bw, sf, symbols, fs, 0.0, 0.0);
        let mut out = vec![Complex32::new(0.01, 0.0); 20_000];
        out.extend(x);
        out.extend(vec![Complex32::new(0.01, 0.0); 20_000]);
        (out, fs)
    }

    /// `symbols` cyclically shifted upchirps at `fs`, the carrier `offset_hz`
    /// off centre, with Gaussian noise `snr_db` below the chirp.
    fn chirps(bw: f64, sf: u8, symbols: usize, fs: f64, offset_hz: f64, snr_db: f64) -> (Vec<Complex32>, f64) {
        let n = ((1u64 << sf) as f64 * fs / bw).round() as usize;
        let sigma = if snr_db.is_finite() { (10f64.powf(-snr_db / 10.0) / 2.0).sqrt() } else { 0.0 };
        let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ (sf as u64) << 40 ^ bw as u64;
        let mut uni = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
        };
        let mut ph = 0.0f64;
        let mut x = Vec::with_capacity(n * symbols);
        for s in 0..symbols {
            let off = (s * 37 % (1 << sf)) as f64 / (1 << sf) as f64; // symbol value shifts the start
            for i in 0..n {
                let t = (i as f64 / n as f64 + off) % 1.0;
                let f = -bw / 2.0 + bw * t + offset_hz;
                ph += 2.0 * std::f64::consts::PI * f / fs;
                let r = sigma * (-2.0 * uni().ln()).sqrt();
                let th = 2.0 * std::f64::consts::PI * uni();
                x.push(Complex32::from_polar(1.0, ph as f32) + Complex32::new((r * th.cos()) as f32, (r * th.sin()) as f32));
            }
        }
        (x, fs)
    }

    #[test]
    fn finds_lora_sf7_125k() {
        let (x, fs) = lora_signal(125e3, 7, 40);
        let b = bursts(&x, fs);
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!(lora(&x[b[0].0..b[0].1], fs), Some((125e3, 7)));
    }

    /// Every advertised mode, at the channel rate the classifier uses
    /// (2.5 × the bandwidth), off centre, 10 dB above Gaussian noise.
    /// The review found the fixed-window analysis rejected 125 kHz SF9-12,
    /// 250 kHz SF10-12 and 500 kHz SF11-12 even without noise.
    #[test]
    fn finds_every_lora_mode_with_noise_and_offset() {
        for bw in [125e3, 250e3, 500e3] {
            for sf in 7u8..=12 {
                let (x, fs) = chirps(bw, sf, 8, bw * 2.5, 0.1 * bw, 10.0);
                assert_eq!(lora(&x, fs), Some((bw, sf)), "{bw} Hz SF{sf}");
            }
        }
    }

    #[test]
    fn noise_and_carriers_are_not_lora() {
        let fs = 312.5e3;
        let (noise, _) = chirps(125e3, 7, 1, fs, 0.0, -40.0);
        assert_eq!(lora(&noise, fs), None);
        let carrier: Vec<Complex32> = (0..50_000).map(|i| Complex32::from_polar(1.0, (0.3 * i as f64) as f32)).collect();
        assert_eq!(lora(&carrier, fs), None);
    }

    #[test]
    fn fsk_is_not_a_chirp() {
        let fs = 200e3;
        let mut ph = 0.0f64;
        let mut x = vec![Complex32::new(0.01, 0.0); 40_000];
        for i in 0..40_000usize {
            let f = if (i / 400) % 2 == 0 { 20e3 } else { -20e3 };
            ph += 2.0 * std::f64::consts::PI * f / fs;
            x.push(Complex32::from_polar(1.0, ph as f32));
        }
        let b = bursts(&x, fs);
        let st = analyse(&x[b[0].0..b[0].1], fs);
        assert!(st.ramp_fraction < 0.2 && st.level_fraction > 0.6, "{st:?}");
        assert_eq!(lora(&x[b[0].0..b[0].1], fs), None);
        // Wide FSK whose range matches a LoRa bandwidth, with the symbol
        // transitions where a chirp's wraps would be.
        let fs = 312.5e3;
        let mut ph = 0.0f64;
        let x: Vec<Complex32> = (0..200_000usize)
            .map(|i| {
                let f = if (i / 128) % 2 == 0 { 56e3 } else { -56e3 };
                ph += 2.0 * std::f64::consts::PI * f / fs;
                Complex32::from_polar(1.0, ph as f32)
            })
            .collect();
        assert_eq!(lora(&x, fs), None);
    }
}
