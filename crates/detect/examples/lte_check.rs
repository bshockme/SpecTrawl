//! Runs the LTE identifier on a cf32 SigMF channel capture (ideally the
//! central ~1.4 MHz of a downlink carrier at ≥ 1.92 MS/s), in slices.
//!
//!     cargo run --release -p detect --example lte_check -- <path without extension> [--slice-s S] [--mib-debug] [--max N]
//!
//! `--mib-debug` prints the MIB stage's diagnostics (CRS SNR per port and
//! frame, Viterbi metric and CRC outcome per hypothesis) for each slice.

use detect::lte;
use num_complex::Complex32;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let stem = a.get(1).expect("path stem");
    let arg = |name: &str| a.iter().position(|s| s == name).and_then(|i| a.get(i + 1));
    let slice_s: f64 = arg("--slice-s").and_then(|v| v.parse().ok()).unwrap_or(0.1);
    let max: usize = arg("--max").and_then(|v| v.parse().ok()).unwrap_or(40);
    let debug = a.iter().any(|s| s == "--mib-debug");
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
    let (mut synced, mut mibs) = (0, 0);
    while (k + 1) * n <= x.len() && k < max {
        let s = &x[k * n..(k + 1) * n];
        let num = lte::cp_autocorrelation(s, fs);
        let mut log = Vec::new();
        let cell = if debug { lte::identify_debug(s, fs, &mut log) } else { lte::identify(s, fs) };
        match (&num, &cell) {
            (Some(nm), Some(c)) => {
                synced += c.sync.is_some() as usize;
                mibs += c.mib.is_some() as usize;
                println!(
                    "  slice {k:2} (t={:.1} s): numerology {} kHz rho {:.2} cfo {:+.0} Hz · {}{}{}",
                    k as f64 * slice_s,
                    nm.scs_khz,
                    nm.rho,
                    nm.cfo_hz,
                    c.label(),
                    c.sync
                        .as_ref()
                        .map(|s| format!(
                            " (corr {:.2}, sss {:.2}, cfo {:+.0} Hz, frame at {})",
                            s.corr_peak, s.sss_peak, s.cfo_hz, s.frame_start
                        ))
                        .unwrap_or_default(),
                    c.mib
                        .as_ref()
                        .map(|m| format!(
                            " · SFN {} · PHICH {}{}",
                            m.sfn,
                            ["1/6", "1/2", "1", "2"][m.phich_resource as usize & 3],
                            if m.phich_extended { " ext" } else { "" }
                        ))
                        .unwrap_or_default()
                );
            }
            (Some(nm), None) => println!("  slice {k:2}: numerology {} kHz rho {:.2}, no cell", nm.scs_khz, nm.rho),
            (None, _) => println!("  slice {k:2}: no OFDM numerology"),
        }
        for l in &log {
            println!("      {l}");
        }
        k += 1;
    }
    println!("{synced} of {k} slices synchronised, {mibs} MIBs decoded");
}
