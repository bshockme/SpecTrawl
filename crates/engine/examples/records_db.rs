//! The new engine headless into a record store: `records_db <backend> <serial> <db> "MHz:secs,..."`.
//!
//! Opens the device (`airspy` or `sim`), tunes to each stop for its seconds, and after each stop prints the confirmed
//! emitters seen within the usable span of that tuning (centre, width, records, SNR) and the stop's records; the store
//! keeps everything. The step 4 on-air check runs this beside `engine-old`'s `trawl_db` on another radio.
//!
//! With `SLICES=<dir>` each record's IQ slice is kept there too (no cap), to replay what was or wasn't named.

use std::time::Duration;

use engine::store::StoreConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Backend;

fn main() {
    env_logger_lite();
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: records_db <airspy|sim> <serial> <db> <MHz:secs>[,...]");
        std::process::exit(2);
    }
    let (backend, serial, db, stops) = (&a[1], &a[2], &a[3], &a[4]);
    let device = match backend.as_str() {
        "airspy" => backend_airspy::AirspyBackend::new().open(serial),
        "sim" => backend_sim::SimBackend.open(serial),
        other => panic!("no backend {other}"),
    }
    .expect("open the device");
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 2 << 30, sweep_secs: 1.0 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: Some(StoreConfig::new(db)),
        slices: std::env::var_os("SLICES")
            .map(|d| engine::recorder::SliceConfig { cap_bytes: u64::MAX, ..engine::recorder::SliceConfig::new(d) }),
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::default(),
    });
    if let Some(err) = e.store_error() {
        panic!("store: {err}");
    }
    let src = e.add_source(device, Mode::Live).expect("start the device");
    for stop in stops.split(',') {
        let (mhz, secs) = stop.split_once(':').expect("MHz:seconds");
        let (center, secs): (f64, u64) = (mhz.parse::<f64>().expect("MHz") * 1e6, secs.parse().expect("seconds"));
        src.set_center(center).expect("tune");
        let from = src.head();
        std::thread::sleep(Duration::from_secs(secs));
        let view = e.records().view();
        let half = src.snapshot().caps.usable_bandwidth_fraction * src.snapshot().sample_rate / 2.0;
        let mut emitters: Vec<_> = view.emitters.iter().filter(|m| m.confirmed && (m.center_hz - center).abs() < half).collect();
        emitters.sort_by(|a, b| a.center_hz.total_cmp(&b.center_hz));
        let records = view.records.iter().filter(|r| r.first_sample >= from && r.tuner_hz == center).count();
        println!("== {mhz} MHz after {secs} s: {} confirmed emitters, {records} records", emitters.len());
        let c = e.classify_view();
        println!(
            "   pool: {} threads, looked {}, missed {}, dropped {}, waiting {}; {:.1} s of work for {:.1} s of IQ",
            c.workers, c.looked, c.missed, c.dropped, c.waiting, c.work_secs, c.looked_secs
        );
        for m in &emitters {
            let snr = view.records.iter().filter(|r| r.emitter == Some(m.id)).map(|r| r.max_snr_db).fold(f32::MIN, f32::max);
            println!(
                "  {:10.4} MHz {:7.1} kHz recs {:4} snr {:5.1} duty {:4.2}",
                m.center_hz / 1e6,
                m.bandwidth_hz / 1e3,
                m.records,
                snr,
                m.duty
            );
        }
    }
    drop(src);
    let store = e.store().map(|s| s.view());
    drop(e);
    println!("filed in {db} ({store:?})");
}

/// Warnings to stderr without a logging crate.
fn env_logger_lite() {
    struct L;
    impl log::Log for L {
        fn enabled(&self, m: &log::Metadata) -> bool {
            // `LOG_CLASSIFY=1`: the pool's reasons for not looking at a record, too.
            m.level() <= log::Level::Warn || (m.target().starts_with("engine::classify") && std::env::var_os("LOG_CLASSIFY").is_some())
        }
        fn log(&self, r: &log::Record) {
            if self.enabled(r.metadata()) {
                eprintln!("{}: {}", r.level(), r.args());
            }
        }
        fn flush(&self) {}
    }
    let _ = log::set_logger(&L).map(|()| log::set_max_level(log::LevelFilter::Debug));
}
