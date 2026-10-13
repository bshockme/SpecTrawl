//! Per-window features and label of a cf32 SigMF channel capture, as the
//! live classifier sees it: for diagnosing a label that flips between
//! windows.
//!
//!     cargo run --release -p detect --example window_features -- <path without extension> [window s] [bandwidth kHz] [--lines]
//!
//! `--lines` adds, per window, the strongest lines of the discriminator's
//! spectrum with their share of its power, and a frequency histogram.

use num_complex::Complex32;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stem = &a[1];
    let win: f64 = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let bw: f64 = a.get(3).and_then(|v| v.parse().ok()).unwrap_or(12.5) * 1e3;
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let lines = a.iter().any(|v| v == "--lines");
    let n = (win * fs) as usize;
    println!("{:>6}  {:<26} {:>5} {:>6} {:>7} {:>6} {:>6} {:>7}", "t s", "label", "lv", "cv", "dev Hz", "rate", "tone", "syncs");
    for (k, w) in x.chunks(n).enumerate() {
        if w.len() < n / 2 {
            break;
        }
        let c = detect::classify(w, fs, bw);
        let f = &c.features;
        println!(
            "{:>6.1}  {:<26} {:>5} {:>6.2} {:>7.0} {:>6} {:>6} {:>7}",
            k as f64 * win,
            match c.fields.as_ref().and_then(|f| f.id.as_deref()) {
                Some(id) => format!("{} · {id}", c.label),
                None => c.label.clone(),
            },
            f.fsk_levels,
            f.envelope_cv,
            f.deviation_hz,
            f.symbol_rate_raw.map(|r| format!("{r:.0}")).unwrap_or("-".into()),
            f.tone_hz.map(|t| format!("{t:.0}")).unwrap_or("-".into()),
            c.sync.as_ref().map(|s| s.count).unwrap_or(0)
        );
        if lines {
            disc_lines(w, fs);
        }
    }
}

fn disc_lines(w: &[Complex32], fs: f64) {
    let d = detect::features::discriminator(w, fs);
    let m = 1usize << d.len().min(1 << 16).ilog2();
    let mean = d[..m].iter().sum::<f32>() / m as f32;
    let mut buf: Vec<Complex32> = d[..m]
        .iter()
        .enumerate()
        .map(|(i, v)| Complex32::new((v - mean) * (0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / m as f32).cos()), 0.0))
        .collect();
    rustfft::FftPlanner::new().plan_fft_forward(m).process(&mut buf);
    let bin = fs / m as f64;
    let pw: Vec<f32> = buf[..m / 2].iter().map(|c| c.norm_sqr()).collect();
    let lo = (50.0 / bin) as usize;
    let tot: f32 = pw[lo..].iter().sum();
    let mut order: Vec<usize> = (lo..pw.len()).collect();
    order.sort_by(|a, b| pw[*b].total_cmp(&pw[*a]));
    let mut shown: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    for i in order {
        if shown.iter().any(|s| s.abs_diff(i) as f64 * bin < 60.0) {
            continue;
        }
        shown.push(i);
        let share: f32 = pw[i.saturating_sub(2)..(i + 3).min(pw.len())].iter().sum::<f32>() / tot;
        out.push(format!("{:.0} Hz {:.0}%", i as f64 * bin, share * 100.0));
        if shown.len() >= 6 {
            break;
        }
    }
    let band = |a: f64, b: f64| pw[(a / bin) as usize..((b / bin) as usize).min(pw.len())].iter().sum::<f32>() / tot * 100.0;
    println!("        lines: {} | 50-300 Hz {:.0}%, 300-3000 {:.0}%, >3000 {:.0}%", out.join(", "), band(50.0, 300.0), band(300.0, 3000.0), band(3000.0, fs / 2.0));
    let mut h = [0u32; 20];
    for v in &d {
        let t = ((v + 2500.0) / 5000.0 * 20.0) as i64;
        if (0..20).contains(&t) {
            h[t as usize] += 1;
        }
    }
    println!("        hist -2.5..2.5 kHz: {}", h.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "));
}
