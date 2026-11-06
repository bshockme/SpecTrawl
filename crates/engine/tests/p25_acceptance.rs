//! Step 6's acceptance (spec §6): a P25 system row within 10 s, with no contradiction between views. The owner's
//! 851–860 MHz capture was never recorded (decision 4), so the stream is the simulator's 850.5–860.5 MHz scenario with
//! the committed real P25 control channel (`testdata/small/859.7677MHz_P25_Phase_1`, NAC 0x34D) played into it at its
//! own frequency. The scenario's other P25-like emitters carry sync words over random symbols: they must be named by
//! their syncs and decoded as nothing.

use std::time::{Duration, Instant};

use api::{Command, Evidence, Identity, Lamp, Outcome, Snapshot};
use backend_sim::{Clip, Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::snapshot::PublisherConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, SourceConfig};
use sdr_core::Complex32;

const TUNER_HZ: f64 = 855.5e6;
/// The control channel the site announces; the capture was tuned 5.2 kHz above it.
const CONTROL_HZ: f64 = 859.7625e6;
const SITE: Identity = Identity::P25 { nac: 0x34D, wacn: Some(0xBEE00), system: Some(0x348), rfss: Some(1), site: Some(13) };

fn clip() -> &'static Clip {
    let stem = format!("{}/../../testdata/small/859.7677MHz_P25_Phase_1", env!("CARGO_MANIFEST_DIR"));
    let meta = std::fs::read_to_string(format!("{stem}.sigmf-meta")).expect("the P25 capture (testdata/small)");
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    let (fs, tuned) = (meta["global"]["core:sample_rate"].as_f64().unwrap(), meta["captures"][0]["core:frequency"].as_f64().unwrap());
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    // Centre the clip on the control channel before it is low-passed to it.
    let w = std::f64::consts::TAU * (tuned - CONTROL_HZ) / fs;
    let x: Vec<Complex32> = raw
        .as_chunks::<8>()
        .0
        .iter()
        .enumerate()
        .map(|(k, c)| {
            let v = Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]]));
            v * Complex32::from_polar(1.0, (w * k as f64) as f32)
        })
        .collect();
    Clip::leak(&x, fs, 8_500.0)
}

struct Sim;

impl sdr_core::Backend for Sim {
    fn name(&self) -> &str {
        "sim"
    }
    fn enumerate(&self) -> sdr_core::Result<Vec<sdr_core::DeviceInfo>> {
        sdr_core::Backend::enumerate(&backend_sim::SimBackend)
    }
    fn open(&self, _serial: &str) -> sdr_core::Result<Box<dyn sdr_core::Device>> {
        let mut emitters = backend_sim::default_scenario();
        emitters.push(Emitter {
            freq_hz: CONTROL_HZ,
            level_dbfs: -45.0,
            modulation: Modulation::Iq { clip: clip() },
            keying: Keying::Continuous,
        });
        let (d, _) =
            SimDevice::with_config(SimConfig { emitters, center_hz: TUNER_HZ, paced: true, report_lag_gaps: false, ..Default::default() });
        Ok(Box::new(d))
    }
}

fn p25(c: &api::Claim) -> Option<&Identity> {
    match &c.by {
        Evidence::Decoder { identity: Some(i @ Identity::P25 { .. }), .. } => Some(i),
        _ => None,
    }
}

#[test]
fn a_p25_system_row_within_10_s_that_every_view_agrees_with() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: Default::default(),
    });
    let h = engine::snapshot::spawn(engine, vec![Box::new(Sim)], PublisherConfig::default());
    let seq = h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let t0 = Instant::now();
    let full = |s: &Snapshot| s.systems.systems.iter().any(|r| r.identity == SITE);
    let s = loop {
        let s = h.latest();
        assert!(!s.acks.iter().any(|a| a.seq == seq && a.result.is_err()), "{:?}", s.acks);
        if full(&s) {
            break s;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "no system row in 10 s: {:?}, decoders {:?}, {:?}",
            s.systems,
            s.decoders,
            s.classify
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let secs = t0.elapsed().as_secs_f64();
    assert!(s.acks.iter().any(|a| a.seq == seq && matches!(a.result, Ok(Outcome::Opened(_)))));
    println!("system row after {secs:.1} s: {:?}", s.systems.systems);
    println!("decoders: {:?}", s.decoders);
    println!("classify: {:?}", s.classify);

    agree(&s);
    // Then the decoder attaches to the control channel, locks, and its claim (surer than a 1.5 s look) takes over in
    // every view at once.
    let s = loop {
        let s = h.latest();
        let decoded = s.systems.systems.first().is_some_and(|row| {
            s.decoders.iter().any(|d| d.emitter == row.emitter && d.lamp == Lamp::Locked) && row.summary.starts_with("P25 control 859.76")
        });
        if decoded {
            break s;
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "the decoder did not take over: {:?}, decoders {:?}", s.systems, s.decoders);
        std::thread::sleep(Duration::from_millis(20));
    };
    println!("decoder after {:.1} s: {:?}", t0.elapsed().as_secs_f64(), s.decoders);
    agree(&s);
    // The scenario's sync-only P25 emitters are named by their syncs and decoded as nothing (once their windows,
    // 1.5 s from their start, have been looked at).
    std::thread::sleep(Duration::from_secs(6).saturating_sub(t0.elapsed()));
    let s = h.latest();
    agree(&s);
    for hz in [851.0125e6, 851.2625e6] {
        let mine: Vec<_> = s.records.records.iter().filter(|r| (r.center_hz() - hz).abs() < 3e3).collect();
        for r in &mine {
            assert!(!r.claims.iter().any(|c| matches!(c.by, Evidence::Decoder { .. })), "{hz}: {:?}", r.claims);
        }
        assert!(
            mine.iter().any(|r| r.claims.iter().any(|c| matches!(&c.by, Evidence::SyncBank { protocol, .. } if protocol == "P25 Phase 1"))),
            "{hz}: no sync claim on {} records: {:?}",
            mine.len(),
            mine.iter().map(|r| &r.claims).collect::<Vec<_>>()
        );
    }
    println!("classify at 6 s: {:?}", s.classify);
}

/// One system at the control channel, and the row, the emitter's headline and its newest record's headline are one
/// identity; no other record names a P25 identity or disagrees with the row.
fn agree(s: &Snapshot) {
    assert_eq!(s.systems.systems.len(), 1, "{:?}", s.systems);
    let row = &s.systems.systems[0];
    assert_eq!(row.identity, SITE);
    assert!((row.control_hz - CONTROL_HZ).abs() < 2e3, "{row:?}");
    let em = s.records.emitters.iter().find(|e| e.id == row.emitter).unwrap();
    assert_eq!(em.headline.as_ref().and_then(p25), Some(&row.identity), "the emitter: {:?}", em.headline);
    let newest = s.records.records.iter().filter(|r| r.emitter == Some(row.emitter)).max_by_key(|r| r.first_sample).unwrap();
    assert_eq!(newest.headline().and_then(p25), Some(&row.identity), "the record: {:?}", newest.claims);
    assert!(row.records.contains(&newest.id));
    for r in s.records.records.iter() {
        for c in r.claims.iter().filter_map(p25) {
            assert!(r.emitter == Some(row.emitter) && c.same(&row.identity), "record {} claims {c:?}", r.id.0);
        }
    }
}
