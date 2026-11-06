//! The sweep on the simulator (step 4a): stitched rows through the stage's history, every hop a segment, back to Live,
//! survey records, and the snapshot's view and commands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Command, Mode, Outcome, Role, SignalRecord};
use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::snapshot::PublisherConfig;
use engine::sweep::{STITCHED, SweepPlan};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, SourceConfig};

/// Two carriers in a 845–875 MHz sweep (four 9 MHz hops), tuned first to 855.25 MHz.
// Off the 5 MHz spur grid, which the sweep blanks.
const A_HZ: f64 = 851.3e6;
const B_HZ: f64 = 870.5e6;

fn device() -> Box<dyn sdr_core::Device> {
    sim(false)
}

/// `native`: the simulated device sweeps by itself, as an RFOne with sweep firmware does.
fn sim(native: bool) -> Box<dyn sdr_core::Device> {
    let cw = |freq_hz| Emitter { freq_hz, level_dbfs: -50.0, modulation: Modulation::Cw, keying: Keying::Continuous };
    let (d, _) = SimDevice::with_config(SimConfig {
        emitters: vec![cw(A_HZ), cw(B_HZ)],
        paced: true,
        report_lag_gaps: false,
        native_sweep: native,
        ..Default::default()
    });
    Box::new(d)
}

fn engine() -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 512 << 20, sweep_secs: 0.5 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::off(),
    })
}

