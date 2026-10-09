//! Runs the spectral identifier on spectra saved by the engine's `trawl`
//! example (`<stem>.spec` + `<stem>.json`).
//!
//!     cargo run --release -p detect --example identify_spec -- <stem>... [--max]

use detect::spectral::{Spectrum, floor_estimate, identify};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let use_max = args.iter().any(|a| a == "--max");
    for stem in args.iter().filter(|a| !a.starts_with("--")) {
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.json")).unwrap()).unwrap();
        let n = meta["bins"].as_u64().unwrap() as usize;
        let span = meta["span_hz"].as_f64().unwrap();
        let bin = span / n as f64;
        let f0 = meta["center_hz"].as_f64().unwrap() - span / 2.0;
        let raw = std::fs::read(format!("{stem}.spec")).unwrap();
        let v: Vec<f32> = raw.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        let db = if use_max { &v[n..2 * n] } else { &v[..n] };
        let window = ((span / 20.0).max(1e6) / bin) as usize;
        let floor = floor_estimate(&v[..n], window);
        let s = Spectrum { db, floor: &floor, f0_hz: f0, bin_hz: bin };
        let occ = identify(&s);
        println!("{stem}: {} occupants ({:.0} Hz bins)", occ.len(), bin);
        for o in &occ {
            println!(
                "  {:>10.4} MHz  {:>8.3}-{:<8.3}  {:>5.1} dB  {}",
                o.center_hz / 1e6,
                o.lo_hz / 1e6,
                o.hi_hz / 1e6,
                o.snr_db,
                o.kind.label()
            );
        }
    }
}
