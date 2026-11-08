//! Serve a radio's channel as rtl_tcp for a decoder worker: `iqserve_air <serial> <tuner MHz> <channel MHz> <rate MS/s> <secs>`.
//! Prints the port, then the server's counters every 5 s.

use std::time::{Duration, Instant};

use engine::iqserve::IqServeConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, Priority, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!("usage: iqserve_air <serial> <tuner MHz> <channel MHz> <rate MS/s> <secs>");
        std::process::exit(2);
    }
    let num = |i: usize| a[i].parse::<f64>().expect("a number");
    let (tuner, center, rate, secs) = (num(2) * 1e6, num(3) * 1e6, num(4) * 1e6, num(5));
    let device = backend_airspy::AirspyBackend::new().open(&a[1]).expect("open the radio");
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 5.0, ceiling_bytes: 1 << 30, sweep_secs: 1.0 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::off(),
    });
    let src = e.add_source(device, Mode::Live).expect("start the radio");
    src.set_center(tuner).expect("tune");
    std::thread::sleep(Duration::from_millis(300));
    let srv = e.serve_iq(src.id(), IqServeConfig { center_hz: center, rate, priority: Priority::EmitterDecoder }).expect("serve");
    println!("PORT {}", srv.addr().port());
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_secs(5));
        println!("{:5.0} s {:?}", t0.elapsed().as_secs_f64(), srv.stats());
    }
}
