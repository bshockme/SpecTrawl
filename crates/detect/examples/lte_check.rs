//! Runs the LTE identifier on a cf32 SigMF channel capture (ideally the
//! central ~1.4 MHz of a downlink carrier at ≥ 1.92 MS/s), in slices.
//!
//!     cargo run --release -p detect --example lte_check -- <path without extension> [--slice-s S]

use detect::lte;
use num_complex::Complex32;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stem = a.get(1).expect("path stem");
    let slice_s: f64 = a.iter().position(|s| s == "--slice-s").and_then(|i| a.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(0.1);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    assert_eq!(meta["global"]["core:datatype"], "cf32_le", "channel captures only");
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    println!("{stem}: {:.2} s at {:.0} kS/s", x.len() as f64 / fs, fs / 1e3);
    let n = (slice_s * fs) as usize;
    let mut k = 0;
    let mut hits = 0;
    while (k + 1) * n <= x.len() && k < 40 {
        let s = &x[k * n..(k + 1) * n];
        let num = lte::cp_autocorrelation(s, fs);
        let cell = lte::identify(s, fs);
        match (&num, &cell) {
            (Some(nm), Some(c)) => {
                hits += c.sync.is_some() as usize;
                println!(
                    "  slice {k:2}: numerology {} kHz rho {:.2} cfo {:+.0} Hz · {}{}",
                    nm.scs_khz,
                    nm.rho,
                    nm.cfo_hz,
                    c.label(),
                    c.sync.as_ref().map(|s| format!(" (corr {:.2}, sss {:.2}, cfo {:+.0} Hz)", s.corr_peak, s.sss_peak, s.cfo_hz)).unwrap_or_default()
                );
            }
            (Some(nm), None) => println!("  slice {k:2}: numerology {} kHz rho {:.2}, no cell", nm.scs_khz, nm.rho),
            (None, _) => println!("  slice {k:2}: no OFDM numerology"),
        }
        k += 1;
    }
    println!("{hits} of {k} slices synchronised");
}
