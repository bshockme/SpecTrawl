//! Weak channels: what `classify` names in cf32 SigMF slices, then the sync search and its frames' decodes on the
//! sample discriminator against the symbol discriminator, exact and with soft sync matching at two thresholds.
//!
//!     cargo run --release -p detect --example weak_sync -- <stem>[@<record width Hz>]... | noise:<secs>:<seed>
//!
//! `BRIEF=1` prints only the classification; `SECS=<s>` keeps each slice's first seconds (the pool's first look is
//! 1.5 s); `BW=<Hz>` is the width to classify at when a stem doesn't give one (default 12.5 kHz).

use detect::{decode, frames, sync};
use num_complex::Complex32;

fn read(stem: &str) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]])))
        .collect();
    (x, fs)
}

/// The symbol discriminator less its median.
fn symbol_disc(y: &[Complex32], fs: f64, baud: f64) -> Vec<f32> {
    let mut out = detect::features::symbol_discriminator(y, fs, baud);
    let mut s = out.clone();
    let mid = s.len() / 2;
    let med = *s.select_nth_unstable_by(mid, f32::total_cmp).1;
    out.iter_mut().for_each(|v| *v -= med);
    out
}

fn report(name: &str, d: &[f32], fs: f64, opts: frames::SliceOpts, search: sync::SearchOpts) {
    let hits = sync::search_with(d, fs, &sync::table(), search);
    let best = sync::best(&hits);
    let Some(b) = best else {
        println!("  {name:<6} hits {:5}  -", hits.len());
        return;
    };
    let mine: Vec<sync::SyncHit> = hits.iter().filter(|h| h.entry.protocol == b.protocol && h.entry.baud == b.baud).cloned().collect();
    let (before, after) = match b.protocol {
        "DMR" => (54, 80),
        "P25" if b.detail == "Phase 2" => (560, 380),
        "P25" => (8, 880),
        _ => (8, 64),
    };
    let fr = frames::slice_with(d, fs, &mine, before, after, opts);
    let f = decode::summarise(&fr);
    let tsbk: usize = fr.iter().map(|f| detect::p25::tsbks(f, detect::p25::CODING).iter().filter(|t| t.crc_ok).count()).sum();
    println!(
        "  {name:<6} hits {:5}  {:<34} frames {:3}  decoded {:3}/{:<3} id {:<10} TSBKs ok {tsbk}",
        hits.len(),
        format!("{} {} ×{}", b.protocol, b.detail, b.count),
        fr.len(),
        f.decoded,
        f.tried,
        f.id.unwrap_or_default(),
    );
}

fn main() {
    for stem in std::env::args().skip(1) {
        let (x, fs) = if let Some(spec) = stem.strip_prefix("noise:") {
            // noise:<seconds>:<seed>, complex Gaussian at 78.125 kS/s
            let mut it = spec.split(':');
            let secs: f64 = it.next().unwrap().parse().unwrap();
            let mut seed: u64 = it.next().unwrap_or("1").parse().unwrap();
            let mut u = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 11) as f64 + 1.0) / (1u64 << 53) as f64
            };
            let fs = 78_125.0;
            let x = (0..(secs * fs) as usize)
                .map(|_| {
                    let (a, b) = (u(), u());
                    let r = (-2.0 * a.ln()).sqrt();
                    Complex32::new((r * (std::f64::consts::TAU * b).cos()) as f32, (r * (std::f64::consts::TAU * b).sin()) as f32)
                })
                .collect();
            (x, fs)
        } else {
            read(stem.split('@').next().unwrap())
        };
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        // BW=<Hz>: classify as a record that wide (the pool passes the record's own width); BRIEF=1: only that.
        // stem@bw: that record's width; SECS=<s>: only its first seconds (the pool's first look is 1.5 s).
        let (stem_bw, x) = match name.split_once('@') {
            Some((_, b)) => (b.parse::<f64>().ok(), x),
            None => (None, x),
        };
        let x = match std::env::var("SECS").ok().and_then(|v| v.parse::<f64>().ok()) {
            Some(s) => x[..x.len().min((s * fs) as usize)].to_vec(),
            None => x,
        };
        let bw: f64 = stem_bw.or(std::env::var("BW").ok().and_then(|v| v.parse().ok())).unwrap_or(12_500.0);
        let t0 = std::time::Instant::now();
        let c = detect::classify(&x, fs, bw);
        let f = c.fields.as_ref();
        println!(
            "{name}: {:.1} s · classify: {} · {} · decoded {}/{} · {:.0} ms",
            x.len() as f64 / fs,
            c.label,
            f.and_then(|f| f.id.clone()).unwrap_or_default(),
            f.map_or(0, |f| f.decoded),
            f.map_or(0, |f| f.tried),
            t0.elapsed().as_secs_f64() * 1e3
        );
        if std::env::var("BRIEF").is_ok() {
            continue;
        }
        let y = detect::prepare_cutoff(&x, fs, detect::NARROW_CUTOFF_HZ);
        let gate = detect::features::envelope_gate(&y);
        let mut disc = detect::features::discriminator(&y, fs);
        let mut s = disc.clone();
        let mid = s.len() / 2;
        let med = *s.select_nth_unstable_by(mid, f32::total_cmp).1;
        for (v, g) in disc.iter_mut().zip(&gate[1..]) {
            *v = if *g { *v - med } else { 0.0 };
        }
        let plain = sync::SearchOpts::default();
        report("disc", &disc, fs, frames::SliceOpts::default(), plain);
        let sd = symbol_disc(&y, fs, 4800.0);
        let raw = frames::SliceOpts { smooth: 0.0, post_lpf: None, ..frames::SliceOpts::default() };
        report("sym", &sd, fs, raw, sync::SearchOpts { smooth: 0.0, soft: None });
        for rho in [0.8f32, 0.75] {
            report(&format!("s{rho}"), &sd, fs, raw, sync::SearchOpts { smooth: 0.0, soft: Some(rho) });
        }
    }
}
