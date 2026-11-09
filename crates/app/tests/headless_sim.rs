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
        classify: engine::classify::ClassifyConfig::off(),
    });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(GapFreeSim), Box::new(SlowBackend)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    let mut frame_ms: Vec<f64> = Vec::new();
    let mut opened: Option<SourceId> = None;
    let mut stage = None; // A's stage before it closes, for the frame-time message
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
            stage = handle.latest().receivers.iter().find(|r| r.id == id).map(|r| r.stage.clone());
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(app.pane(opened.unwrap()).is_none(), "the closed receiver's pane is gone");
    let frame_ms_by_k = frame_ms.clone();
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    let (p50, max) = (frame_ms[frame_ms.len() / 2], *frame_ms.last().unwrap());
    println!("headless frames: p50 {p50:.2} ms, max {max:.2} ms");
    // A 700 ms open on the snapshot thread must not reach the UI; 100 ms leaves room for a 2-vCPU CI runner's scheduling.
    let slow: Vec<usize> = (0..frame_ms.len()).filter(|&k| frame_ms_by_k[k] >= 100.0).collect();
    assert!(max < 100.0, "a frame took {max:.1} ms while the engine ran (slow open at frame 60); frames {slow:?}; stage {stage:?}");
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
        classify: engine::classify::ClassifyConfig::off(),
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
        classify: engine::classify::ClassifyConfig::off(),
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

#[test]
fn play_on_a_record_plays_it_and_stop_stops_it() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        // Plays advance at real time with no sound card.
        audio: engine::audio::AudioConfig::Paced,
        classify: engine::classify::ClassifyConfig::off(),
    });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(GapFreeSim)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    let input = |k: usize| egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0))),
        time: Some(k as f64 / 20.0),
        ..Default::default()
    };
    let mut frame_ms: Vec<f64> = Vec::new();
    let mut frame = |app: &mut App, k: usize| {
        let t = Instant::now();
        let mut output = ctx.run_ui(input(k), |ui| app.frame(ui));
        output.textures_delta.clear();
        frame_ms.push(t.elapsed().as_secs_f64() * 1e3);
        std::thread::sleep(Duration::from_millis(50));
    };
    handle.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    // The default scenario's continuous CW at 855.000 MHz: an open record, played at the live edge.
    let mut cw = None;
    for k in 0..100 {
        frame(&mut app, k);
        cw = handle.latest().records.records.iter().find(|r| (r.center_hz() - 855.0e6).abs() < 3e3 && r.open && r.rows > 10).map(|r| r.id);
        if cw.is_some() {
            break;
        }
    }
    let cw = cw.expect("an open record at 855.000 MHz");
    app.select(Some(cw));
    let play = app.inspector().play_command(cw);
    app.push_command(play);
    let mut playing = None;
    for k in 100..160 {
        frame(&mut app, k);
        let snap = handle.latest();
        if let Some(p) = spectrawl_app::inspector::Inspector::play_of(&snap, cw)
            && p.state == api::PlayState::Playing
        {
            playing = Some(p.clone());
            break;
        }
    }
    let p = playing.expect("the play reached Playing");
    let line = spectrawl_app::inspector::Inspector::play_line(&p);
    // 855 MHz is not an air band: NFM, chosen by band, from the ring.
    assert!(line.starts_with("playing") && line.contains("NFM (by band) from the ring"), "{line}");
    app.push_command(Command::StopPlay(p.id));
    let mut stopped = false;
    for k in 160..200 {
        frame(&mut app, k);
        if spectrawl_app::inspector::Inspector::play_of(&handle.latest(), cw).is_some_and(|p| p.state == api::PlayState::Stopped) {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "Stop did not stop it: {:?}", handle.latest().audio);
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    let max = *frame_ms.last().unwrap();
    assert!(max < 100.0, "a frame took {max:.1} ms");
}

/// The simulator's scenario tuned to 855.5 MHz with the committed real P25 control channel (NAC 0x34D) at 859.7625 MHz.
struct P25Sim;

impl Backend for P25Sim {
    fn name(&self) -> &str {
        "sim"
    }
    fn enumerate(&self) -> sdr_core::Result<Vec<DeviceInfo>> {
        SimBackend.enumerate()
    }
    fn open(&self, _serial: &str) -> sdr_core::Result<Box<dyn Device>> {
        let stem = format!("{}/../../testdata/small/859.7677MHz_P25_Phase_1", env!("CARGO_MANIFEST_DIR"));
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).expect("the P25 capture")).unwrap();
        let (fs, tuned) = (meta["global"]["core:sample_rate"].as_f64().unwrap(), meta["captures"][0]["core:frequency"].as_f64().unwrap());
        let w = std::f64::consts::TAU * (tuned - 859.7625e6) / fs;
        let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
        let x: Vec<sdr_core::Complex32> = raw
            .as_chunks::<8>()
            .0
            .iter()
            .enumerate()
            .map(|(k, c)| {
                sdr_core::Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]]))
                    * sdr_core::Complex32::from_polar(1.0, (w * k as f64) as f32)
            })
            .collect();
        let clip = backend_sim::Clip::leak(&x, fs, 8_500.0);
        let mut emitters = backend_sim::default_scenario();
        emitters.push(backend_sim::Emitter {
            freq_hz: 859.7625e6,
            level_dbfs: -45.0,
            modulation: backend_sim::Modulation::Iq { clip },
            keying: backend_sim::Keying::Continuous,
        });
        let (d, _) =
            SimDevice::with_config(SimConfig { emitters, center_hz: 855.5e6, paced: true, report_lag_gaps: false, ..Default::default() });
        Ok(Box::new(d))
    }
}

