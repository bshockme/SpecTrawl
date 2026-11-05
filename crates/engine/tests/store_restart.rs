//! The record store: records survive a restart, and a slow store never stalls the engine (step 4 review focus 4 and 5).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Command, Outcome, RecordId, RecordQuery, SignalRecord};
use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::snapshot::PublisherConfig;
use engine::store::StoreConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};

fn scenario() -> Vec<Emitter> {
    vec![
        Emitter { freq_hz: 855.0e6, level_dbfs: -50.0, modulation: Modulation::Cw, keying: Keying::Continuous },
        Emitter {
            freq_hz: 856.2125e6,
            level_dbfs: -42.0,
            modulation: Modulation::FmTone { tone_hz: 1000.0, dev_hz: 3000.0 },
            keying: Keying::Bursts { period_s: 1.0, duty: 0.5, phase: 0.25 },
        },
    ]
}

fn device() -> Box<dyn sdr_core::Device> {
    let (d, _) = SimDevice::with_config(SimConfig { emitters: scenario(), paced: true, report_lag_gaps: false, ..Default::default() });
    Box::new(d)
}

fn engine(store: StoreConfig) -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 512 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: Some(store),
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
    })
}

/// A fresh directory for one test's store, removed when dropped.
struct Dir(PathBuf);
impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("spectrawl-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Dir(d)
    }
    fn db(&self) -> PathBuf {
        self.0.join("records.db")
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn closed(e: &Engine) -> BTreeMap<RecordId, Arc<SignalRecord>> {
    e.records().view().records.iter().filter(|r| !r.open).map(|r| (r.id, r.clone())).collect()
}

fn ms(t: std::time::SystemTime) -> i128 {
    t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i128
}

#[test]
fn records_tags_and_notes_survive_a_restart_with_their_ids() {
    let dir = Dir::new("restart");
    let (before, tagged, noted) = {
        let e = engine(StoreConfig::new(dir.db()));
        assert!(e.store().is_some(), "{:?}", e.store_error());
        e.add_source(device(), Mode::Live).unwrap();
        std::thread::sleep(Duration::from_secs(6));
        let c = closed(&e);
        assert!(c.len() >= 4, "{} closed records", c.len());
        let mut ids = c.keys().copied();
        let (tagged, noted) = (ids.next().unwrap(), ids.next().unwrap());
        assert!(e.records().tag(tagged, "keep", true));
        assert!(e.records().note(noted, "seen twice"));
        let before = closed(&e);
        (before, tagged, noted)
        // Dropping the engine closes the open records and flushes the store.
    };
    let e = engine(StoreConfig::new(dir.db()));
    let after = closed(&e);
    for (id, r) in &before {
        let a = after.get(id).unwrap_or_else(|| panic!("record {} lost: {r:?}", id.0));
        assert_eq!(
            (a.first_sample, a.end_sample, a.segment, a.emitter),
            (r.first_sample, r.end_sample, r.segment, r.emitter),
            "{a:?} vs {r:?}"
        );
        assert!((ms(a.t_start) - ms(r.t_start)).abs() <= 1 && (ms(a.t_end) - ms(r.t_end)).abs() <= 1);
        assert_eq!((a.f_lo_hz, a.f_hi_hz, a.rows), (r.f_lo_hz, r.f_hi_hz, r.rows));
        assert_eq!(a.claims, r.claims);
        assert_eq!(a.tags, r.tags);
        assert_eq!(a.notes.iter().map(|n| &n.text).collect::<Vec<_>>(), r.notes.iter().map(|n| &n.text).collect::<Vec<_>>());
    }
    assert_eq!(after[&tagged].tags, ["keep"]);
    assert_eq!(after[&noted].notes[0].text, "seen twice");
    // The records still open at the drop were closed and kept too.
    assert!(after.len() > before.len(), "{} after vs {} before", after.len(), before.len());
    // The emitters came back with their ids.
    let emitters: Vec<_> = e.records().view().emitters.iter().map(|v| v.id).collect();
    for r in before.values() {
        assert!(emitters.contains(&r.emitter.unwrap()), "emitter {:?} not loaded", r.emitter);
    }
    // New records get ids above every old one.
    let old_max = after.keys().max().unwrap().0;
    e.add_source(device(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let fresh: Vec<RecordId> = e.records().view().records.iter().filter(|r| !after.contains_key(&r.id)).map(|r| r.id).collect();
    assert!(!fresh.is_empty());
    assert!(fresh.iter().all(|id| id.0 > old_max), "reused ids: {fresh:?} (old max {old_max})");
}

#[test]
fn a_slow_store_never_stalls_the_engine() {
    let dir = Dir::new("slow");
    let cfg = StoreConfig { queue: 2, write_delay: Duration::from_secs(2), shutdown: Duration::from_secs(1), ..StoreConfig::new(dir.db()) };
    let (h, guard) = engine::snapshot::spawn_with_guard(engine(cfg), vec![Box::new(Sim)], PublisherConfig::default());
    let seq = h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let t0 = Instant::now();
    let (mut versions, mut first) = (0u64, None);
    while t0.elapsed() < Duration::from_secs(5) {
        let s = h.latest();
        if first.is_none() && s.acks.iter().any(|a| a.seq == seq) {
            first = Some((s.version, Instant::now()));
        }
        versions = s.version;
        std::thread::sleep(Duration::from_millis(10));
    }
    let (v0, t1) = first.expect("the open was acknowledged");
    let rate = (versions - v0) as f64 / t1.elapsed().as_secs_f64();
    assert!((16.0..24.0).contains(&rate), "{rate:.1} snapshots/s with the store stalled");
    let s = h.latest();
    let store = s.store.clone().expect("a store view");
    assert!(store.dropped > 0, "{store:?}");
    assert!(store.error.is_none(), "{store:?}");
    assert!(s.records.records.len() >= 4, "records kept flowing: {}", s.records.records.len());
    drop(s);
    drop(h);
    let t = Instant::now();
    assert!(guard.join(Duration::from_secs(5)), "the engine did not stop");
    assert!(t.elapsed() < Duration::from_secs(4), "shutdown took {:?}", t.elapsed());
}

#[test]
fn a_history_search_finds_tagged_records_in_a_later_snapshot() {
    let dir = Dir::new("query");
    let h = engine::snapshot::spawn(engine(StoreConfig::new(dir.db())), vec![Box::new(Sim)], PublisherConfig::default());
    h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let deadline = Instant::now() + Duration::from_secs(6);
    let id = loop {
        if let Some(r) = h.latest().records.records.iter().find(|r| !r.open) {
            break r.id;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    h.send(Command::Tag { record: id, tag: "interesting".into() });
    h.send(Command::Note { record: id, text: "the morning burst".into() });
    std::thread::sleep(Duration::from_millis(600)); // a batch or two
    let by_tag = h.send(Command::QueryRecords(RecordQuery { tag: Some("interesting".into()), limit: 10, ..Default::default() }));
    let answer = |seq: u64| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let s = h.latest();
            if let Some(q) = s.query.clone().filter(|q| q.seq == seq) {
                assert!(s.acks.iter().any(|a| a.seq == seq && a.result == Ok(Outcome::Queried)));
                return q;
            }
            assert!(Instant::now() < deadline, "no answer to {seq}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let q = answer(by_tag);
    assert_eq!(q.records.iter().map(|r| r.id).collect::<Vec<_>>(), [id]);
    assert_eq!(q.records[0].tags, ["interesting"]);
    let by_word = h.send(Command::QueryRecords(RecordQuery { text: Some("morning".into()), limit: 10, ..Default::default() }));
    assert_eq!(answer(by_word).records.iter().map(|r| r.id).collect::<Vec<_>>(), [id]);
    let none = h.send(Command::QueryRecords(RecordQuery { text: Some("evening".into()), limit: 10, ..Default::default() }));
    assert!(answer(none).records.is_empty());
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
        Ok(device())
    }
}
