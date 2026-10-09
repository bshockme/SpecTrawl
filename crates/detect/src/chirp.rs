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
        let mx = (n - 1.0) / 2.0;
        let my = win.iter().map(|v| *v as f64).sum::<f64>() / n;
        let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
        for (i, v) in win.iter().enumerate() {
            let (dx, dy) = (i as f64 - mx, *v as f64 - my);
            sxy += dx * dy;
            sxx += dx * dx;
            syy += dy * dy;
        }
        let slope = sxy / sxx; // Hz per sample
        let r2 = if syy > 0.0 { sxy * sxy / (sxx * syy) } else { 0.0 };
        let sweep = (slope * n).abs();
        if r2 > 0.9 && sweep > 0.1 * range {
            ramps.push(slope * fs);
        } else if (syy / n).sqrt() < 0.05 * range {
            levels += 1;
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
/// Returns (bandwidth Hz, spreading factor) when a burst's ramps match one.
pub fn lora_params(st: &ChirpStats) -> Option<(f64, u8)> {
    if st.ramp_fraction < 0.5 {
        return None;
    }
    let bw = st.f_hi_hz - st.f_lo_hz;
    let rate = st.slope_hz_per_s.abs();
    [125e3, 250e3, 500e3].into_iter().filter(|b| (bw - b * 0.9).abs() < 0.25 * b).find_map(|b| {
        (7u8..=12)
            .find(|&sf| {
                let r = b * b / (1u64 << sf) as f64;
                (rate - r).abs() < 0.2 * r
            })
            .map(|sf| (b, sf))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LoRa-like upchirps: `bw` wide, 2^sf samples per symbol at fs = bw,
    /// upsampled ×4.
    fn lora(bw: f64, sf: u8, symbols: usize) -> (Vec<Complex32>, f64) {
        let fs = bw * 4.0;
        let n = (1usize << sf) * 4;
        let mut x = vec![Complex32::new(0.01, 0.0); 20_000];
        let mut ph = 0.0f64;
        for s in 0..symbols {
            let off = (s * 37 % (1 << sf)) as f64 / (1 << sf) as f64; // symbol value shifts the start
            for i in 0..n {
                let t = (i as f64 / n as f64 + off) % 1.0;
                let f = -bw / 2.0 + bw * t;
                ph += 2.0 * std::f64::consts::PI * f / fs;
                x.push(Complex32::from_polar(1.0, ph as f32));
            }
        }
        x.extend(vec![Complex32::new(0.01, 0.0); 20_000]);
        (x, fs)
    }

    #[test]
    fn finds_lora_sf7_125k() {
        let (x, fs) = lora(125e3, 7, 40);
        let b = bursts(&x, fs);
        assert_eq!(b.len(), 1, "{b:?}");
        let st = analyse(&x[b[0].0..b[0].1], fs);
        assert_eq!(lora_params(&st), Some((125e3, 7)), "{st:?}");
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
        assert_eq!(lora_params(&st), None);
    }
}