#[test]
fn the_systems_tab_shows_the_p25_site_its_decoder_and_its_record_says_the_same() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: Default::default(),
    });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(P25Sim)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    app.show_tab(spectrawl_app::tabs::Tab::Systems);
    let mut frame_ms: Vec<f64> = Vec::new();
    let mut frame = |app: &mut App, k: usize| {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0))),
            time: Some(k as f64 / 20.0),
            ..Default::default()
        };
        let t = Instant::now();
        let mut output = ctx.run_ui(input, |ui| app.frame(ui));
        output.textures_delta.clear();
        frame_ms.push(t.elapsed().as_secs_f64() * 1e3);
        std::thread::sleep(Duration::from_millis(50));
    };
    handle.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let mut k = 0;
    let row = loop {
        frame(&mut app, k);
        k += 1;
        let s = handle.latest();
        let locked = |e| s.decoders.iter().any(|d| d.emitter == e && d.lamp == api::Lamp::Locked);
        if let Some(row) = s.systems.systems.first().filter(|r| locked(r.emitter)) {
            break row.clone();
        }
        assert!(k < 200, "no locked P25 system in 10 s: {:?} {:?}", s.systems, s.decoders);
    };
    assert_eq!(row.identity.line(), "NAC 0x34D WACN BEE00 SYS 348 RFSS 1 SITE 13");
    // The control channel's newest record, selected: the list's "What" and the detail read the row's claim.
    let s = handle.latest();
    let rec = s.records.records.iter().filter(|r| r.emitter == Some(row.emitter)).max_by_key(|r| r.first_sample).unwrap().clone();
    assert_eq!(spectrawl_app::records_pane::what(&rec), ("P25 Phase 1 control".to_string(), false));
    app.select(Some(rec.id));
    app.show_tab(spectrawl_app::tabs::Tab::Records);
    for _ in 0..10 {
        frame(&mut app, k);
        k += 1;
    }
    let line = spectrawl_app::systems_pane::budget_line(&handle.latest());
    assert!(line.starts_with("Looked at ") && line.contains(" 0 missed"), "{line}");
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    let max = *frame_ms.last().unwrap();
    assert!(max < 100.0, "a frame took {max:.1} ms");
}

/// An AM transmitter keying on 124.850 MHz, alone.
struct AirSim;

impl Backend for AirSim {
    fn name(&self) -> &str {
        "sim"
    }
    fn enumerate(&self) -> sdr_core::Result<Vec<DeviceInfo>> {
        SimBackend.enumerate()
    }
    fn open(&self, _serial: &str) -> sdr_core::Result<Box<dyn Device>> {
        let emitters = vec![backend_sim::Emitter {
            freq_hz: 124.85e6,
            level_dbfs: -40.0,
            modulation: backend_sim::Modulation::AmTone { tone_hz: 1000.0, depth: 0.7 },
            keying: backend_sim::Keying::Bursts { period_s: 2.5, duty: 0.6, phase: 0.2 },
        }];
        let (d, _) =
            SimDevice::with_config(SimConfig { emitters, center_hz: 124.5e6, paced: true, report_lag_gaps: false, ..Default::default() });
        Ok(Box::new(d))
    }
}

#[test]
fn the_channels_tab_lists_an_air_band_call_and_its_record_reads_the_same() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: Default::default(),
    });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(AirSim)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    app.show_tab(spectrawl_app::tabs::Tab::Channels);
    let frame = |app: &mut App, k: usize| {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0))),
            time: Some(k as f64 / 20.0),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.frame(ui));
        output.textures_delta.clear();
        std::thread::sleep(Duration::from_millis(50));
    };
    handle.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let mut k = 0;
    let row = loop {
        frame(&mut app, k);
        k += 1;
        if let Some(row) = handle.latest().channels.channels.iter().find(|c| c.name == "124.850").cloned() {
            break row;
        }
        assert!(k < 300, "no channel row in 15 s: {:?}", handle.latest().channels);
    };
    assert_eq!((row.plan.as_str(), row.summary.as_str()), ("Air band", "AM voice"));
    // Its newest record headlines as the channel's voice, in the records list too.
    let s = handle.latest();
    let rec = s.records.records.iter().find(|r| Some(&r.id) == row.records.last()).unwrap().clone();
    assert_eq!(spectrawl_app::records_pane::what(&rec).0, "Air band voice");
    for _ in 0..5 {
        frame(&mut app, k);
        k += 1;
    }
}
