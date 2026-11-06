//! The detector bank on the committed real captures, in the 1.5 s windows the pool cuts (step 6 review focus 2): a P25
//! control channel is named by its sync bank and its decoded site; a DMR channel by its syncs; an analog FM repeater
//! by its squelch code and shape, never by a protocol; noise and weak windows get nothing they don't show.

use api::{Evidence, Identity};
use detect::Hints;
use detect::bank::Window;
use engine::classify::bank::{bank, look};
use sdr_core::Complex32;

/// A cf32 SigMF capture in `testdata/small` and its rate. A missing capture fails the test (spec §6 step 0).
fn capture(name: &str) -> (Vec<Complex32>, f64) {
    let stem = format!("{}/../../testdata/small/{name}", env!("CARGO_MANIFEST_DIR"));
    let meta = std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap_or_else(|e| panic!("capture {name} missing: {e}"));
    let meta: serde_json::Value = serde_json::from_str(&meta).expect("sigmf-meta");
    let fs = meta["global"]["core:sample_rate"].as_f64().expect("core:sample_rate");
    let raw = std::fs::read(format!("{stem}.sigmf-data")).expect("sigmf-data");
    (
        raw.as_chunks::<8>()
            .0
            .iter()
            .map(|c| Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]])))
            .collect(),
        fs,
    )
}

/// Every 1.5 s window's claims.
fn windows(x: &[Complex32], fs: f64, bandwidth_hz: f64, snr_db: f32) -> Vec<Vec<api::Claim>> {
    let b = bank();
    let n = (1.5 * fs) as usize;
    x.chunks(n).filter(|w| w.len() == n).map(|w| look(&b, &Window::new(w, fs, bandwidth_hz, Hints { snr_db: Some(snr_db) }))).collect()
}

fn p25(c: &api::Claim) -> Option<&Identity> {
    match &c.by {
        Evidence::Decoder { identity: Some(i @ Identity::P25 { .. }), .. } => Some(i),
        _ => None,
    }
}

#[test]
fn a_p25_control_channel_is_named_by_its_syncs_and_its_site() {
    let (x, fs) = capture("859.7677MHz_P25_Phase_1");
    let all = windows(&x, fs, 12_500.0, 25.0);
    assert!(all.len() >= 5, "{} windows", all.len());
    let mut full = 0;
    for (k, claims) in all.iter().enumerate() {
        let sync = claims.iter().find(|c| matches!(c.by, Evidence::SyncBank { .. })).unwrap_or_else(|| panic!("window {k}: {claims:?}"));
        assert_eq!(sync.label, "P25 Phase 1");
        let id = claims.iter().find_map(p25).unwrap_or_else(|| panic!("window {k}: no P25 identity: {claims:?}"));
        let Identity::P25 { nac, wacn, system, rfss, site } = *id else { unreachable!() };
        assert_eq!(nac, 0x34D, "window {k}");
        // What the broadcasts name, when one came round in the window, is the site the capture is from.
        assert!(wacn.is_none_or(|w| w == 0xBEE00) && system.is_none_or(|s| s == 0x348), "window {k}: {id:?}");
        assert!(rfss.is_none_or(|r| r == 1) && site.is_none_or(|s| s == 13), "window {k}: {id:?}");
        full += usize::from(wacn.is_some() && site.is_some());
        let headline = api::headline(claims).unwrap();
        assert!(matches!(headline.by, Evidence::Decoder { .. }), "the decoded claim heads: {headline:?}");
        assert_eq!(headline.label, "P25 Phase 1 control");
        // The shape rules read C4FM as "Analog FM"; with a decode in hand they don't speak.
        assert!(!claims.iter().any(|c| matches!(c.by, Evidence::Features { .. })), "window {k}: {claims:?}");
    }
    assert!(full >= 1, "no window named the whole site");
}

#[test]
fn a_dmr_channel_is_named_by_its_syncs() {
    let (x, fs) = capture("463.6878MHz_DMR_BS_data");
    let all = windows(&x, fs, 12_500.0, 15.0);
    let named = all
        .iter()
        .filter(|c| c.iter().any(|c| matches!(&c.by, Evidence::SyncBank { protocol, .. } if protocol.starts_with("DMR"))))
        .count();
    assert!(named * 2 > all.len(), "{named} of {} windows say DMR: {all:?}", all.len());
    assert!(all.iter().flatten().all(|c| !matches!(&c.by, Evidence::SyncBank { protocol, .. } if !protocol.starts_with("DMR"))), "{all:?}");
}

#[test]
fn an_analog_fm_repeater_is_never_a_protocol_and_a_weak_window_is_not_described() {
    let (x, fs) = capture("855.6375MHz_Analog_FM_repeater");
    let all = windows(&x, fs, 9_000.0, 25.0);
    assert!(!all.is_empty());
    for claims in &all {
        assert!(!claims.iter().any(|c| matches!(c.by, Evidence::SyncBank { .. })), "{claims:?}");
        for c in claims {
            match &c.by {
                // Its DCS code, decoded: analog FM with an identity.
                Evidence::Decoder { mode, identity: Some(Identity::Other { .. }), .. } => assert_eq!(mode, "FM", "{c:?}"),
                Evidence::Features { .. } => assert!(c.confidence <= 0.6 && !c.label.contains("FSK"), "{c:?}"),
                _ => panic!("{c:?}"),
            }
        }
    }
    assert!(all.iter().flatten().any(|c| matches!(c.by, Evidence::Decoder { .. })), "its DCS code was never read: {all:?}");
    // Five dB over the floor is too weak to describe by shape: no Features claim.
    for claims in windows(&x, fs, 9_000.0, 5.0) {
        assert!(!claims.iter().any(|c| matches!(c.by, Evidence::Features { .. })), "{claims:?}");
    }
}

#[test]
fn noise_gets_no_claim() {
    let fs = 78_125.0;
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut g = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.1
    };
    let x: Vec<Complex32> = (0..(4.5 * fs) as usize).map(|_| Complex32::new(g(), g())).collect();
    for bw in [9e3, 12.5e3, 25e3] {
        for claims in windows(&x, fs, bw, 12.0) {
            assert!(claims.is_empty(), "{bw} Hz: {claims:?}");
        }
    }
}
