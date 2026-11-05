//! The whole step, headless: the engine's snapshot thread, the app's frames and the simulator, with no window.

use std::time::{Duration, Instant};

use api::{Command, SourceId};
use backend_sim::{SimBackend, SimConfig, SimDevice};
use egui::{Pos2, Rect, vec2};
use engine::snapshot::PublisherConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, SourceConfig};
use sdr_core::{Backend, Device, DeviceInfo};
use spectrawl_app::app::App;

/// The simulator's backend, but its device never reports a lag gap: a loaded runner must not put a hole in the history.
struct GapFreeSim;
impl Backend for GapFreeSim {
    fn name(&self) -> &str {
        "sim"
    }
    fn enumerate(&self) -> sdr_core::Result<Vec<DeviceInfo>> {
        SimBackend.enumerate()
    }
    fn open(&self, serial: &str) -> sdr_core::Result<Box<dyn Device>> {
        if serial != "SIM-0001" {
            return Err(sdr_core::Error::NotFound(serial.into()));
        }
        Ok(Box::new(SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() }).0))
    }
}

/// A backend whose `open` takes 700 ms: the snapshot thread stalls, the UI must not.
struct SlowBackend;
impl Backend for SlowBackend {
    fn name(&self) -> &str {
        "slow"
    }
    fn enumerate(&self) -> sdr_core::Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo { backend: "slow".into(), serial: "SLOW-1".into(), label: "Slow SDR".into() }])
    }
    fn open(&self, _serial: &str) -> sdr_core::Result<Box<dyn Device>> {
        std::thread::sleep(Duration::from_millis(700));
        Ok(Box::new(SimDevice::new(vec![])))
    }
}

