//! The record book on the simulator: extents, burst timing, segment boundaries and the snapshot's view (step 4 review
//! focus 1 and 6).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Command, Outcome, RecordId, SignalRecord};
use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::snapshot::PublisherConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};

const CW_HZ: f64 = 855.0e6;
const P25_HZ: f64 = 851.0125e6;
const FM_HZ: f64 = 856.2125e6;

fn scenario() -> Vec<Emitter> {
    vec![
        Emitter { freq_hz: CW_HZ, level_dbfs: -50.0, modulation: Modulation::Cw, keying: Keying::Continuous },
        Emitter {
            freq_hz: P25_HZ,
            level_dbfs: -40.0,
            modulation: Modulation::Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1800.0, sync: None },
            keying: Keying::Continuous,
        },
        Emitter {
            freq_hz: FM_HZ,
            level_dbfs: -42.0,
            modulation: Modulation::FmTone { tone_hz: 1000.0, dev_hz: 3000.0 },
            keying: Keying::Bursts { period_s: 2.0, duty: 0.5, phase: 0.25 },
        },
    ]
}

fn device() -> Box<dyn sdr_core::Device> {
    let (d, _) = SimDevice::with_config(SimConfig { emitters: scenario(), paced: true, report_lag_gaps: false, ..Default::default() });
    Box::new(d)
}

fn engine() -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 512 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
    })
}

fn near(r: &SignalRecord, hz: f64) -> bool {
    (r.center_hz() - hz).abs() < 3e3
}

#[test]
fn records_have_the_right_extent_and_timing_and_never_span_a_retune() {
    let e = engine();
    let src = e.add_source(device(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_secs(5));
    let retuned_at = src.head();
    src.set_center(855.3e6).unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let view = e.records().view();
    let recs: Vec<&Arc<SignalRecord>> = view.records.iter().collect();
    assert!(!recs.is_empty());

    // No record spans two segments, and each names the one it is in.
    for r in &recs {
        let (a, b) = (src.segment_at(r.first_sample), src.segment_at(r.end_sample - 1));
        assert_eq!(a.id, b.id, "{r:?} spans {a:?} and {b:?}");
        assert_eq!(r.segment, a.id, "{r:?}");
    }

    // The continuous emitters: one record closed at the retune and one open after it, each the emitter's width.
    for (hz, lo_khz, hi_khz) in [(CW_HZ, 0.3, 2.5), (P25_HZ, 5.0, 16.0)] {
        let mine: Vec<_> = recs.iter().filter(|r| near(r, hz)).collect();
        let closed: Vec<_> = mine.iter().filter(|r| !r.open).collect();
        let open: Vec<_> = mine.iter().filter(|r| r.open).collect();
        assert!(closed.iter().any(|r| r.end_sample <= retuned_at + 2 * 7 * 32_768 + 65_536), "{hz}: none closed at the retune: {mine:?}");
        assert_eq!(open.len(), 1, "{hz}: {mine:?}");
        assert!(open[0].first_sample >= retuned_at, "{hz}: the open record starts before the retune: {:?}", open[0]);
        for r in &mine {
            let khz = r.bandwidth_hz() / 1e3;
            assert!((lo_khz..hi_khz).contains(&khz), "{hz}: {khz:.1} kHz: {r:?}");
        }
        assert_eq!(mine.iter().filter_map(|r| r.emitter).collect::<BTreeSet<_>>().len(), 1, "{hz}: one emitter: {mine:?}");
    }

    // The bursty channel: closed records about 1 s long (±3 rows of 22.9 ms plus the hangover), on one emitter. (It is
    // already on at sample 0, so the first burst is cut short by the stream's start.)
    let bursts: Vec<_> =
        recs.iter().filter(|r| near(r, FM_HZ) && !r.open && r.first_sample > 2_000_000 && r.end_sample < retuned_at).collect();
    assert!(bursts.len() >= 2, "{bursts:?}");
    for r in &bursts {
        let secs = (r.end_sample - r.first_sample) as f64 / 10e6;
        assert!((0.85..1.45).contains(&secs), "a 1 s burst lasted {secs:.3} s: {r:?}");
        assert!(r.t_end > r.t_start, "{r:?}");
        assert!(r.max_snr_db > 20.0 && r.peak_dbfs > r.mean_dbfs, "{r:?}");
    }
    assert_eq!(bursts.iter().filter_map(|r| r.emitter).collect::<BTreeSet<_>>().len(), 1);

    // Nothing else (no noise records, no edge blobs).
    let stray: Vec<_> = recs.iter().filter(|r| ![CW_HZ, P25_HZ, FM_HZ].iter().any(|&f| near(r, f))).collect();
    // A stage that fell behind and skipped ahead publishes short, noisier rows; say so if it did.
    let stage = e.bus().stage(src.id()).map(|s| s.stats());
    assert!(stray.is_empty(), "records where nothing transmits: {stray:?}; stage {stage:?}");

    // Every emitter in the view is one of the three.
    assert_eq!(view.emitters.len(), 3, "{:?}", view.emitters);
    let ids: BTreeSet<RecordId> = recs.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), recs.len(), "a record listed twice");
}

