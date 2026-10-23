//! The spectral identifier on real spectra in testdata/spectra (mean dB per
//! bin, made with `cargo run -p engine --example trawl`). Each test skips if
//! its spectrum isn't present.

mod common;

use detect::spectral::{Kind, Occupant, Spectrum, floor_estimate, identify};

fn occupants(name: &str) -> Option<Vec<Occupant>> {
    let meta: serde_json::Value = serde_json::from_str(&common::spectrum(name)?).ok()?;
    let raw = common::spectrum_file(name, "spec")?;
    let n = meta["bins"].as_u64()? as usize;
    let span = meta["span_hz"].as_f64()?;
    let bin = span / n as f64;
    let db: Vec<f32> = raw[..4 * n].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    // As the engine does: floor windows a twentieth of the span, 1-8 MHz.
    let floor = floor_estimate(&db, ((span / 20.0).clamp(1e6, 8e6) / bin) as usize);
    let f0 = meta["center_hz"].as_f64()? - span / 2.0;
    Some(identify(&Spectrum { db: &db, floor: &floor, f0_hz: f0, bin_hz: bin }))
}

fn has(o: &[Occupant], kind: Kind, mhz: f64, tol_mhz: f64) -> bool {
    o.iter().any(|x| x.kind == kind && (x.center_hz / 1e6 - mhz).abs() <= tol_mhz)
}

#[test]
fn fm_broadcast_with_hd() {
    let Some(o) = occupants("live_101.000MHz") else { return };
    assert!(has(&o, Kind::FmBroadcast { hd: true }, 102.1, 0.01), "{o:#?}");
    for mhz in [97.5, 103.5, 105.5] {
        assert!(has(&o, Kind::FmBroadcast { hd: false }, mhz, 0.01), "{mhz}: {o:#?}");
    }
}

#[test]
fn atsc_tv_by_pilot() {
    let Some(o) = occupants("sweep_465-615MHz") else { return };
    for ch in [18, 24, 34, 36] {
        assert!(o.iter().any(|x| x.kind == Kind::Tv { channel: ch, atsc1: true }), "ch {ch}: {o:#?}");
    }
    let Some(o) = occupants("sweep_170-220MHz") else { return };
    for ch in [7, 10] {
        assert!(o.iter().any(|x| x.kind == Kind::Tv { channel: ch, atsc1: true }), "ch {ch}: {o:#?}");
    }
}

#[test]
fn lte_downlinks() {
    let Some(o) = occupants("sweep_610-900MHz") else { return };
    let cells: Vec<f64> = o.iter().filter(|x| matches!(x.kind, Kind::Cellular { .. })).map(|x| x.center_hz / 1e6).collect();
    for mhz in [632.0, 731.5, 739.0, 751.0, 763.0, 879.0] {
        assert!(cells.iter().any(|c| (c - mhz).abs() < 1.5), "{mhz}: {cells:?}");
    }
    // Nothing in the uplink or public-safety allocations.
    assert!(cells.iter().all(|c| !(663.0..716.0).contains(c) && !(769.0..806.0).contains(c)), "{cells:?}");
}
