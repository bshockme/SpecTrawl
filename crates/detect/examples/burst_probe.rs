//! Per-burst look at a cf32 SigMF channel capture: when each burst is on,
//! its frequency range, and how much of its instantaneous frequency follows
//! straight ramps (chirps, as LoRa sends) rather than holding levels (FSK).
//!
//!     cargo run --release -p detect --example burst_probe -- <path without extension>...

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
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        println!("{name}: {:.1} s at {:.0} kHz", x.len() as f64 / fs, fs / 1e3);
        for b in detect::chirp::bursts(&x, fs).iter().take(12) {
            let c = detect::chirp::analyse(&x[b.0..b.1], fs);
            println!(
                "  {:>8.3} s  {:>7.1} ms  f {:>+7.1}..{:<+7.1} kHz  ramp {:.2}  slope {:>8.2} MHz/s  levels {:.2}",
                b.0 as f64 / fs,
                (b.1 - b.0) as f64 / fs * 1e3,
                c.f_lo_hz / 1e3,
                c.f_hi_hz / 1e3,
                c.ramp_fraction,
                c.slope_hz_per_s / 1e6,
                c.level_fraction
            );
        }
    }
}
