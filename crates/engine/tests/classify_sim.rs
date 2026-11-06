//! The classify pool on the simulator (step 6 review focus 2–4): every live record is looked at, or counted as not;
//! the simulator's sync-only P25 (random payload) gets its sync-bank claim and no decoded one; a carrier and an FM
//! tone are described by shape and decoded as nothing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Evidence, SignalRecord};
use backend_sim::{Emitter, Keying, Modulation, P25_SYNC, SimConfig, SimDevice};
use engine::classify::ClassifyConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};

const CW_HZ: f64 = 855.0e6;
const P25_HZ: f64 = 851.0125e6;
const FM_HZ: f64 = 856.2125e6;

fn device() -> Box<dyn sdr_core::Device> {
    let emitters = vec![
        Emitter { freq_hz: CW_HZ, level_dbfs: -50.0, modulation: Modulation::Cw, keying: Keying::Continuous },
        Emitter {
            freq_hz: P25_HZ,
            level_dbfs: -40.0,
            modulation: Modulation::Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1800.0, sync: Some(P25_SYNC) },
            keying: Keying::Bursts { period_s: 2.5, duty: 0.6, phase: 0.3 },
        },
        Emitter {
            freq_hz: FM_HZ,
            level_dbfs: -42.0,
            modulation: Modulation::FmTone { tone_hz: 1000.0, dev_hz: 3000.0 },
            keying: Keying::Bursts { period_s: 2.0, duty: 0.5, phase: 0.25 },
        },
    ];
    let (d, _) = SimDevice::with_config(SimConfig { emitters, paced: true, report_lag_gaps: false, ..Default::default() });
    Box::new(d)
}

fn near(r: &SignalRecord, hz: f64) -> bool {
    (r.center_hz() - hz).abs() < 3e3
}

#[test]
fn every_record_is_looked_at_and_each_says_only_what_it_shows() {
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: ClassifyConfig { workers: 1, ..Default::default() },
    });
    let src = e.add_source(device(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_secs(6));
    e.remove_source(src.id());
    // The records the removal closed reach the scheduler on its next tick (their source is gone: counted, not looked
    // at), the windows still on the bus finish with their source, and the pool drains.
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    while e.classify_view().waiting > 0 {
        assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", e.classify_view());
        std::thread::sleep(Duration::from_millis(50));
    }
    let view = e.records().view();
    let recs: Vec<&Arc<SignalRecord>> = view.records.iter().collect();
    let c = e.classify_view();
    println!("{} records, {c:?}", recs.len());
    // Merged records were looked at too, so the counts may exceed the records left.
    assert!(c.looked + c.missed + c.dropped >= recs.len() as u64, "every record is looked at or counted: {c:?}");
    assert_eq!((c.dropped, c.survey), (0, 0), "{c:?}");
    assert!(c.missed <= 3, "only the records the source's removal closed miss their look: {c:?}");
    assert!(c.work_secs < c.looked_secs, "the pool keeps up: {c:?}");

    let decoded = |r: &SignalRecord| r.claims.iter().any(|c| matches!(c.by, Evidence::Decoder { .. }));
    let p25: Vec<_> = recs.iter().filter(|r| near(r, P25_HZ) && r.first_sample > 0).collect();
    assert!(!p25.is_empty(), "{recs:?}");
    for r in &p25 {
        let h = r.headline().unwrap_or_else(|| panic!("no headline: {:?}", r.claims));
        assert!(matches!(&h.by, Evidence::SyncBank { protocol, syncs, .. } if protocol == "P25 Phase 1" && *syncs >= 2), "{:?}", r.claims);
        assert!(!decoded(r), "random payload decoded: {:?}", r.claims);
    }
    for (hz, label) in [(CW_HZ, "Carrier"), (FM_HZ, "Analog FM")] {
        let mine: Vec<_> = recs.iter().filter(|r| near(r, hz)).collect();
        assert!(!mine.is_empty());
        for r in mine {
            assert!(r.headline().is_none() && !decoded(r), "{hz}: {:?}", r.claims);
            let f = r.claims.iter().find(|c| matches!(c.by, Evidence::Features { .. })).unwrap_or_else(|| panic!("{hz}: {:?}", r.claims));
            assert!(f.label == label && f.confidence <= 0.6, "{hz}: {f:?}");
        }
    }
    // The emitters vote the same way.
    let p25_emitter = view.emitters.iter().find(|v| (v.center_hz - P25_HZ).abs() < 3e3).unwrap();
    assert_eq!(p25_emitter.headline.as_ref().map(|c| c.label.as_str()), Some("P25 Phase 1"));
    assert!(view.emitters.iter().filter(|v| (v.center_hz - P25_HZ).abs() > 3e3).all(|v| v.headline.is_none()));
}

#[test]
fn a_record_that_starts_at_a_retune_is_looked_at() {
    // On air, every continuous signal opens a record at the retune that tunes it; its window, back-filled from the
    // segment's first sample, begins with the block that straddles the boundary (a gap). It must still be looked at.
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: ClassifyConfig { workers: 1, ..Default::default() },
    });
    let src = e.add_source(device(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let retuned_at = src.head();
    src.set_center(855.3e6).unwrap();
    let t0 = Instant::now();
    let cw = loop {
        let view = e.records().view();
        let r = view.records.iter().find(|r| near(r, CW_HZ) && r.open && r.first_sample >= retuned_at).cloned();
        if let Some(r) = r.as_ref().filter(|r| r.claims.iter().any(|c| matches!(c.by, Evidence::Features { .. }))) {
            break r.clone();
        }
        assert!(t0.elapsed() < Duration::from_secs(6), "the CW record after the retune was never looked at: {r:?} {:?}", e.classify_view());
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(cw.claims.iter().find(|c| matches!(c.by, Evidence::Features { .. })).unwrap().label, "Carrier");
    assert_eq!(e.classify_view().missed, 0, "{:?}", e.classify_view());
}
