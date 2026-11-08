//! Decoder workers through the engine (step 7, `engine::workers`), with a fake worker program: it reads its rtl_tcp feed
//! and "decodes" one message at the simulator's carrier, which lands as a claim on that carrier's record.

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use api::{Evidence, Lamp, WorkerMessage, WorkerMode};
use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::workers::{Worker, WorkerFactory, WorkerReport, WorkerSpec};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};

const CW_HZ: f64 = 855.0e6;

struct Fake {
    bytes: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    sent: bool,
}

impl Worker for Fake {
    fn poll(&mut self) -> WorkerReport {
        let n = self.bytes.load(Ordering::Relaxed);
        let mut r =
            WorkerReport { lamp: Some(if n > 0 { Lamp::Locked } else { Lamp::Searching }), blocks_ok: n / 4096, ..Default::default() };
        r.summary = format!("fake: {n} bytes");
        if n > 1_000_000 && !self.sent {
            self.sent = true;
            r.messages.push(WorkerMessage {
                time: SystemTime::now(),
                freq_hz: CW_HZ + 300.0,
                kind: "test.message".into(),
                text: "hello".into(),
            });
        }
        r
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct FakeFactory;

impl WorkerFactory for FakeFactory {
    fn name(&self) -> String {
        "fake 1".into()
    }
    fn modes(&self) -> Vec<WorkerMode> {
        vec![WorkerMode { id: "fake".into(), name: "Fake".into(), by: "fake 1".into(), rate: 1.3e6 }]
    }
    fn start(&self, spec: &WorkerSpec) -> Result<Box<dyn Worker>, String> {
        assert_eq!(spec.rate, 1.25e6, "the nearest exact rate under the mode's");
        let (bytes, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let (b, st, addr) = (bytes.clone(), stop.clone(), spec.iq);
        std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
            let mut buf = vec![0u8; 65536];
            while !st.load(Ordering::Relaxed) {
                if let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    b.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        });
        Ok(Box::new(Fake { bytes, stop, sent: false }))
    }
}

fn engine() -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::off(),
    })
}

fn device() -> Box<dyn sdr_core::Device> {
    let cw = Emitter { freq_hz: CW_HZ, level_dbfs: -45.0, modulation: Modulation::Cw, keying: Keying::Continuous };
    let (d, _) = SimDevice::with_config(SimConfig { emitters: vec![cw], paced: true, report_lag_gaps: false, ..Default::default() });
    Box::new(d)
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
fn a_worker_is_fed_polled_and_its_messages_land_on_records() {
    let e = engine();
    assert!(e.worker_modes().is_empty());
    e.add_worker_factory(Arc::new(FakeFactory));
    assert_eq!(e.worker_modes().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["fake"]);
    let src = e.add_source(device(), Mode::Live).unwrap();
    assert!(e.start_worker(src.id(), "nonesuch", 855.25e6).unwrap_err().contains("no installed worker"));
    let id = e.start_worker(src.id(), "fake", 855.25e6).unwrap();
    let v = wait(5.0, "the worker locked", || e.worker_views().into_iter().find(|v| v.id == id && v.lamp == Lamp::Locked));
    assert_eq!((v.mode.as_str(), v.by.as_str(), v.rate), ("fake", "fake 1", 1.25e6));
    // Its message lands on the CW record, as the worker's claim.
    let rec = wait(5.0, "the message's claim", || {
        e.records()
            .view()
            .records
            .iter()
            .find(|r| r.claims.iter().any(|c| matches!(&c.by, Evidence::Decoder { mode, .. } if mode == "fake")))
            .cloned()
    });
    assert!((rec.center_hz() - CW_HZ).abs() < 2e3, "{rec:?}");
    let c = rec.claims.iter().find(|c| matches!(c.by, Evidence::Decoder { .. })).unwrap();
    assert_eq!(c.label, "Fake (fake 1)");
    assert!(matches!(&c.by, Evidence::Decoder { summary, .. } if summary == "hello (1 message from fake 1)"), "{c:?}");
    assert_eq!(e.worker_views()[0].messages.len(), 1);
    // Stopped: gone from the views, its feed closed.
    assert!(e.stop_worker(id));
    assert!(!e.stop_worker(id));
    assert!(e.worker_views().is_empty());
    // A source going away takes its workers with it.
    let id2 = e.start_worker(src.id(), "fake", 855.25e6).unwrap();
    assert_eq!(e.worker_views().len(), 1);
    e.remove_source(src.id());
    assert!(e.worker_views().iter().all(|v| v.id != id2), "{:?}", e.worker_views());
}
