//! Headless end-to-end check: run a source through the engine and print what
//! each emitter was classified as.
//!
//!     cargo run --release -p engine --example classify_sim -- [seconds] [airspy-serial freq-MHz]

use std::time::Duration;

use engine::{Mode, Source, SourceConfig};
use sdr_core::Backend;

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    let args: Vec<String> = std::env::args().collect();
    let dev = match (args.get(2), args.get(3).and_then(|f| f.parse::<f64>().ok())) {
        (Some(serial), Some(mhz)) => {
            let b = backend_airspy::AirspyBackend::new();
            let mut d = b.open(serial).expect("open airspy");
            d.set_center(mhz * 1e6).unwrap();
            d
        }
        _ => {
            let b = backend_sim::SimBackend;
            b.open(&b.enumerate().unwrap()[0].serial).unwrap()
        }
    };
    let src = Source::start(dev, SourceConfig::default(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_secs(secs));

    println!("{:>12} {:>7} {:>6} {:>6} {:>6}  {}", "MHz", "BW kHz", "recs", "duty%", "act", "label  [tally]");
    for e in src.emitters() {
        let tally: Vec<String> = e.tally.iter().map(|(k, v)| format!("{k}×{v}")).collect();
        println!(
            "{:>12.4} {:>7.1} {:>6} {:>6.0} {:>6}  {}  [{}]",
            e.center_hz / 1e6,
            e.bandwidth_hz / 1e3,
            e.records,
            e.duty() * 100.0,
            if e.active { "yes" } else { "" },
            e.label().unwrap_or_else(|| "-".into()),
            tally.join(", ")
        );
    }
}
