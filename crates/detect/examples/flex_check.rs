//! FLEX decoding on recorded channels: per-frame block quality and page
//! counts. Message text is only printed with `--show` (pager traffic is
//! often personal or medical).
//!
//!     cargo run --release -p detect --example flex_check -- [--show] <path without extension>...

use detect::flex;
use num_complex::Complex32;

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

fn main() {
    let show = std::env::args().any(|a| a == "--show");
    for stem in std::env::args().skip(1).filter(|a| a != "--show") {
        let (x, fs) = load(&stem);
        let t = std::time::Instant::now();
        let frames = flex::decode(&x, fs);
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        let words: usize = frames.iter().flat_map(|f| f.block_ok).map(|n| n as usize).sum();
        let pages: Vec<&flex::Page> = frames.iter().flat_map(|f| &f.pages).collect();
        println!(
            "{name}: {} frames, words {words}/{} ok, {} pages ({} damaged), {:.2} s to decode {:.0} s",
            frames.len(),
            frames.len() * 11 * 32,
            pages.len(),
            pages.iter().filter(|p| p.damaged).count(),
            t.elapsed().as_secs_f64(),
            x.len() as f64 / fs
        );
        // What the live classifier (1.5 s windows) would call it.
        let mut labels: std::collections::BTreeMap<String, usize> = Default::default();
        for w in x.chunks((1.5 * fs) as usize) {
            *labels.entry(detect::classify(w, fs, 16e3).label).or_default() += 1;
        }
        println!("  1.5 s windows: {labels:?}");
        for f in &frames {
            println!("  t {:7.2} {} cycle {:2} frame {:3} blocks {:?}", f.at as f64 / fs, f.mode(), f.cycle, f.frame, f.block_ok);
            for p in &f.pages {
                let body = if show { p.text.clone() } else { format!("{} chars", p.text.chars().count()) };
                println!(
                    "      {:09} {} {:11} {}{}{}",
                    p.capcode,
                    p.phase,
                    p.kind,
                    if p.damaged { "[damaged] " } else { "" },
                    if p.continued { "[continued] " } else { "" },
                    body
                );
            }
        }
    }
}
