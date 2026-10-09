//! Prints the known wideband occupants a source identifies.
//!
//!     cargo run --release -p engine --example occupancy_check -- <serial> <MHz | loMHz-hiMHz> <seconds>

use std::time::Duration;

use engine::{Mode, Source, SourceConfig, SweepPlan};
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
    std::thread::sleep(Duration::from_secs(secs));
    let occ = src.occupants();
    println!("{} occupants", occ.len());
    for k in &occ {
        let o = &k.occupant;
        println!(
            "  {:>10.4} MHz  {:>9.3}-{:<9.3}  {:>5.1} dB  {}",
            o.center_hz / 1e6,
            o.lo_hz / 1e6,
            o.hi_hz / 1e6,
            o.snr_db,
            o.kind.label()
        );
    }
}