#[test]
fn the_app_shows_the_simulator_and_never_blocks_on_the_engine() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
    });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(GapFreeSim), Box::new(SlowBackend)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    let mut frame_ms: Vec<f64> = Vec::new();
    let mut opened: Option<SourceId> = None;
    let input = |k: usize| egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0))),
        time: Some(k as f64 / 60.0),
        ..Default::default()
    };
    for k in 0..300 {
        if k == 10 {
            handle.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
        }
        if k == 60 {
            handle.send(Command::Open { backend: "slow".into(), serial: "SLOW-1".into() });
        }
        if k == 240 {
            handle.send(Command::Close(opened.unwrap()));
        }
        let t = Instant::now();
        // Nothing paints here, so the texture uploads are dropped on purpose.
        let mut output = ctx.run_ui(input(k), |ui| app.frame(ui));
        output.textures_delta.clear();
        frame_ms.push(t.elapsed().as_secs_f64() * 1e3);
        if opened.is_none() {
            opened = handle.latest().receivers.iter().find(|r| r.letter == 'A').map(|r| r.id);
        }
        if k == 230 {
            let id = opened.expect("the simulator opened as A");
            let pane = app.pane(id).expect("a pane for A");
            assert!(pane.history_len() >= 40, "{} rows after ~3.7 s", pane.history_len());
            // Engine rows overlap by one hop; the history must take that seam as contiguous, not as a hole.
            assert_eq!(pane.history_holes(), 0, "hole rows in a gap-free stream");
            assert!(pane.last_status().text.starts_with("Live at 855.250 MHz, 10 MS/s: holding "), "{}", pane.last_status().text);
            assert!(handle.latest().receivers.iter().any(|r| r.letter == 'B'), "the slow device opened as B");
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(app.pane(opened.unwrap()).is_none(), "the closed receiver's pane is gone");
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    let (p50, max) = (frame_ms[frame_ms.len() / 2], *frame_ms.last().unwrap());
    println!("headless frames: p50 {p50:.2} ms, max {max:.2} ms");
    // A 700 ms open on the snapshot thread must not reach the UI; 100 ms leaves room for a 2-vCPU CI runner's scheduling.
    assert!(max < 100.0, "a frame took {max:.1} ms while the engine ran (slow open at frame 60)");
    assert!(p50 < 10.0, "median frame {p50:.1} ms");
}

#[test]
fn records_list_and_box_with_one_snapshot_in_five() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
    });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(GapFreeSim)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    let input = |k: usize| egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0))),
        time: Some(k as f64 / 4.0),
        ..Default::default()
    };
    let frame = |app: &mut App, k: usize| {
        let mut output = ctx.run_ui(input(k), |ui| app.frame(ui));
        output.textures_delta.clear();
    };
    handle.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    // A frame every 250 ms: the app sees about one snapshot in five.
    for k in 0..24 {
        frame(&mut app, k);
        std::thread::sleep(Duration::from_millis(250));
    }
    let snap = handle.latest();
    let rows = app.records_pane().rows(&snap);
    // The list holds what the engine's view holds (nothing lost to the skipped snapshots).
    let ids = |v: &[std::sync::Arc<api::SignalRecord>]| v.iter().map(|r| r.id).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids(&rows), ids(&snap.records.records));
    // The default scenario's continuous emitters (P25 at 851.0125, CW at 855.000) are listed, unknown, with a box.
    let id = snap.receivers.iter().find(|r| r.letter == 'A').expect("the simulator opened").id;
    let pane = app.pane(id).expect("its pane");
    for hz in [851.0125e6, 855.0e6] {
        let r =
            rows.iter().find(|r| (r.center_hz() - hz).abs() < 3e3 && r.open).unwrap_or_else(|| panic!("no open record at {hz}: {rows:?}"));
        assert_eq!(spectrawl_app::records_pane::headline(r), "unknown");
        let (top, bottom) = pane.record_rows(r).unwrap_or_else(|| panic!("no box for {r:?}"));
        assert_eq!(top, 0, "an open record reaches the newest row: {r:?}");
        assert!(bottom > 20, "{r:?} spans rows {top}..{bottom}");
    }
    // Tagging from the app shows in a later snapshot, and the filter finds it.
    let cw = rows.iter().find(|r| (r.center_hz() - 855.0e6).abs() < 3e3).unwrap().id;
    app.select(Some(cw));
    app.push_command(Command::Tag { record: cw, tag: "birdie".into() });
    for k in 24..32 {
        frame(&mut app, k);
        std::thread::sleep(Duration::from_millis(100));
    }
    let snap = handle.latest();
    app.records_pane_mut().filter = "birdie".into();
    let found = app.records_pane().rows(&snap);
    assert_eq!(found.iter().map(|r| r.id).collect::<Vec<_>>(), [cw]);
    assert_eq!(found[0].tags, ["birdie"]);
    assert_eq!(app.selected(), Some(cw));
}

#[test]
fn a_sweeping_receiver_shows_its_passes_over_the_span() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.5 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
    });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(GapFreeSim)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    let input = |k: usize| egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0))),
        time: Some(k as f64 / 20.0),
        ..Default::default()
    };
    handle.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let mut id = None;
    for k in 0..80 {
        let mut output = ctx.run_ui(input(k), |ui| app.frame(ui));
        output.textures_delta.clear();
        if id.is_none()
            && let Some(r) = handle.latest().receivers.first()
        {
            id = Some(r.id);
            app.push_command(Command::SetSweep { source: r.id, start_hz: 845e6, stop_hz: 875e6, segments: vec![] });
            app.push_command(Command::SetMode { source: r.id, mode: api::Mode::Sweep });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let id = id.expect("the simulator opened");
    let r = handle.latest().receivers.iter().find(|r| r.id == id).cloned().unwrap();
    assert_eq!(r.letter, 'S');
    assert!(r.sweep.as_ref().is_some_and(|s| s.passes > 5), "{:?}", r.status);
    let pane = app.pane(id).expect("its pane");
    let (lo, hi) = pane.history_span().expect("rows in the waterfall");
    assert!((lo - 845e6).abs() < 1.0 && (hi - 881e6).abs() < 1.0, "the waterfall spans {lo}..{hi}");
    assert!(pane.history_len() > 5);
}
