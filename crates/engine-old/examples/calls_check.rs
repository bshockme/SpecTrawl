//! Runs a source live and prints the call log: grants from control-channel
//! monitors merged with what voice channels' signalling said.
//!
//!     cargo run --release -p engine-old --example calls_check -- <serial> <MHz> <seconds>

use std::time::Duration;

use engine_old::{Mode, Source, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, mhz, secs) = (&a[1], a[2].parse::<f64>().unwrap(), a[3].parse::<u64>().unwrap());
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    d.set_center(mhz * 1e6).unwrap();
    let src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
    for t in (30..=secs).step_by(30) {
        std::thread::sleep(Duration::from_secs(30));
        let calls = src.calls(10_000);
        let mons: Vec<String> = src.monitors().iter().map(|m| format!("{} {:.4}", m.label, m.center_hz / 1e6)).collect();
        println!(
            "{t:>4} s  monitors {mons:?}  calls {} (granted {}, heard {}, both {})",
            calls.len(),
            calls.iter().filter(|c| c.granted).count(),
            calls.iter().filter(|c| c.heard).count(),
            calls.iter().filter(|c| c.granted && c.heard).count()
        );
    }
    for c in src.calls(10_000).iter().rev() {
        println!(
            "  {:>4.0} s  {:>10}  {:<16} {:<28} {}{}",
            c.last.duration_since(c.first).unwrap_or_default().as_secs_f64(),
            c.center_hz.map(|f| format!("{:.5}", f / 1e6)).unwrap_or("?".into()),
            c.protocol,
            c.call.summary(),
            if c.granted { "granted " } else { "" },
            if c.heard { "heard" } else { "" }
        );
    }
}
