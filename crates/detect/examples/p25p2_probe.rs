//! P25 Phase 2 on real recordings: sync pairing, ISCH, burst types,
//! Reed-Solomon success per burst kind, and the calls named.
//!
//!     cargo run --release -p detect --example p25p2_probe -- <wacn hex> <system hex> <nac hex> <path without extension>...

use std::collections::BTreeMap;

use detect::p25p2::{self, ScrambleKey};
use num_complex::Complex32;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let key = ScrambleKey { wacn: hex(&a[1]), system: hex(&a[2]) as u16, nac: hex(&a[3]) as u16 };
    let seq = p25p2::scrambling(key);
    for stem in &a[4..] {
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
        let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
        let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
        let x: Vec<Complex32> = raw
            .chunks_exact(8)
            .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
            .collect();
        let mut syncs = 0;
        let mut frags = 0;
        let mut fragment_ids: BTreeMap<String, usize> = BTreeMap::new();
        let mut bursts: BTreeMap<String, usize> = BTreeMap::new();
        let mut decoded: BTreeMap<String, usize> = BTreeMap::new();
        let mut talk: BTreeMap<String, usize> = BTreeMap::new();
        let mut other: BTreeMap<String, usize> = BTreeMap::new();
        // Pieces of 10 s keep memory down.
        for w in x.chunks((10.0 * fs) as usize) {
            let frames = detect::frames(w, fs, 12_500.0, 560, 380);
            for f in frames.iter().filter(|f| f.protocol == "P25" && f.detail == "Phase 2") {
                syncs += 1;
                let Some(info) = p25p2::fragment(f, &[&seq]) else { continue };
                frags += 1;
                *fragment_ids.entry(format!("{:?}", info.fragment)).or_default() += 1;
                for b in &info.bursts {
                    *bursts.entry(format!("{b:?}")).or_default() += 1;
                }
                for s in &info.signals {
                    *decoded.entry(format!("{:?}", s.burst)).or_default() += 1;
                    match s.mac.talk() {
                        Some(t) => *talk.entry(format!("ch{} {t}", s.channel)).or_default() += 1,
                        None => *other.entry(format!("{:?}", s.mac)).or_default() += 1,
                    }
                }
            }
        }
        let name = std::path::Path::new(stem).file_name().unwrap().to_string_lossy().to_string();
        println!("{name}: {syncs} syncs, {frags} fragments; fragment ids {fragment_ids:?}");
        println!("  bursts  {bursts:?}");
        println!("  RS ok   {decoded:?}");
        for (t, n) in &talk {
            println!("    {n:>4} x {t}");
        }
        let mut o: Vec<_> = other.into_iter().collect();
        o.sort_by(|a, b| b.1.cmp(&a.1));
        for (t, n) in o.iter().take(8) {
            println!("    {n:>4} x {t}");
        }
    }
}