#[test]
fn reading_every_fifth_snapshot_misses_no_record() {
    struct Sim;
    impl sdr_core::Backend for Sim {
        fn name(&self) -> &str {
            "sim"
        }
        fn enumerate(&self) -> sdr_core::Result<Vec<sdr_core::DeviceInfo>> {
            sdr_core::Backend::enumerate(&backend_sim::SimBackend)
        }
        fn open(&self, _serial: &str) -> sdr_core::Result<Box<dyn sdr_core::Device>> {
            Ok(device())
        }
    }
    let h = engine::snapshot::spawn(engine(), vec![Box::new(Sim)], PublisherConfig::default());
    let seq = h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let t0 = Instant::now();
    let (mut seen_closed, mut last_version, mut k, mut opened) = (BTreeSet::new(), 0, 0u64, false);
    let mut last = h.latest();
    while t0.elapsed() < Duration::from_secs(7) {
        let s = h.latest();
        opened |= s.acks.iter().any(|a| a.seq == seq && matches!(a.result, Ok(Outcome::Opened(_))));
        if s.version != last.version {
            k += 1;
            if k % 5 == 0 {
                assert!(s.records.version >= last_version, "the records view went backwards");
                last_version = s.records.version;
                // Every closed record seen before is still in the view (nothing closed was dropped or renumbered).
                let now: BTreeSet<RecordId> = s.records.records.iter().map(|r| r.id).collect();
                assert!(seen_closed.is_subset(&now), "closed records vanished: {:?}", seen_closed.difference(&now).collect::<Vec<_>>());
                seen_closed.extend(s.records.records.iter().filter(|r| !r.open).map(|r| r.id));
            }
            last = s;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(opened);
    assert!(seen_closed.len() >= 2, "{} closed records in 7 s", seen_closed.len());
    // Consecutive snapshots share the view when nothing changed: the publisher doesn't rebuild it for nothing.
    let (a, b) = (h.latest(), h.latest());
    assert!(Arc::ptr_eq(&a.records, &b.records));
}

#[test]
fn a_tag_and_a_note_show_in_a_later_snapshot() {
    struct Sim;
    impl sdr_core::Backend for Sim {
        fn name(&self) -> &str {
            "sim"
        }
        fn enumerate(&self) -> sdr_core::Result<Vec<sdr_core::DeviceInfo>> {
            sdr_core::Backend::enumerate(&backend_sim::SimBackend)
        }
        fn open(&self, _serial: &str) -> sdr_core::Result<Box<dyn sdr_core::Device>> {
            Ok(device())
        }
    }
    let h = engine::snapshot::spawn(engine(), vec![Box::new(Sim)], PublisherConfig::default());
    h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let deadline = Instant::now() + Duration::from_secs(5);
    let id = loop {
        if let Some(r) = h.latest().records.records.iter().find(|r| near(r, CW_HZ)) {
            break r.id;
        }
        assert!(Instant::now() < deadline, "no CW record");
        std::thread::sleep(Duration::from_millis(20));
    };
    let t = h.send(Command::Tag { record: id, tag: "birdie".into() });
    let n = h.send(Command::Note { record: id, text: "steady line".into() });
    let bad = h.send(Command::Tag { record: RecordId(9_999_999), tag: "x".into() });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let s = h.latest();
        let acked = |q: u64| s.acks.iter().find(|a| a.seq == q).cloned();
        if let (Some(a), Some(b), Some(c)) = (acked(t), acked(n), acked(bad)) {
            assert_eq!(a.result, Ok(Outcome::Done));
            assert_eq!(b.result, Ok(Outcome::Done));
            assert!(c.result.is_err(), "{c:?}");
            let r = s.records.records.iter().find(|r| r.id == id).expect("the record");
            assert_eq!(r.tags, ["birdie"]);
            assert_eq!(r.notes.len(), 1);
            assert_eq!(r.notes[0].text, "steady line");
            assert!(r.notes[0].time >= r.t_start, "a note timed before its record: {:?}", r.notes[0]);
            break;
        }
        assert!(Instant::now() < deadline, "not acknowledged");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn claims_set_on_an_open_record_survive_its_rows_and_its_close_and_reach_its_emitter() {
    let e = engine();
    let src = e.add_source(device(), Mode::Live).unwrap();
    let wait_for = |what: &str, f: &dyn Fn() -> Option<Arc<SignalRecord>>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(r) = f() {
                break r;
            }
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let r = wait_for("an open CW record", &|| e.records().view().records.iter().find(|r| near(r, CW_HZ) && r.open).cloned());
    // The band plan names 855 MHz without being asked, as an inferred claim, and is never the headline.
    assert!(r.claims.iter().any(|c| matches!(c.by, api::Evidence::BandPlan { .. })), "{:?}", r.claims);
    assert!(r.headline().is_none());
    let sync = api::Claim::new(
        "P25 Phase 1",
        0.97,
        api::Evidence::SyncBank { protocol: "P25 Phase 1".into(), syncs: 9, frames_tried: 4, frames_decoded: 4 },
    );
    assert!(e.records().set_claims(r.id, "bank", vec![sync.clone()]));
    std::thread::sleep(Duration::from_millis(500));
    let again = e.records().get(r.id).expect("still held");
    assert!(again.open && again.rows > r.rows, "the record grew: {again:?}");
    assert_eq!(again.headline(), Some(&sync), "the claim survived {} more rows", again.rows - r.rows);
    let em = |id| e.records().view().emitters.iter().find(|v| Some(v.id) == id).and_then(|v| v.headline.clone());
    assert_eq!(em(again.emitter), Some(sync.clone()), "a strong sync claim names the emitter");
    // A retune closes it: the claim goes with it.
    src.set_center(855.3e6).unwrap();
    let closed = wait_for("the record closed", &|| e.records().get(r.id).filter(|r| !r.open));
    assert_eq!(closed.headline(), Some(&sync));
    // A maker's next set replaces its last; an empty set removes it.
    assert!(e.records().set_claims(r.id, "bank", vec![]));
    assert!(e.records().get(r.id).unwrap().headline().is_none());
    assert!(e.records().get(r.id).unwrap().claims.iter().any(|c| matches!(c.by, api::Evidence::BandPlan { .. })), "derived claims stay");
    assert!(!e.records().set_claims(RecordId(9_999_999), "bank", vec![sync]));
}
