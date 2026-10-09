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

#[test]
fn capplus_status_rest_field() {
    let Some((x, fs)) = load("855.5882MHz_DMR_BS_data") else {
        eprintln!("skipping: capture not present");
        return;
    };
    // Idle site: status announcing "rest" 3/2; no voice, so the channel's
    // own repeater number stays unknown.
    let f = detect::classify(&x, fs, 12_500.0).fields.expect("fields");
    assert_eq!((f.capplus_repeater, f.capplus_rest), (None, Some((3, 2))));
}

#[test]
fn p25_traffic_channel_link_control() {
    let Some((x, fs)) = load("771.6443MHz_P25_Phase_1") else {
        eprintln!("skipping: capture not present");
        return;
    };
    // TDULC link control (Golay + Reed-Solomon) after a call: termination,
    // then the site's adjacent-site and RFSS broadcasts.
    let f = detect::classify(&x, fs, 12_500.0).fields.expect("fields");
    let n = |k: &str| f.kinds.iter().find(|(name, _)| name == k).map_or(0, |e| e.1);
    assert!(n("LC adjacent site") >= 10 && n("LC call termination") >= 2, "{:?}", f.kinds);
}

#[test]
fn p25_voice_link_control_matches_grants() {
    let Some((x, fs)) = load("P25_site_770.3244") else {
        eprintln!("skipping: capture not present");
        return;
    };
    // The control channel granted TG 10025 on this channel to radios
    // 2121592 and 2121604 (recorded at the same time); LDU1/TDULC link
    // control on the voice channel names the same call.
    let mut talks = Vec::new();
    for w in x.chunks((1.5 * fs) as usize) {
        if let Some(t) = detect::classify(w, fs, 12_500.0).fields.and_then(|f| f.talk) {
            if t == "TG 10025 ← 2121592" || t == "TG 10025 ← 2121604" {
                return;
            }
            talks.push(t);
        }
    }
    panic!("TG 10025 not heard: {talks:?}");
}

#[test]
fn p25_phase2_talkgroups_after_learning_the_nac() {
    let Some((x, fs)) = load("P25P2_site_771.2067") else {
        eprintln!("skipping: capture not present");
        return;
    };
    // The system (from the Phase 1 control channel) is known; this site's
    // NAC (0x2A0) is learned from unscrambled end-of-PTT messages, after
    // which the scrambled FACCH/SACCH decode and name the calls.
    detect::p25p2::register_system(0xBEE00, 0x2A5);
    let mut talks = std::collections::BTreeSet::new();
    for w in x.chunks((1.5 * fs) as usize) {
        let c = detect::classify(w, fs, 12_500.0);
        if let Some(t) = c.fields.and_then(|f| f.talk) {
            talks.insert(t);
        }
    }
    assert!(talks.iter().any(|t| t.starts_with("TG 47018 ← ")), "{talks:?}");
    assert!(talks.iter().any(|t| t.starts_with("TG 47000 ← 4024312")), "{talks:?}");
}
