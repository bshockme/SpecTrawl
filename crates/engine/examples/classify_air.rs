//! Step 6 on air: `classify_air <serial> <MHz> <secs>`.
//!
//! Tunes an Airspy-protocol radio to `MHz` with the detector bank and the decoders on, for `secs`, then prints the
//! pool's budget, the decoders, the systems, every emitter's headline with its records' headlines tallied, and any
//! record whose identity disagrees with its system row (there should be none).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use api::{Evidence, Identity};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: classify_air <serial> <MHz> <secs>");
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
    let mut first_system = None;
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(250));
        let view = e.records().view();
        if first_system.is_none() && !engine::decoders::systems(&view).systems.is_empty() {
            first_system = Some(t0.elapsed().as_secs_f64());
            println!("{:6.1} s: first system row", t0.elapsed().as_secs_f64());
        }
    }
    let c = e.classify_view();
    let load = 100.0 * c.work_secs / c.looked_secs.max(1e-9);
    println!(
        "\npool: {} threads, looked {}, missed {}, dropped {}, waiting {}; {load:.1}% of real time",
        c.workers, c.looked, c.missed, c.dropped, c.waiting
    );
    println!("\ndecoders:");
    for d in e.decoder_views() {
        println!("  {:?} {}  ({} ok, {} bad)", d.lamp, d.summary, d.blocks_ok, d.blocks_bad);
    }
    let view = e.records().view();
    let systems = engine::decoders::systems(&view);
    println!("\nsystems (first after {:?} s):", first_system.map(|s| (s * 10.0).round() / 10.0));
    for s in &systems.systems {
        println!("  {} at {:.4} MHz: {}  ({} records)", s.mode, s.control_hz / 1e6, s.identity.line(), s.records.len());
    }
    println!("\nemitters with a headline, and what their records headline as:");
    let mut ems: Vec<_> = view.emitters.iter().filter(|e| e.headline.is_some()).collect();
    ems.sort_by(|a, b| a.center_hz.total_cmp(&b.center_hz));
    for em in ems {
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for r in view.records.iter().filter(|r| r.emitter == Some(em.id)) {
            *tally.entry(r.headline().map_or("unknown".into(), |c| c.label.clone())).or_default() += 1;
        }
        let h = em.headline.as_ref().unwrap();
        let id = match &h.by {
            Evidence::Decoder { identity: Some(i), .. } => format!(" [{}]", i.line()),
            Evidence::SyncBank { syncs, .. } => format!(" [{syncs} syncs]"),
            _ => String::new(),
        };
        println!("  {:.4} MHz {:>5.1} kHz: {}{id}   records {:?}", em.center_hz / 1e6, em.bandwidth_hz / 1e3, h.label, tally);
    }
    let mut unknown: BTreeMap<String, usize> = BTreeMap::new();
    for r in view.records.iter().filter(|r| r.headline().is_none()) {
        let shape =
            r.claims.iter().find(|c| matches!(c.by, Evidence::Features { .. })).map_or("(no shape)".to_string(), |c| c.label.clone());
        *unknown.entry(shape).or_default() += 1;
    }
    println!("\nrecords with no headline, by shape: {unknown:?}");
    let mut bad = 0;
    for r in view.records.iter() {
        for c in &r.claims {
            let Evidence::Decoder { identity: Some(i @ Identity::P25 { .. }), .. } = &c.by else { continue };
            // Traffic channels name their NAC with no row of their own; only a control channel's row can disagree.
            let Some(row) = systems.systems.iter().find(|s| Some(s.emitter) == r.emitter) else { continue };
            if !row.identity.same(i) {
                bad += 1;
                println!(
                    "  record {} at {:.4} MHz claims {} but its system row is {}",
                    r.id.0,
                    r.center_hz() / 1e6,
                    i.line(),
                    row.identity.line()
                );
            }
        }
    }
    println!("\nrecords disagreeing with their system row: {bad}");
    let mut nacs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in view.records.iter() {
        for c in &r.claims {
            if let Evidence::Decoder { identity: Some(i), mode, .. } = &c.by {
                nacs.entry(format!("{mode}: {}", i.line())).or_default().push(format!("{:.4}", r.center_hz() / 1e6));
            }
        }
    }
    for (k, v) in &mut nacs {
        v.sort();
        v.dedup();
        println!("  {k}: {v:?}");
    }
    println!("{} records, {} emitters", view.records.len(), view.emitters.len());
    if std::env::var_os("DUMP").is_some() {
        let mut rs: Vec<_> = view.records.iter().collect();
        rs.sort_by(|a, b| a.center_hz().total_cmp(&b.center_hz()).then(a.first_sample.cmp(&b.first_sample)));
        for r in rs {
            let claims: Vec<String> = r
                .claims
                .iter()
                .filter(|c| !matches!(c.by, Evidence::BandPlan { .. }))
                .map(|c| match &c.by {
                    Evidence::SyncBank { syncs, frames_tried, frames_decoded, .. } => {
                        format!("{} ({syncs} syncs, {frames_decoded}/{frames_tried})", c.label)
                    }
                    Evidence::Decoder { summary, .. } => format!("{} [{summary}]", c.label),
                    _ => c.label.clone(),
                })
                .collect();
            println!(
                "DUMP {:.4} MHz {:5.1} kHz at {:7.2} s for {:7.2} s {:5} snr {:4.1}/{:4.1} em {:?} record {}: {claims:?}",
                r.center_hz() / 1e6,
                r.bandwidth_hz() / 1e3,
                r.first_sample as f64 / 10e6,
                (r.end_sample - r.first_sample) as f64 / 10e6,
                if r.open { "open" } else { "" },
                r.mean_snr_db,
                r.max_snr_db,
                r.emitter.map(|e| e.0),
                r.id.0
            );
        }
    }
}
