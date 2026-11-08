//! Step 7 on air: `airband_air <serial> <MHz> <secs>`.
//!
//! Tunes an Airspy-protocol radio to `MHz` (an air band centre: 124.9 or 130.0) with the detector bank and the decoders
//! on, for `secs`, then prints the CHANNELS list, every ACARS block the bank decoded, and for each raster channel what
//! its records' looks said (the bank's AM test, decoders, or nothing). `PROBE_SLICES=<dir>` also keeps the records'
//! slices there.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use api::Evidence;
use engine::decoders::airband;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: airband_air <serial> <MHz> <secs>");
        std::process::exit(2);
    }
    let (serial, center, secs) = (&a[1], a[2].parse::<f64>().expect("MHz") * 1e6, a[3].parse::<u64>().expect("secs"));
    let device = backend_airspy::AirspyBackend::new().open(serial).expect("open the radio");
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 30.0, ceiling_bytes: 4 << 30, sweep_secs: 1.0 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: std::env::var_os("PROBE_SLICES").map(engine::recorder::SliceConfig::new),
        audio: engine::audio::AudioConfig::Manual,
        classify: Default::default(),
    });
    let src = e.add_source(device, Mode::Live).expect("start the radio");
    src.set_center(center).expect("tune");
    let t0 = Instant::now();
    // ACARS blocks as they are decoded (a record's claims can be replaced by a later look; keep what was seen).
    let mut acars: BTreeMap<u64, (f64, String)> = BTreeMap::new();
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(500));
        for r in e.records().view().records.iter() {
            for c in &r.claims {
                if let Evidence::Decoder { mode, summary, .. } = &c.by
                    && mode == "ACARS"
                {
                    acars.entry(r.id.0).or_insert_with(|| (r.center_hz(), summary.clone()));
                }
            }
        }
    }
    std::thread::sleep(Duration::from_secs(3));
    let c = e.classify_view();
    println!(
        "pool: {} threads, looked {}, missed {}, dropped {}; {:.1}% of real time",
        c.workers,
        c.looked,
        c.missed,
        c.dropped,
        100.0 * c.work_secs / c.looked_secs.max(1e-9)
    );
    let view = e.records().view();
    println!("\nCHANNELS:");
    for ch in airband::channels(&view).channels {
        println!(
            "  {} {:8} {} voice {:2} data {:2} carrier {:2}  {:5.1} s on air  peak {:5.1} dBFS  {}",
            if ch.open { "●" } else { " " },
            ch.name,
            ch.plan,
            ch.voice,
            ch.data,
            ch.carrier,
            ch.airtime_s,
            ch.peak_dbfs,
            ch.summary
        );
    }
    println!("\nACARS ({} blocks):", acars.len());
    for (id, (hz, s)) in &acars {
        println!("  #{id:<5} {:.4} MHz  {s}", hz / 1e6);
    }
    println!("\nevery raster channel's records, by what their looks said:");
    let mut by: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
    for r in view.records.iter() {
        let Some(ch) = airband::channel_of(r) else { continue };
        let said =
            r.claims.iter().filter(|c| !matches!(c.by, Evidence::BandPlan { .. })).map(|c| c.label.clone()).collect::<Vec<_>>().join(" + ");
        *by.entry(ch.name).or_default().entry(if said.is_empty() { "(nothing)".into() } else { said }).or_default() += 1;
    }
    for (ch, said) in by {
        let s: Vec<String> = said.iter().map(|(k, n)| format!("{n}× {k}")).collect();
        println!("  {ch}: {}", s.join(", "));
    }
}
