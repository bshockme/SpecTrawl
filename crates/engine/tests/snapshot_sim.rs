//! The snapshot publisher against the simulator: cadence, rows, commands, device loss and shutdown.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Ack, Command, Handle, Health, Outcome, Row, Snapshot};
use backend_sim::SimConfig;
use dsp::channelizer::HOP;
use engine::snapshot::PublisherConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};

fn engine() -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
    })
}

fn spawn_sim() -> Handle {
    engine::snapshot::spawn(engine(), vec![Box::new(backend_sim::SimBackend)], PublisherConfig::default())
}

/// Poll `latest()` every 2 ms until `f` holds; panic with the last snapshot on timeout.
fn wait_for(h: &Handle, timeout: Duration, f: impl Fn(&Snapshot) -> bool) -> Arc<Snapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let s = h.latest();
        if f(&s) {
            return s;
        }
        if Instant::now() >= deadline {
            panic!("condition not met within {timeout:?}; last snapshot: {s:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The acknowledgement of command `seq`, waiting up to 2 s for it.
fn ack(h: &Handle, seq: u64) -> Ack {
    let s = wait_for(h, Duration::from_secs(2), |s| s.acks.iter().any(|a| a.seq == seq));
    s.acks.iter().find(|a| a.seq == seq).cloned().unwrap()
}

#[test]
fn snapshots_arrive_at_twenty_hertz_with_contiguous_rows() {
    let h = spawn_sim();
    let s = wait_for(&h, Duration::from_millis(500), |s| s.devices.iter().any(|d| d.info.serial == "SIM-0001"));
    assert!(s.version >= 1);
    let seq = h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let Ok(Outcome::Opened(id)) = ack(&h, seq).result else { panic!() };
    wait_for(&h, Duration::from_secs(1), |s| s.receivers.iter().any(|r| r.id == id && !r.rows.is_empty()));
    let t0 = Instant::now();
    let (mut versions, mut rows): (Vec<u64>, BTreeMap<u64, Arc<Row>>) = (vec![], BTreeMap::new());
    while t0.elapsed() < Duration::from_secs(2) {
        let s = h.latest();
        if versions.last() != Some(&s.version) {
            versions.push(s.version);
            let r = s.receivers.iter().find(|r| r.id == id).unwrap();
            for row in &r.rows {
                rows.entry(row.first_sample).or_insert_with(|| row.clone());
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(versions.windows(2).all(|w| w[1] > w[0]));
    assert!((34..=46).contains(&versions.len()), "{} snapshots in 2 s", versions.len());
    assert!(rows.len() >= 30, "{} rows", rows.len());
    // Block b spans (b - 1)L .. (b + 1)L with L = N/2, so each row's first block is the hop after the previous row's last:
    // the next row starts one hop before the previous one ends.
    let steps: Vec<(u64, u64)> = rows.values().zip(rows.values().skip(1)).map(|(a, b)| (a.end_sample, b.first_sample)).collect();
    assert!(steps.iter().all(|&(end, next)| next + HOP as u64 == end), "rows are not contiguous: {steps:?}");
    let r = h.latest().receivers.iter().find(|r| r.id == id).cloned().unwrap();
    assert_eq!(r.letter, 'A');
    assert_eq!(r.status.health, Health::Ok);
    assert!(r.status.text.starts_with("Live at 855.250 MHz, 10 MS/s: holding "), "{}", r.status.text);
    assert!(r.rows.len() <= 16);
}

#[test]
fn a_tune_is_acknowledged_and_shows_in_the_snapshot_and_the_log() {
    let h = spawn_sim();
    let seq = h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let Ok(Outcome::Opened(id)) = ack(&h, seq).result else { panic!() };
    let seq = h.send(Command::Tune { source: id, center_hz: 851.25e6 });
    assert!(matches!(ack(&h, seq).result, Ok(Outcome::Done)));
    let s = wait_for(&h, Duration::from_millis(800), |s| {
        s.receivers
            .iter()
            .any(|r| r.id == id && r.center_hz == 851.25e6 && r.rows.last().is_some_and(|row| row.segment.center_hz == 851.25e6))
    });
    let r = s.receivers.iter().find(|r| r.id == id).unwrap();
    assert!(r.status.text.contains("851.250 MHz"));
    assert!(s.log.iter().any(|l| l.receiver == Some('A') && l.text == "A tuned to 851.250 MHz"), "{:?}", s.log);
    // Out of the device's range: acknowledged with the error, tuning unchanged.
    let seq = h.send(Command::Tune { source: id, center_hz: 10e9 });
    let a = ack(&h, seq);
    assert!(a.result.is_err(), "{a:?}");
    assert_eq!(h.latest().receivers[0].center_hz, 851.25e6);
    assert!(h.latest().log.iter().any(|l| l.text.starts_with(&format!("command {seq} failed: "))));
}

#[test]
fn a_lost_device_is_said_so_with_the_reason() {
    let engine = engine();
    let (d, control) = backend_sim::SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() });
    let src = engine.add_source(Box::new(d), Mode::Live).unwrap();
    let h = engine::snapshot::spawn(engine, vec![], PublisherConfig::default());
    wait_for(&h, Duration::from_secs(1), |s| s.receivers.iter().any(|r| r.id == src.id() && !r.rows.is_empty()));
    control.drop_device();
    let s = wait_for(&h, Duration::from_millis(3500), |s| s.receivers[0].status.health == Health::Lost);
    assert_eq!(s.receivers[0].status.text, "Device lost");
    assert!(s.receivers[0].status.reason.as_deref().unwrap().starts_with("no samples for "));
    assert!(s.log.iter().any(|l| l.text.ends_with("device lost")));
}

#[test]
fn closing_frees_the_letter_and_opening_twice_is_refused() {
    let h = spawn_sim();
    let open = || h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let Ok(Outcome::Opened(a)) = ack(&h, open()).result else { panic!() };
    assert_eq!(ack(&h, open()).result, Err("already open as A".to_string()));
    let seq = h.send(Command::Close(a));
    assert!(matches!(ack(&h, seq).result, Ok(Outcome::Done)));
    let s = wait_for(&h, Duration::from_millis(500), |s| s.receivers.is_empty());
    assert_eq!(s.devices.iter().find(|d| d.info.serial == "SIM-0001").unwrap().open_as, None);
    let Ok(Outcome::Opened(b)) = ack(&h, open()).result else { panic!() };
    assert_ne!(a, b);
    let s = wait_for(&h, Duration::from_millis(500), |s| s.receivers.len() == 1);
    assert_eq!(s.receivers[0].letter, 'A');
    assert_eq!(
        ack(&h, h.send(Command::Open { backend: "none".into(), serial: "x".into() })).result,
        Err("no backend named none".to_string())
    );
}

#[test]
fn dropping_the_handle_stops_the_engine() {
    let engine = engine();
    let (d, control) = backend_sim::SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() });
    engine.add_source(Box::new(d), Mode::Live).unwrap();
    let h = engine::snapshot::spawn(engine, vec![], PublisherConfig::default());
    wait_for(&h, Duration::from_secs(1), |s| !s.receivers.is_empty());
    drop(h);
    std::thread::sleep(Duration::from_millis(300));
    let sent = control.samples_sent();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(control.samples_sent(), sent, "the device is still streaming after the handle was dropped");
}
