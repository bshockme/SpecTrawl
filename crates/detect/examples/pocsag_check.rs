//! POCSAG on a recorded channel, the way the pager monitor runs it: sync
//! frames from the bank, then pages. Text only with `--show`.
//!
//!     cargo run --release -p detect --example pocsag_check -- [--show] <stem>...

use detect::pocsag;
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
    for stem in std::env::args().skip(1).filter(|a| a != "--show" && a != "--batches") {
        let (x, fs) = load(&stem);
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        // The monitor's 4 s windows, 2.2 s kept.
        let mut pages = Vec::new();
        let mut frames = 0;
        let (block, keep) = ((1.8 * fs) as usize, (2.2 * fs) as usize);
        let mut start = 0;
        while start + block + keep <= x.len() {
            let buf = &x[start..start + block + keep];
            let fr = detect::frames_with(buf, fs, 6_500.0, 0, 520, Default::default());
            frames += fr.len();
            if std::env::args().any(|a| a == "--batches") {
                for f in fr.iter().filter(|f| f.protocol == "POCSAG") {
                    if let Some(cws) = pocsag::batch_codewords(f) {
                        let mut h = [0usize; 4];
                        for &raw in &cws {
                            match pocsag::correct(raw) {
                                Some((_, k)) => h[(k as usize).min(2)] += 1,
                                None => h[3] += 1,
                            }
                        }
                        println!(
                            "   batch at {:7.2} s: codewords fixed0 {:2} fixed1 {:2} fixed2 {:2} bad {:2}",
                            (start + f.at_symbol * 0) as f64 / fs,
                            h[0],
                            h[1],
                            h[2],
                            h[3]
                        );
                    }
                }
            }
            pages.extend(pocsag::pages(&fr));
            start += block;
        }
        println!("{name}: {:.0} s, {frames} sync frames, {} pages", x.len() as f64 / fs, pages.len());
        for p in &pages {
            let body = if show { p.text.clone() } else { format!("{} chars", p.text.chars().count()) };
            println!("   {:07} func {} corrected {:2} {body}", p.address, (b'A' + p.function) as char, p.corrected);
        }
    }
}
