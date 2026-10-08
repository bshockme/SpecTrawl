//! Runs an Airspy on a band and prints what the P25 control-channel monitors
//! learn: systems, sites, band plans and talkgroup grants.
//!
//!     cargo run --release -p engine --example p25_watch -- <serial> <MHz> <seconds>

use std::time::{Duration, Instant};

use engine::{Mode, Source, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, mhz, secs) = (&a[1], a[2].parse::<f64>().unwrap(), a[3].parse::<u64>().unwrap());
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    d.set_center(mhz * 1e6).unwrap();
    let src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
    let t0 = Instant::now();
    let mut shown = 0;
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(5));
        let mons = src.monitors();
        let grants = src.p25_grants(10_000);
        println!(
            "[{:>3}s] monitors: {:?} · grants so far: {}",
            t0.elapsed().as_secs(),
            mons.iter().map(|m| format!("{:.4} MHz ({} msgs)", m.center_hz / 1e6, m.messages)).collect::<Vec<_>>(),
            grants.len()
        );
        for g in grants.iter().rev().skip(shown) {
            println!(
                "    NAC {:#05x} TG {:>6} → {} {} src {} (×{})",
                g.nac,
                g.talkgroup,
                g.freq_hz.map(|f| format!("{:.5} MHz", f / 1e6)).unwrap_or("?".into()),
                g.slot.map(|s| format!("slot {s}")).unwrap_or_default(),
                g.source.map(|s| s.to_string()).unwrap_or("-".into()),
                g.count
            );
        }
        shown = grants.len();
    }
    for (nac, s) in src.p25_systems() {
        println!(
            "SYSTEM NAC {nac:#05x}: WACN {:?} sys {:?} rfss {:?} site {:?} control {:?} MHz · {} band plans · {} adjacent sites",
            s.wacn.map(|w| format!("{w:#07x}")),
            s.system.map(|v| format!("{v:#05x}")),
            s.rfss,
            s.site,
            s.control_hz.map(|f| f / 1e6),
            s.plans.len(),
            s.adjacent.len()
        );
    }
}
