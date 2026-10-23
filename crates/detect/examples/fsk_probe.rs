//! What kind of FSK is on a channel capture: the discriminator's level
//! histogram (2 or 4 levels, deviation) and the intervals between sign
//! changes (the symbol rate), over the first `--secs` seconds.
//!
//!     cargo run --release -p detect --example fsk_probe -- [--secs S] <stem>...

use detect::features;
use num_complex::Complex32;

fn load(stem: &str, secs: f64) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = raw
        .chunks_exact(8)
        .take((secs * fs) as usize)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    (x, fs)
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let secs = a.iter().position(|x| x == "--secs").and_then(|i| a.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(20.0);
    for stem in a.iter().filter(|x| !x.starts_with("--") && x.parse::<f64>().is_err()) {
        let (x, fs) = load(stem, secs);
        let d = features::discriminator(&x, fs);
        let mut s = d.clone();
        s.sort_by(f32::total_cmp);
        let mid = s[s.len() / 2];
        let p = |q: f64| s[((s.len() - 1) as f64 * q) as usize];
        // Level histogram, 500 Hz bins over ±12 kHz about the median.
        let mut hist = vec![0usize; 48];
        for &v in &d {
            let b = ((v - mid) / 500.0 + 24.0).floor();
            if (0.0..48.0).contains(&b) {
                hist[b as usize] += 1;
            }
        }
        let peak = *hist.iter().max().unwrap() as f64;
        let bar: String = hist.iter().map(|&h| b" .:-=+*#@"[((h as f64 / peak) * 8.0).round() as usize] as char).collect();
        // Sign-change intervals in microseconds, 25 µs bins up to 2 ms.
        let mut iv = vec![0usize; 80];
        let (mut last, mut sign) = (0usize, d[0] > mid);
        for (i, &v) in d.iter().enumerate() {
            let sg = v > mid;
            if sg != sign {
                sign = sg;
                let us = (i - last) as f64 / fs * 1e6;
                let b = (us / 25.0) as usize;
                if b < 80 {
                    iv[b] += 1;
                }
                last = i;
            }
        }
        let first = iv
            .iter()
            .enumerate()
            .skip(2)
            .find(|&(_, &n)| n as f64 > iv.iter().map(|&n| n as f64).fold(0.0, f64::max) * 0.3)
            .map(|(b, _)| b as f64 * 25.0 + 12.5);
        let mean_power = x.iter().map(|c| c.norm_sqr()).sum::<f32>() / x.len() as f32;
        println!(
            "{}: {:.0} s, {:.1} dB, deviation p5/p95 {:+.0}/{:+.0} Hz about {:+.0} Hz",
            std::path::Path::new(stem).file_name().unwrap().to_string_lossy(),
            x.len() as f64 / fs,
            10.0 * mean_power.log10(),
            p(0.05) - mid,
            p(0.95) - mid,
            mid
        );
        println!("   levels  -12k |{bar}| +12k");
        println!(
            "   shortest common sign-change interval ≈ {} → ≈ {} baud; intervals ≤2 ms (25 µs bins): {}",
            first.map_or("-".into(), |f| format!("{f:.0} µs")),
            first.map_or("-".into(), |f| format!("{:.0}", 1e6 / f)),
            iv.iter().map(|&n| (n.min(9)).to_string()).collect::<String>()
        );
    }
}
