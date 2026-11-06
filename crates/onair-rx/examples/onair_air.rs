//! A real OnAir as a worker on air: `onair_air <OnAir folder> <serial> <tuner MHz> <mode> <channel MHz> <secs>`.
//! The OnAir folder holds `release/<version>/bin/onair-server` (SpecTrawl's `onair/`). Prints the worker every 10 s and
//! the records its messages landed on.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::Evidence;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 7 {
        eprintln!("usage: onair_air <OnAir folder> <serial> <tuner MHz> <mode> <channel MHz> <secs>");
        std::process::exit(2);
    }
    let onair = onair_rx::OnAir::find(std::path::Path::new(&a[1])).expect("an OnAir release under the folder");
    let (tuner, mode, center, secs) =
        (a[3].parse::<f64>().unwrap() * 1e6, a[4].clone(), a[5].parse::<f64>().unwrap() * 1e6, a[6].parse::<f64>().unwrap());
    let device = backend_airspy::AirspyBackend::new().open(&a[2]).expect("open the radio");
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 2 << 30, sweep_secs: 1.0 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: Default::default(),
    });
    println!("worker: {}", onair.label);
    e.add_worker_factory(Arc::new(onair));
    let src = e.add_source(device, Mode::Live).expect("start the radio");
    src.set_center(tuner).expect("tune");
    std::thread::sleep(Duration::from_millis(300));
    let id = e.start_worker(src.id(), &mode, center).expect("start the worker");
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_secs(10));
        if let Some(v) = e.worker_views().into_iter().find(|v| v.id == id) {
            println!("{:5.0} s {:?} {} (restarts {}, error {:?})", t0.elapsed().as_secs_f64(), v.lamp, v.summary, v.restarts, v.error);
            for m in v.messages.iter().rev().take(3) {
                println!("        {:.4} MHz {}: {}", m.freq_hz / 1e6, m.kind, m.text.chars().take(100).collect::<String>());
            }
        }
    }
    let view = e.records().view();
    for r in view.records.iter() {
        for c in r.claims.iter().filter(|c| matches!(&c.by, Evidence::Decoder { mode: m, .. } if *m == mode)) {
            if let Evidence::Decoder { summary, .. } = &c.by {
                println!(
                    "record {} at {:.4} MHz: {} — {}",
                    r.id.0,
                    r.center_hz() / 1e6,
                    c.label,
                    summary.chars().take(120).collect::<String>()
                );
            }
        }
    }
}
