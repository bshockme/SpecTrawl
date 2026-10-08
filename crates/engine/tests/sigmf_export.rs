//! End to end: simulator → engine → SigMF files.

use std::time::Duration;

use engine::sigmf::{ExportKind, ExportRequest};
use engine::{Mode, Source, SourceConfig};
use sdr_core::Backend;

#[test]
fn exports_channel_and_wideband() {
    let b = backend_sim::SimBackend;
    let dev = b.open(&b.enumerate().unwrap()[0].serial).unwrap();
    let src = Source::start(dev, SourceConfig::default(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let dir = std::env::temp_dir().join(format!("spectrawl-test-{}", std::process::id()));
    let (_, head) = src.ring_range();
    let rate = 10e6;
    let start = head - (0.5 * rate) as u64;

    // The P25 control channel is on continuously.
    let ch = src
        .export(ExportRequest {
            start_sample: start,
            end_sample: head,
            kind: ExportKind::Channel { center_hz: 851.0125e6, bandwidth_hz: 12.5e3 },
            dir: dir.clone(),
            stem: "ch".into(),
            description: "test".into(),
            annotations: vec![],
        })
        .join()
        .unwrap()
        .expect("channel export");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&ch.meta_path).unwrap()).unwrap();
    assert_eq!(meta["global"]["core:datatype"], "cf32_le");
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    assert!((48e3..60e3).contains(&fs), "{fs}");
    assert_eq!(meta["captures"][0]["core:frequency"], 851.0125e6);
    let data = std::fs::read(&ch.data_path).unwrap();
    assert_eq!(data.len() as u64, ch.samples * 8);
    // Constant-envelope C4FM at -38 dBFS: mean |x|^2 ≈ 1.6e-4, far above noise.
    let p: f64 = data.chunks(8).skip(1000).map(|c| {
        let re = f32::from_le_bytes(c[0..4].try_into().unwrap()) as f64;
        let im = f32::from_le_bytes(c[4..8].try_into().unwrap()) as f64;
        re * re + im * im
    }).sum::<f64>() / (ch.samples - 1000) as f64;
    let db = 10.0 * p.log10();
    assert!((-42.0..-34.0).contains(&db), "channel power {db:.1} dBFS");

    let wide = src
        .export(ExportRequest {
            start_sample: start,
            end_sample: start + 1_000_000,
            kind: ExportKind::Wideband,
            dir: dir.clone(),
            stem: "wide".into(),
            description: "test".into(),
            annotations: vec![],
        })
        .join()
        .unwrap()
        .expect("wideband export");
    assert_eq!(std::fs::metadata(&wide.data_path).unwrap().len(), 4_000_000);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&wide.meta_path).unwrap()).unwrap();
    assert_eq!(meta["global"]["core:datatype"], "ci16_le");
    assert_eq!(meta["global"]["core:sample_rate"], 10e6);
    let _ = std::fs::remove_dir_all(&dir);
}
