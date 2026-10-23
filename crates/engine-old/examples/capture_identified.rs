//! Runs an Airspy on one band, then saves the look-back channel IQ of every
//! emitter the classifier identified, as SigMF test material.
//!
//!     cargo run --release -p engine-old --example capture_identified -- <serial> <MHz> <seconds> <out-dir>

use std::path::PathBuf;
use std::time::Duration;

use engine_old::sigmf::{ExportKind, ExportRequest};
use engine_old::{Mode, Source, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, mhz, secs, out) = (&a[1], a[2].parse::<f64>().unwrap(), a[3].parse::<u64>().unwrap(), PathBuf::from(&a[4]));
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    d.set_center(mhz * 1e6).unwrap();
    let src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_secs(secs));
    let (s, e) = src.ring_range();
    for em in src.emitters().into_iter().filter(|e| e.has_protocol()) {
        let label = em.label().unwrap_or_default();
        let stem = engine_old::sigmf::sanitize(&format!("{:.4}MHz_{}", em.center_hz / 1e6, label));
        let r = src
            .export(ExportRequest {
                start_sample: s,
                end_sample: e,
                kind: ExportKind::Channel { center_hz: em.center_hz, bandwidth_hz: em.bandwidth_hz.max(12.5e3) },
                dir: out.clone(),
                stem: stem.clone(),
                description: format!("{label} via SpecTrawl capture_identified"),
                annotations: vec![],
            })
            .join()
            .unwrap();
        match r {
            Ok(r) => println!("{stem}: {:.1} s, {} samples, {} bursts, tally {:?}", r.seconds, r.samples, em.records, em.tally),
            Err(e) => println!("{stem}: {e}"),
        }
    }
}
