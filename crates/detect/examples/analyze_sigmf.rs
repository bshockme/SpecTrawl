//! Decodes protocol fields from a cf32 SigMF channel capture.
//!
//!     cargo run --release -p detect --example analyze_sigmf -- <path without extension>

use std::collections::BTreeMap;

use detect::decode;
use num_complex::Complex32;

fn main() {
    let stem = std::env::args().nth(1).expect("path stem");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    assert_eq!(meta["global"]["core:datatype"], "cf32_le", "channel captures only");
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
    let c = detect::classify(&x, fs, 12_500.0);
    println!("{name}: {:.1} s · {} ({:.0}%)", x.len() as f64 / fs, c.label, c.confidence * 100.0);
    if let Some(f) = &c.fields {
        let kinds: Vec<String> = f.kinds.iter().map(|(k, n)| format!("{k}×{n}")).collect();
        println!("  classify fields: {} · {} · decoded {}/{}", f.id.as_deref().unwrap_or("-"), kinds.join(", "), f.decoded, f.tried);
    }
    // Whole-capture frame decode with error histograms.
    let frames = detect::frames(&x, fs, 12_500.0, 8, 64);
    let mut e: BTreeMap<String, BTreeMap<u32, usize>> = BTreeMap::new();
    for fr in &frames {
        if let Some(n) = decode::p25_nid(fr) {
            *e.entry("P25 NID".into()).or_default().entry(n.errors).or_default() += 1;
        }
        if let Some(s) = decode::dmr_slot_type(fr) {
            *e.entry("DMR slot type".into()).or_default().entry(s.errors).or_default() += 1;
        }
    }
    let all = decode::summarise(&frames);
    let kinds: Vec<String> = all.kinds.iter().map(|(k, n)| format!("{k}×{n}")).collect();
    println!("  whole capture: {} frames · {} · {} · decoded {}/{}", frames.len(), all.id.as_deref().unwrap_or("-"), kinds.join(", "), all.decoded, all.tried);
    for (k, h) in e {
        println!("    {k} corrected-bit histogram: {h:?}");
    }
}
