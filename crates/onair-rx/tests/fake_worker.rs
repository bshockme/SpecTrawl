//! `onair-rx` against a stand-in `onair-server` (the `fake_onair_server` binary), through the engine: the worker is
//! started on SpecTrawl's feed, polled, its message lands on the record, a crash is restarted, a worker that picks a
//! real radio is stopped, and stopping the worker stops its process.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Evidence, Lamp};
use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use onair_rx::OnAir;

const CW_HZ: f64 = 855.0e6;

struct Dir(PathBuf);
impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("spectrawl-onair-rx-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Dir(d)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fake(dir: &Dir, extra: &[&str]) -> OnAir {
    let mut o = OnAir::new(PathBuf::from(env!("CARGO_BIN_EXE_fake_onair_server")), "fake OnAir", dir.0.clone());
    o.extra = extra.iter().map(|s| s.to_string()).collect();
    o
}

fn engine() -> (Engine, Arc<engine::Source>) {
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::off(),
    });
    let cw = Emitter { freq_hz: CW_HZ, level_dbfs: -45.0, modulation: Modulation::Cw, keying: Keying::Continuous };
    let (d, _) = SimDevice::with_config(SimConfig { emitters: vec![cw], paced: true, report_lag_gaps: false, ..Default::default() });
    let src = e.add_source(Box::new(d), Mode::Live).unwrap();
    (e, src)
}

fn wait<T>(secs: f64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t0.elapsed().as_secs_f64() < secs, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_worker_runs_on_the_feed_and_its_message_lands_on_the_record() {
    let dir = Dir::new("run");
    let (e, src) = engine();
    e.add_worker_factory(Arc::new(fake(&dir, &[])));
    assert!(e.worker_modes().iter().any(|m| m.id == "flex" && m.by == "fake OnAir"));
    let id = e.start_worker(src.id(), "flex", CW_HZ).unwrap();
    let v = wait(10.0, "the worker locked", || e.worker_views().into_iter().find(|v| v.id == id && v.lamp == Lamp::Locked));
    assert!(v.summary.starts_with("FLEX 855.0000 MHz: locked"), "{}", v.summary);
    let rec = wait(10.0, "the page's claim", || {
        e.records()
            .view()
            .records
            .iter()
            .find(|r| r.claims.iter().any(|c| matches!(&c.by, Evidence::Decoder { mode, .. } if mode == "flex")))
            .cloned()
    });
    let c = rec.claims.iter().find(|c| matches!(c.by, Evidence::Decoder { .. })).unwrap();
    assert_eq!(c.label, "FLEX (fake OnAir)");
    assert!(matches!(&c.by, Evidence::Decoder { summary, .. } if summary.starts_with("fake page")), "{c:?}");
    // Its settings name SpecTrawl's feed and never a radio.
    let run = std::fs::read_dir(&dir.0).unwrap().next().unwrap().unwrap().path();
    let conf = std::fs::read_to_string(run.join("server.conf")).unwrap();
    assert!(conf.contains("device=rtl_tcp\n") && conf.contains("net_radio=rtltcp,127.0.0.1,"), "{conf}");
    // Stopping the worker stops its process: its control port stops answering.
    let port: u16 = conf.lines().find_map(|l| l.strip_prefix("port=")).unwrap().parse().unwrap();
    assert!(onair_rx::http::get(port, "/api/v1/status").is_ok());
    assert!(e.stop_worker(id));
    wait(5.0, "the process to go", || onair_rx::http::get(port, "/api/v1/status").is_err().then_some(()));
}

#[test]
fn a_worker_that_crashes_is_restarted() {
    let dir = Dir::new("crash");
    let (e, src) = engine();
    e.add_worker_factory(Arc::new(fake(&dir, &["fake_exit_after_secs=1.0"])));
    let id = e.start_worker(src.id(), "flex", CW_HZ).unwrap();
    let v = wait(15.0, "a restart", || e.worker_views().into_iter().find(|v| v.id == id && v.restarts >= 1 && v.lamp == Lamp::Locked));
    assert!(v.error.is_none(), "{v:?}");
}

#[test]
fn a_worker_that_chooses_a_real_radio_is_stopped() {
    let dir = Dir::new("radio");
    let (e, src) = engine();
    e.add_worker_factory(Arc::new(fake(&dir, &["fake_device=Airspy 2F650AE3 (native, experimental)"])));
    let err = e.start_worker(src.id(), "flex", CW_HZ).unwrap_err();
    assert!(err.contains("chose the radio") && err.contains("Airspy"), "{err}");
    assert!(e.worker_views().is_empty());
}

#[test]
fn the_newest_release_is_found() {
    let dir = Dir::new("find");
    for v in ["3.5.0", "3.6.0", "3.10.1"] {
        let bin = dir.0.join("release").join(v).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(if cfg!(windows) { "onair-server.exe" } else { "onair-server" }), b"").unwrap();
    }
    let o = OnAir::find(&dir.0).unwrap();
    assert_eq!(o.label, "OnAir 3.10.1");
    assert_eq!(o.runs, dir.0.join("run"));
    assert!(OnAir::find(&dir.0.join("nothing")).is_none());
}
