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
    // cf32 slices, or the ci16 ones written before them.
    let x = if meta["global"]["core:datatype"] == "ci16_le" {
        let v = |b: &[u8]| i16::from_le_bytes([b[0], b[1]]) as f32 / 32767.0;
        raw.as_chunks::<4>().0.iter().map(|c| Complex32::new(v(&c[0..2]), v(&c[2..4]))).collect()
    } else {
        let v = |b: &[u8]| f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        raw.as_chunks::<8>().0.iter().map(|c| Complex32::new(v(&c[0..4]), v(&c[4..8]))).collect()
    };
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
        // A slice shorter than one window is read whole.
        while start == 0 || start + block + keep <= x.len() {
            let buf = &x[start..(start + block + keep).min(x.len())];
            let fr = detect::pocsag_frames(buf, fs);
            if let Some(h) = detect::sync_hits(buf, fs, 6_500.0).iter().find(|h| h.entry.protocol == "POCSAG") {
                println!("   POCSAG at {} Bd", h.entry.baud);
            }
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
                            "   batch at {:7.2} s + symbol {:5}: codewords fixed0 {:2} fixed1 {:2} fixed2 {:2} bad {:2}",
                            start as f64 / fs,
                            f.at_symbol,
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
            let flags = if p.damaged { " damaged" } else { "" };
            println!(
                "   {:07} func {} corrected {:2} repaired {}{flags} {body}",
                p.address,
                (b'A' + p.function) as char,
                p.corrected,
                p.repaired
            );
        }
    }
}
