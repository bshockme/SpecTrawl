//! Who was talking on recorded channels: classifies 1.5 s windows (as the
//! live classifier does) and counts the talk strings and link control
//! kinds found.
//!
//!     cargo run --release -p detect --example talk_check -- <path without extension>...

use std::collections::BTreeMap;

use num_complex::Complex32;

fn main() {
    for stem in std::env::args().skip(1) {
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
        let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
        let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
        let x: Vec<Complex32> = raw
            .chunks_exact(8)
            .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
            .collect();
        let mut talk: BTreeMap<String, usize> = BTreeMap::new();
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        let mut windows = 0;
        for w in x.chunks((1.5 * fs) as usize) {
            let Some(f) = detect::classify(w, fs, 12_500.0).fields else { continue };
            windows += 1;
            if let Some(t) = f.talk {
                *talk.entry(t).or_default() += 1;
            }
            for (k, n) in f.kinds.into_iter().filter(|k| k.0.starts_with("LC")) {
                *kinds.entry(k).or_default() += n;
            }
        }
        // Control channel: talkgroup grants, with the frequency granted.
        let mut sys = detect::p25::SystemInfo::default();
        let mut grants: BTreeMap<(u16, String), usize> = BTreeMap::new();
        for m in detect::p25::messages(&detect::frames(&x, fs, 12_500.0, 8, 380)) {
            for (tg, ch, src) in sys.apply(&m) {
                let f = sys.freq(ch).map_or("?".into(), |f| format!("{:.4}", f / 1e6));
                *grants.entry((tg, format!("{f} src {}", src.map_or("-".into(), |s| s.to_string())))).or_default() += 1;
            }
        }
        for ((tg, rest), n) in &grants {
            println!("  grant x{n:<3} TG {tg} -> {rest}");
        }
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        println!("{name}: {windows} windows with fields; link control {kinds:?}");
        for (t, n) in &talk {
            println!("    {n:>3} windows: {t}");
        }
    }
}
