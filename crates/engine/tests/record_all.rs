//! Record-all mode end to end: simulator → detector → one SigMF pair per
//! signal in a temp folder.

use std::time::{Duration, Instant};

use engine::{Mode, RecordAllConfig, Source, SourceConfig};
use sdr_core::Backend;

#[test]
fn records_each_signal_as_a_sigmf_pair() {
    let b = backend_sim::SimBackend;
    let dev = b.open(&b.enumerate().unwrap()[0].serial).unwrap();
    let src = Source::start(dev, SourceConfig::default(), Mode::Live).unwrap();
    let dir = std::env::temp_dir().join(format!("spectrawl-recall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // A short cap so the simulator's continuous control channel yields a
    // finished file within the test.
    src.set_record_all(Some(RecordAllConfig { max_duration_s: 2.0, dir: dir.clone(), ..Default::default() }));
    let t0 = Instant::now();
    let mut status = src.record_all_status();
    while status.files == 0 && t0.elapsed() < Duration::from_secs(12) {
        std::thread::sleep(Duration::from_millis(200));
        status = src.record_all_status();
    }
    assert!(status.files >= 1, "no capture finished in 12 s: {status:?}");
    assert!(status.bytes > 0);
    let data_path = status.last_path.clone().expect("last path");
    let meta_path = data_path.with_extension("sigmf-meta");
    assert!(data_path.exists() && meta_path.exists(), "{data_path:?}");
    let name = data_path.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.contains("MHz_") && name.ends_with(".sigmf-data"), "{name}");
    let data_len = std::fs::metadata(&data_path).unwrap().len();
    assert!(data_len > 0 && data_len % 8 == 0, "{data_len}");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["global"]["core:datatype"], "cf32_le");
    let snap = src.snapshot();
    let f = meta["captures"][0]["core:frequency"].as_f64().unwrap();
    assert!((f - snap.center_hz).abs() <= snap.sample_rate / 2.0, "{f} outside the span around {}", snap.center_hz);
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let bw = meta["global"]["spectrawl:bandwidth_hz"].as_f64().unwrap();
    assert!(fs >= 2.0 * bw && fs >= 48e3, "fs {fs} for bw {bw}");
    let seconds = data_len as f64 / 8.0 / fs;
    assert!(seconds >= 0.3, "{seconds} s of capture");
    // Switching off closes any open captures and leaves no data file without its meta.
    src.set_record_all(None);
    std::thread::sleep(Duration::from_millis(600));
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "sigmf-data") {
            assert!(p.with_extension("sigmf-meta").exists(), "{} has no meta", p.display());
        }
    }
    drop(src);
    let _ = std::fs::remove_dir_all(&dir);
}
