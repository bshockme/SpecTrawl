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
