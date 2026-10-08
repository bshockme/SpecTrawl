//! Regression tests on real captures in testdata/captures (git-ignored; made
//! with `cargo run -p engine --example capture_identified`). Each test skips
//! if its capture isn't present.

use std::path::PathBuf;

use num_complex::Complex32;

fn load(name: &str) -> Option<(Vec<Complex32>, f64)> {
    let stem = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/captures").join(name);
    // Names contain dots (769.5782MHz…), so append rather than with_extension.
    let meta = std::fs::read_to_string(format!("{}.sigmf-meta", stem.display())).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta).ok()?;
    let fs = meta["global"]["core:sample_rate"].as_f64()?;
    let raw = std::fs::read(format!("{}.sigmf-data", stem.display())).ok()?;
    let x = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    Some((x, fs))
}

fn check(name: &str, protocol: &str, id: Option<&str>, kind: &str, min_decoded: usize) {
    let Some((x, fs)) = load(name) else {
        eprintln!("skipping: {name} not present");
        return;
    };
    let c = detect::classify(&x, fs, 12_500.0);
    assert_eq!(c.protocol.as_deref(), Some(protocol), "{name}: {}", c.label);
    let f = c.fields.expect("fields");
    assert_eq!(f.id.as_deref(), id, "{name}");
    assert_eq!(f.kinds.first().map(|k| k.0.as_str()), Some(kind), "{name}: {:?}", f.kinds);
    assert!(f.decoded >= min_decoded, "{name}: decoded {}/{}", f.decoded, f.tried);
}

#[test]
fn p25_control_channel() {
    check("769.5782MHz_P25_Phase_1", "P25", Some("NAC 0x2A5"), "TSDU", 60);
}

#[test]
fn p25_traffic_channel() {
    check("771.6443MHz_P25_Phase_1", "P25", Some("NAC 0x2A5"), "TDULC", 40);
}

#[test]
fn dmr_control_channel() {
    check("855.5882MHz_DMR_BS_data", "DMR", Some("CC 9"), "CSBK", 15);
}

#[test]
fn nxdn_control_channel() {
    check("460.5813MHz_NXDN_NXDN48", "NXDN", None, "control (RCCH)", 70);
}

#[test]
fn p25_control_channel_messages() {
    let Some((x, fs)) = load("769.5782MHz_P25_Phase_1") else {
        eprintln!("skipping: capture not present");
        return;
    };
    // TSDUs carry three TSBKs: slice far enough past each sync.
    let frames = detect::frames(&x, fs, 12_500.0, 8, 380);
    let msgs = detect::p25::messages(&frames);
    assert!(msgs.len() >= 100, "{} CRC-valid TSBKs", msgs.len());
    let mut sys = detect::p25::SystemInfo::default();
    let mut grants = Vec::new();
    for m in &msgs {
        grants.extend(sys.apply(m));
    }
    assert_eq!(sys.wacn, Some(0xBEE00));
    assert_eq!(sys.system, Some(0x2A5));
    assert_eq!((sys.rfss, sys.site), (Some(2), Some(21)));
    // The site announces its own control channel: the one we're on.
    assert!((sys.control_hz.unwrap() - 769.58125e6).abs() < 1.0, "{:?}", sys.control_hz);
    assert!(sys.adjacent.len() >= 6);
    // TG 10730 granted on a TDMA channel at 769.19375 MHz, slot 1.
    let (tg, ch, _) = grants.iter().find(|g| g.0 == 10730).expect("grant");
    assert_eq!((*tg, sys.freq(*ch).map(|f| f.round()), sys.slot(*ch)), (10730, Some(769_193_750.0), Some(1)));
}
