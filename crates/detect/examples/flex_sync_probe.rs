//! How many FLEX frames does the exact 64-bit sync 1 match lose? Scans a
//! channel capture for sync 1 at every eighth of a bit, allowing up to
//! `--max-err` bit errors, checks the frame information word on each, and
//! says which hits the decoder (exact match only) did not produce.
//!
//!     cargo run --release -p detect --example flex_sync_probe -- [--max-err N] <stem>...

use detect::{features, flex, pocsag};
use num_complex::Complex32;

const SPEEDS: [(u16, &str); 5] = [(0x870C, "1600/2"), (0xB068, "1600/4"), (0x7B18, "3200/2"), (0xDEA0, "3200/4"), (0x4C7C, "3200/4")];

fn load(stem: &str) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    (x, fs)
}

fn at(d: &[f32], t: f64) -> f32 {
    let i = (t.max(0.0) as usize).min(d.len() - 2);
    let f = (t - i as f64).clamp(0.0, 1.0) as f32;
    d[i] * (1.0 - f) + d[i + 1] * f
}

fn smooth(d: &[f32], n: usize) -> Vec<f32> {
    let mut s = d.to_vec();
    s.sort_by(f32::total_cmp);
    let mid = s[s.len() / 2];
    let mut out = vec![0.0; d.len()];
    let mut acc = 0.0f32;
    for i in 0..d.len() + n / 2 {
        if i < d.len() {
            acc += d[i] - mid;
        }
        if i >= n {
            acc -= d[i - n] - mid;
        }
        if i >= n / 2 && i - n / 2 < d.len() {
            out[i - n / 2] = acc / n as f32;
        }
    }
    out
}

fn data21(cw: u32) -> u32 {
    (cw >> 11).reverse_bits() >> 11
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let max_err = a.iter().position(|x| x == "--max-err").and_then(|i| a.get(i + 1)).and_then(|v| v.parse::<u32>().ok()).unwrap_or(6);
    for stem in a.iter().filter(|x| !x.starts_with("--") && x.parse::<u32>().is_err()) {
        let (x, fs) = load(stem);
        let d = smooth(&features::discriminator(&x, fs), (fs / 1600.0).round() as usize);
        let sps = fs / 1600.0;
        let decoded = flex::decode(&x, fs);
        // (time of sync-1 end, best error count, speed, inverted, FIW ok)
        let mut hits: Vec<(f64, u32, usize, bool, Option<(u8, u8)>)> = Vec::new();
        for p in 0..8 {
            let mut t = sps * p as f64 / 8.0;
            let mut w = 0u64;
            let mut n = 0;
            while t < (d.len() - 1) as f64 {
                w = w << 1 | (at(&d, t) < 0.0) as u64;
                n += 1;
                if n >= 64 {
                    for inv in [false, true] {
                        let v = if inv { !w } else { w };
                        for (si, (code, _)) in SPEEDS.iter().enumerate() {
                            let want = (*code as u64) << 48 | 0xA6C6_AAAAu64 << 16 | (!code) as u64;
                            let err = (v ^ want).count_ones();
                            if err <= max_err {
                                let raw = (0..32).fold(0u32, |a, k| a << 1 | ((at(&d, t + (17 + k) as f64 * sps) > 0.0) != inv) as u32);
                                let fiw = pocsag::correct(raw)
                                    .map(|(cw, _)| data21(cw))
                                    .filter(|x| (0..6).map(|k| x >> (4 * k) & 0xF).sum::<u32>() & 0xF == 0xF)
                                    .map(|x| ((x >> 4 & 0xF) as u8, (x >> 8 & 0x7F) as u8));
                                hits.push((t, err, si, inv, fiw));
                            }
                        }
                    }
                }
                t += sps;
            }
        }
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Group hits within 4 bits; keep the best.
        let mut groups: Vec<(f64, u32, usize, bool, Option<(u8, u8)>, usize)> = Vec::new();
        for h in hits {
            match groups.last_mut() {
                Some(g) if h.0 - g.0 < sps * 4.0 => {
                    g.5 += 1;
                    if h.1 < g.1 || (h.1 == g.1 && h.4.is_some() && g.4.is_none()) {
                        *g = (h.0, h.1, h.2, h.3, h.4, g.5);
                    }
                }
                _ => groups.push((h.0, h.1, h.2, h.3, h.4, 1)),
            }
        }
        let name = std::path::Path::new(stem).file_name().unwrap().to_string_lossy().to_string();
        let (mut extra_fiw, mut extra_nofiw) = (0, 0);
        println!("{name}: decoder found {} frames; sync-1 groups within {max_err} bit errors: {}", decoded.len(), groups.len());
        for g in &groups {
            let t = g.0 / fs;
            let known = decoded.iter().any(|f| (f.at as f64 / fs - t).abs() < 0.05);
            if !known {
                if g.4.is_some() {
                    extra_fiw += 1;
                } else {
                    extra_nofiw += 1;
                }
                println!("   t {t:7.2} {} err {} inv {} fiw {:?} (x{})", SPEEDS[g.2].1, g.1, g.3 as u8, g.4, g.5);
            }
        }
        println!("   not decoded: {extra_fiw} with a valid FIW, {extra_nofiw} without");
    }
}