fn wait_until(what: &str, timeout: Duration, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_sweep_publishes_stitched_passes_with_every_hop_a_segment() {
    let e = engine();
    let src = e.add_source(device(), engine::Mode::Live).unwrap();
    src.set_sweep_plan(SweepPlan { start_hz: 845e6, stop_hz: 875e6, ..Default::default() });
    let stage = e.bus().stage(src.id()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let segments_before = src.stats().segments;
    src.set_mode(engine::Mode::Sweep);
    wait_until("three passes", Duration::from_secs(10), || stage.sweep_status().is_some_and(|s| s.passes >= 3));
    let status = stage.sweep_status().unwrap();
    assert_eq!((status.hops, status.measured_hops), (4, 4), "{status:?}");
    assert!((status.hop_hz - 9e6).abs() < 1.0, "{status:?}");
    assert_eq!(status.unseen_retunes, 0, "{status:?}");
    // Every hop is a segment of its own in the source's table.
    assert!(src.stats().segments >= segments_before + 8, "{} segments after 3 passes of 4 hops", src.stats().segments);

    let rows = stage.rows().since(0);
    let passes: Vec<_> = rows.iter().filter(|r| r.segment.id.0 & STITCHED != 0).collect();
    assert!(passes.len() >= 3, "{} stitched rows", passes.len());
    let ids: std::collections::BTreeSet<u64> = passes.iter().map(|r| r.segment.id.0).collect();
    assert_eq!(ids.len(), 1, "the stitched segment changes between passes: {ids:?}");
    let r = passes.last().unwrap();
    // The row covers the hop grid: 845 MHz + 4 × 9 MHz, centred on its middle.
    assert!((r.segment.center_hz - 863e6).abs() < 1.0 && (r.segment.sample_rate - 36e6).abs() < 1.0, "{:?}", r.segment);
    assert_eq!(r.power.len(), 4 * 3686);
    assert!(r.power.iter().all(|p| p.is_finite() && *p > 0.0), "every hop measured");
    // Both carriers stand out where they are.
    let db: Vec<f32> = r.power.iter().map(|p| 10.0 * p.log10()).collect();
    let mut sorted = db.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 2];
    let bin_hz = r.segment.sample_rate / db.len() as f64;
    let bin = |f: f64| ((f - (r.segment.center_hz - r.segment.sample_rate / 2.0)) / bin_hz) as usize;
    for f in [A_HZ, B_HZ] {
        let k = bin(f);
        let peak = db[k.saturating_sub(3)..k + 4].iter().copied().fold(f32::MIN, f32::max);
        assert!(peak > floor + 30.0, "{f}: {peak:.1} dB over a {floor:.1} dB floor");
    }

    // Back to Live: the earlier centre, live rows again.
    src.set_mode(engine::Mode::Live);
    wait_until("the sweep to stop", Duration::from_secs(3), || stage.sweep_status().is_none());
    wait_until("a live row", Duration::from_secs(3), || {
        stage.rows().latest().is_some_and(|r| r.segment.id.0 & STITCHED == 0 && r.segment.center_hz == 855.25e6)
    });
    assert_eq!(src.current_segment().center_hz, 855.25e6);
}

#[test]
fn a_board_sweep_is_read_off_the_segment_table_and_stitched_the_same_way() {
    let e = engine();
    let src = e.add_source(sim(true), engine::Mode::Live).unwrap();
    src.set_sweep_plan(SweepPlan { start_hz: 845e6, stop_hz: 875e6, ..Default::default() });
    let stage = e.bus().stage(src.id()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let segments_before = src.stats().segments;
    src.set_mode(engine::Mode::Sweep);
    wait_until("twenty passes", Duration::from_secs(10), || stage.sweep_status().is_some_and(|s| s.passes >= 20));
    let status = stage.sweep_status().unwrap();
    assert!(status.on_board && status.tuner_lock, "{status:?}");
    assert_eq!((status.hops, status.measured_hops, status.unseen_retunes), (4, 4, 0), "{status:?}");
    // No retunes, guards or settle checks from here: just the board's hops.
    assert_eq!((status.lock_waits, status.settle_retries, status.slow_hops), (0, 0, 0), "{status:?}");
    assert!(src.stats().segments >= segments_before + 4 * 20, "{} segments after 20 passes of 4 hops", src.stats().segments);

    let rows = stage.rows().since(0);
    let passes: Vec<_> = rows.iter().filter(|r| r.segment.id.0 & STITCHED != 0).collect();
    let ids: std::collections::BTreeSet<u64> = passes.iter().map(|r| r.segment.id.0).collect();
    assert_eq!(ids.len(), 1, "the stitched segment changes between passes: {ids:?}");
    let r = passes.last().unwrap();
    assert!((r.segment.center_hz - 863e6).abs() < 1.0 && (r.segment.sample_rate - 36e6).abs() < 1.0, "{:?}", r.segment);
    assert_eq!(r.power.len(), 4 * 3686);
    assert!(r.power.iter().all(|p| p.is_finite() && *p > 0.0), "every hop measured");
    let db: Vec<f32> = r.power.iter().map(|p| 10.0 * p.log10()).collect();
    let mut sorted = db.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 2];
    let bin_hz = r.segment.sample_rate / db.len() as f64;
    let bin = |f: f64| ((f - (r.segment.center_hz - r.segment.sample_rate / 2.0)) / bin_hz) as usize;
    for f in [A_HZ, B_HZ] {
        let k = bin(f);
        let peak = db[k.saturating_sub(3)..k + 4].iter().copied().fold(f32::MIN, f32::max);
        assert!(peak > floor + 30.0, "{f}: {peak:.1} dB over a {floor:.1} dB floor");
    }

    // Back to Live: the board stops, the earlier centre and live rows return.
    src.set_mode(engine::Mode::Live);
    wait_until("the sweep to stop", Duration::from_secs(3), || stage.sweep_status().is_none());
    wait_until("a live row", Duration::from_secs(3), || {
        stage.rows().latest().is_some_and(|r| r.segment.id.0 & STITCHED == 0 && r.segment.center_hz == 855.25e6)
    });
    assert_eq!(src.current_segment().center_hz, 855.25e6);
}

#[test]
fn a_sweep_makes_survey_records_and_the_snapshot_shows_it() {
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
    let ack = |seq: u64| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(a) = h.latest().acks.iter().find(|a| a.seq == seq).cloned() {
                return a;
            }
            assert!(Instant::now() < deadline, "no ack for {seq}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let Ok(Outcome::Opened(id)) = ack(h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() })).result else { panic!() };
    assert_eq!(ack(h.send(Command::SetSweep { source: id, start_hz: 845e6, stop_hz: 875e6, segments: vec![] })).result, Ok(Outcome::Done));
    assert!(ack(h.send(Command::SetSweep { source: id, start_hz: 2e9, stop_hz: 3e9, segments: vec![] })).result.is_err());
    assert_eq!(ack(h.send(Command::SetMode { source: id, mode: Mode::Sweep })).result, Ok(Outcome::Done));
    let tune = ack(h.send(Command::Tune { source: id, center_hz: 860e6 }));
    assert!(tune.result.is_err(), "a sweeping receiver tuned: {tune:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let s = loop {
        let s = h.latest();
        let r = s.receivers.iter().find(|r| r.id == id).unwrap();
        if r.sweep.as_ref().is_some_and(|v| v.passes >= 4) && s.records.records.iter().any(|x| x.role == Role::Survey) {
            break s;
        }
        assert!(Instant::now() < deadline, "no survey records: {:?}", r.status);
        std::thread::sleep(Duration::from_millis(20));
    };
    let r = s.receivers.iter().find(|r| r.id == id).unwrap();
    assert_eq!(r.letter, 'S', "a sweeping receiver takes the sweep's letter");
    assert!(r.status.text.starts_with("Sweeping 845.0–881.0 MHz: pass "), "{}", r.status.text);
    assert!(r.rows.iter().all(|row| row.segment.id.0 & STITCHED != 0 || row.segment.center_hz == 855.25e6));
    let survey: Vec<_> = s.records.records.iter().filter(|x| x.role == Role::Survey).collect();
    for f in [A_HZ, B_HZ] {
        assert!(survey.iter().any(|x| (x.center_hz() - f).abs() < 30e3), "no survey record at {f}: {survey:?}");
    }
    // No position claims on a stitched row (and no IQ to look at); the band plan's entry, which needs neither.
    let only_band_plan = |x: &SignalRecord| x.claims.iter().all(|c| matches!(c.by, api::Evidence::BandPlan { .. }));
    assert!(survey.iter().all(|x| only_band_plan(x) && x.iq_clip.is_none()), "{survey:?}");
    let _ = Arc::clone(&s);
}
