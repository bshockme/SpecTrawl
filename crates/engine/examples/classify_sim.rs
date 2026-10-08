//! Headless end-to-end check: run a source through the engine and print what
//! each emitter was classified as.
//!
//!     cargo run --release -p engine --example classify_sim -- [seconds] [airspy-serial freq-MHz]

use std::collections::BTreeMap;
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

    // Group by 12.5 kHz channel.
    let mut by_ch: BTreeMap<i64, Vec<engine::SignalRecord>> = BTreeMap::new();
    for r in src.recent_records(5000) {
        by_ch.entry((r.center_hz() / 12_500.0).round() as i64).or_default().push(r);
    }
    println!("{:>12} {:>5} {:>8}  labels (count)", "MHz", "recs", "BW kHz");
    for (ch, recs) in by_ch {
        let mut labels: BTreeMap<String, usize> = BTreeMap::new();
        for r in &recs {
            let l = r.class.as_ref().map(|c| {
                let f = &c.features;
                let sync = c.sync.as_ref().map(|s| format!(" sync×{}{}", s.count, if s.inverted { "i" } else { "" })).unwrap_or_default();
                format!("{}{} [lv{} {}Bd dev{:.0}]", c.label, sync, f.fsk_levels, f.symbol_rate_raw.map(|v| format!("{v:.0}")).unwrap_or("-".into()), f.deviation_hz)
            });
            *labels.entry(l.unwrap_or_else(|| "(unclassified)".into())).or_default() += 1;
        }
        let bw = recs.iter().map(|r| r.bandwidth_hz()).sum::<f64>() / recs.len() as f64 / 1e3;
        let l: Vec<String> = labels.into_iter().map(|(k, v)| format!("{k} ({v})")).collect();
        println!("{:>12.4} {:>5} {:>8.1}  {}", ch as f64 * 12_500.0 / 1e6, recs.len(), bw, l.join(", "));
    }
}
