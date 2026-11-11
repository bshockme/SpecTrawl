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
    // A carrier is described and headlines nothing; an FM tone is measured analog FM, which headlines (owner,
    // 2026-11-06: analog modulations carry no code words; only digital modes need a sync or a decode to be named).
    for (hz, label, headlines) in [(CW_HZ, "Carrier", false), (FM_HZ, "Analog FM", true)] {
        let mine: Vec<_> = recs.iter().filter(|r| near(r, hz)).collect();
        assert!(!mine.is_empty());
        for r in mine {
            assert!(!decoded(r), "{hz}: {:?}", r.claims);
            let f = r.claims.iter().find(|c| matches!(c.by, Evidence::Features { .. })).unwrap_or_else(|| panic!("{hz}: {:?}", r.claims));
            assert!(f.label == label && f.confidence <= 0.6, "{hz}: {f:?}");
            assert_eq!(r.headline().map(|h| h.label.as_str()), headlines.then_some(label), "{hz}: {:?}", r.claims);
        }
    }
    // The emitters vote the same way.
    let headline =
        |hz: f64| view.emitters.iter().find(|v| (v.center_hz - hz).abs() < 3e3).and_then(|v| v.headline.as_ref()).map(|c| c.label.clone());
    assert_eq!(headline(P25_HZ).as_deref(), Some("P25 Phase 1"));
    assert_eq!(headline(FM_HZ).as_deref(), Some("Analog FM"));
    assert_eq!(headline(CW_HZ), None);
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

#[test]
fn a_call_whose_first_seconds_hold_no_sync_is_named_by_a_later_look() {
    // On air (851.90 MHz, 2026-11-10), a P25 call's first 2 s showed no sync the bank finds, and the rest decoded: the
    // record's one look, at its start, named nothing, and the call closed long before the 30 s relook. Here: one
    // transmission, 2.5 s of 4800 Bd 4FSK with no sync, then 3 s of P25 with its frame sync, every 12 s.
    let (period, hz) = (12.0, 851.2625e6);
    let fsk = |sync| Modulation::Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1800.0, sync };
    let emitters = vec![
        // On from 1 s (the gate is on while `t / period + phase` is within `duty` of a whole number).
        Emitter {
            freq_hz: hz,
            level_dbfs: -40.0,
            modulation: fsk(None),
            keying: Keying::Bursts { period_s: period, duty: 2.5 / period, phase: 1.0 - 1.0 / period },
        },
        Emitter {
            freq_hz: hz,
            level_dbfs: -40.0,
            modulation: fsk(Some(P25_SYNC)),
            keying: Keying::Bursts { period_s: period, duty: 3.0 / period, phase: 1.0 - 3.5 / period },
        },
    ];
    let (d, _) = SimDevice::with_config(SimConfig { emitters, paced: true, report_lag_gaps: false, ..Default::default() });
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: ClassifyConfig { workers: 1, ..Default::default() },
    });
    e.add_source(Box::new(d), Mode::Live).unwrap();
    // The call runs from 1 s to 6.5 s.
    std::thread::sleep(Duration::from_secs(8));
    let view = e.records().view();
    let calls: Vec<_> = view.records.iter().filter(|r| near(r, hz) && !r.open && r.end_sample - r.first_sample > 40_000_000).collect();
    assert!(!calls.is_empty(), "no 4 s record at {hz}: {:?}", view.records);
    for r in calls {
        let p25 = r.claims.iter().any(|c| matches!(&c.by, Evidence::SyncBank { protocol, .. } if protocol == "P25 Phase 1"));
        assert!(p25, "a {:.1} s call never named: {:?}", (r.end_sample - r.first_sample) as f64 / 10e6, r.claims);
    }
}

#[test]
fn a_long_call_whose_syncs_fall_between_its_looks_is_named_when_it_closes() {
    // On air (852.575 MHz, 2026-11-10), an 8.5 s P25 call's syncs fell where none of its looks were, and the whole call
    // decoded. Here a 9 s transmission of 4800 Bd 4FSK carries P25 frame syncs only 4.1–5.7 s in: between its looks
    // (cut about 0.2 s, 2.2 s and 6.2 s in), and inside the look at its last 8 s when it closes.
    let (period, hz) = (20.0, 851.2625e6);
    let fsk = |sync| Modulation::Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1800.0, sync };
    // On from `from` s for `secs` (the gate is on while `t / period + phase` is within `duty` of a whole number).
    let on = |from: f64, secs: f64| Keying::Bursts { period_s: period, duty: secs / period, phase: 1.0 - from / period };
    let emitters = vec![
        Emitter { freq_hz: hz, level_dbfs: -40.0, modulation: fsk(None), keying: on(1.0, 4.1) },
        Emitter { freq_hz: hz, level_dbfs: -40.0, modulation: fsk(Some(P25_SYNC)), keying: on(5.1, 1.6) },
        Emitter { freq_hz: hz, level_dbfs: -40.0, modulation: fsk(None), keying: on(6.7, 3.3) },
    ];
    let (d, _) = SimDevice::with_config(SimConfig { emitters, paced: true, report_lag_gaps: false, ..Default::default() });
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 15.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: ClassifyConfig { workers: 1, ..Default::default() },
    });
    e.add_source(Box::new(d), Mode::Live).unwrap();
    // The call closes about 10 s in and its closing look follows; a slow (debug, CI) build takes a while over it, so wait
    // for it until 20 s, before the next call (21 s in) could close.
    let named =
        |r: &SignalRecord| r.claims.iter().any(|c| matches!(&c.by, Evidence::SyncBank { protocol, .. } if protocol == "P25 Phase 1"));
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(10));
    loop {
        let view = e.records().view();
        let calls: Vec<_> = view.records.iter().filter(|r| near(r, hz) && !r.open && r.end_sample - r.first_sample > 80_000_000).collect();
        if !calls.is_empty() && calls.iter().all(|r| named(r)) {
            break;
        }
        if t0.elapsed() > Duration::from_secs(20) {
            assert!(!calls.is_empty(), "no 9 s record at {hz}: {:?}", view.records);
            for r in calls {
                assert!(named(r), "a {:.1} s call never named: {:?}", (r.end_sample - r.first_sample) as f64 / 10e6, r.claims);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
