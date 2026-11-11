//! Checks a dongle without the engine: lists what librtlsdr sees, opens one,
//! streams for a while and prints the delivered rate, power and events.
//!
//! `cargo run --release -p backend-rtlsdr --example rtl_probe -- [id] [MHz] [secs]`

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use backend_rtlsdr::RtlSdrBackend;
use sdr_core::{Backend, Samples, StreamEvent, StreamSink};

#[derive(Default)]
struct Tally {
    samples: u64,
    power: f64,
    events: Vec<StreamEvent>,
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Tally>>);

impl StreamSink for Sink {
    fn on_samples(&mut self, _first: u64, samples: Samples<'_>) {
        let mut t = self.0.lock().unwrap();
        if let Samples::Cf32(s) = samples {
            t.power += s.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>();
        }
        t.samples += samples.len() as u64;
    }

    fn on_event(&mut self, event: StreamEvent) {
        if !matches!(event, StreamEvent::TimeAnchor { .. }) {
            self.0.lock().unwrap().events.push(event);
        }
    }
}

fn main() {
    env_logger_lite();
    let a: Vec<String> = std::env::args().collect();
    let b = RtlSdrBackend::new();
    let devices = match b.enumerate() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    for d in &devices {
        println!("found {:<20} {}", d.serial, d.label);
    }
    let Some(id) = a.get(1).cloned().or_else(|| devices.first().map(|d| d.serial.clone())) else {
        eprintln!("no RTL-SDR attached");
        std::process::exit(1);
    };
    let mhz: f64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(100.0);
    let secs: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let mut dev = b.open(&id).expect("open the dongle");
    println!("opened {} ({}), {:.3} MS/s, {:?}", id, dev.info().label, dev.sample_rate() / 1e6, dev.capabilities().freq_ranges_hz);
    dev.set_center(mhz * 1e6).expect("tune");
    let sink = Sink::default();
    let t0 = Instant::now();
    dev.start(Box::new(sink.clone())).expect("start");
    std::thread::sleep(Duration::from_secs_f64(secs));
    dev.stop().expect("stop");
    let dt = t0.elapsed().as_secs_f64();
    let t = sink.0.lock().unwrap();
    let dbfs = 10.0 * (t.power / t.samples.max(1) as f64).max(1e-20).log10();
    println!(
        "{} samples in {dt:.2} s = {:.3} MS/s (set {:.3}); mean power {dbfs:.1} dBFS",
        t.samples,
        t.samples as f64 / dt / 1e6,
        dev.sample_rate() / 1e6
    );
    for e in &t.events {
        println!("  {e:?}");
    }
}

/// Prints the backend's log lines without pulling in a logger crate.
fn env_logger_lite() {
    struct L;
    impl log::Log for L {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info
        }
        fn log(&self, r: &log::Record) {
            eprintln!("[{}] {}", r.level(), r.args());
        }
        fn flush(&self) {}
    }
    let _ = log::set_logger(&L);
    log::set_max_level(log::LevelFilter::Info);
}
