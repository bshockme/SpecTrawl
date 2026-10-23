//! Prints the known wideband occupants a source identifies.
//!
//!     cargo run --release -p engine-old --example occupancy_check -- <serial> <MHz | loMHz-hiMHz> <seconds> [--hint loMHz-hiMHz]
//!
//! `--hint` tells a live source about a cellular block it can't see the
//! edges of (as the app's survey would), so the cell monitor runs on it.

use std::time::Duration;

use engine_old::{Mode, Source, SourceConfig, SweepPlan};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, secs) = (&a[1], a[3].parse::<u64>().unwrap());
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    let mode = match a[2].split_once('-') {
        Some((lo, hi)) => Mode::Sweep(SweepPlan {
            start_hz: lo.parse::<f64>().unwrap() * 1e6,
            stop_hz: hi.parse::<f64>().unwrap() * 1e6,
            ..Default::default()
        }),
        None => {
            d.set_center(a[2].parse::<f64>().unwrap() * 1e6).unwrap();
            Mode::Live
        }
    };
    let src = Source::start(d, SourceConfig::default(), mode).unwrap();
    if let Some(h) = a.iter().position(|s| s == "--hint").and_then(|i| a.get(i + 1)) {
        if let Some((lo, hi)) = h.split_once('-') {
            let (lo, hi) = (lo.parse::<f64>().unwrap() * 1e6, hi.parse::<f64>().unwrap() * 1e6);
            src.set_hints(vec![detect::spectral::Occupant {
                kind: detect::spectral::Kind::Cellular { bw_mhz: ((hi - lo) / 1e6) as f32 },
                center_hz: (lo + hi) / 2.0,
                lo_hz: lo,
                hi_hz: hi,
                snr_db: 20.0,
            }]);
        }
    }
    std::thread::sleep(Duration::from_secs(secs));
    let occ = src.occupants();
    println!("{} occupants", occ.len());
    for k in &occ {
        let o = &k.occupant;
        println!("  {:>10.4} MHz  {:>9.3}-{:<9.3}  {:>5.1} dB  {}", o.center_hz / 1e6, o.lo_hz / 1e6, o.hi_hz / 1e6, o.snr_db, k.label());
    }
}
