use detect::{
    Ddc, chirp,
    demod::{Demod, Mode},
    pocsag, sync,
};
use engine::{BurstDetector, DetectorConfig, SpectrumRow, Tuning};
use sdr_core::Complex32;
use std::time::{Duration, SystemTime};

#[test]
fn pocsag_rejects_ambiguous_three_bit_error() {
    assert!(pocsag::correct(0x26).is_none(), "three-bit corruption of the valid zero word accepted as {:?}", pocsag::correct(0x26));
}

#[test]
fn lora_sf12_at_125k_is_recognized() {
    let bw = 125_000.0;
    let fs = 4.0 * bw;
    let sf = 12;
    let n = 4 * (1 << sf);
    let mut ph = 0f64;
    let x: Vec<_> = (0..12 * n)
        .map(|i| {
            let f = -bw / 2.0 + bw * (i % n) as f64 / n as f64;
            ph = (ph + std::f64::consts::TAU * f / fs).rem_euclid(std::f64::consts::TAU);
            Complex32::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect();
    assert_eq!(chirp::lora(&x, fs), Some((bw, sf)), "{:?}", chirp::analyse(&x, fs));
}

#[test]
fn am_squelch_closes_on_channel_filtered_noise() {
    let fs = 10e6;
    let (cutoff, target) = Mode::Am.channel();
    let mut ddc = Ddc::new(fs, 0.0, cutoff, target);
    let mut demod = Demod::new(Mode::Am, ddc.fs_out);
    let mut seed = 315u64;
    let mut uniform = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 11) as f64 + 1.0) / 9007199254740993.0
    };
    let mut input = Vec::new();
    let mut chan = Vec::new();
    let mut audio = Vec::new();
    for _ in 0..100 {
        input.clear();
        for _ in 0..60_000 {
            let mag = (-2.0 * uniform().ln()).sqrt();
            let ph = std::f64::consts::TAU * uniform();
            input.push(Complex32::new((mag * ph.cos()) as f32, (mag * ph.sin()) as f32));
        }
        chan.clear();
        audio.clear();
        ddc.process(&input, &mut chan);
        demod.process(&chan, &mut audio);
    }
    assert!(
        !demod.is_open(),
        "AM noise-only channel left squelch open; audio RMS {}",
        (audio.iter().map(|v| v * v).sum::<f32>() / audio.len() as f32).sqrt()
    );
}

fn row(seq: u64, ranges: &[(usize, usize)]) -> SpectrumRow {
    let mut db = vec![-100.0; 4096];
    for &(a, b) in ranges {
        db[a..b].fill(-80.0);
    }
    SpectrumRow {
        seq,
        tuning: Tuning { center_hz: 100e6, span_hz: 1e6 },
        end_sample: seq * 20000,
        time: SystemTime::UNIX_EPOCH + Duration::from_millis(seq * 20),
        db,
    }
}

#[test]
fn merging_tracks_preserves_mean_snr() {
    let mut det = BurstDetector::new(DetectorConfig::live(50.0), "review".into());
    for seq in 1..=2 {
        det.process(&row(seq, &[(100, 110), (130, 140)]));
    }
    det.process(&row(3, &[(100, 140)]));
    let records = det.open_records();
    assert_eq!(records.len(), 1);
    assert!((records[0].mean_snr_db - 20.0).abs() < 0.01, "mean {}, max {}", records[0].mean_snr_db, records[0].max_snr_db);
}

#[test]
fn impossible_dmr_sync_cadence_is_not_confirmed() {
    let entry = sync::table().into_iter().find(|e| e.protocol == "DMR" && e.detail == "BS voice").unwrap();
    let hits: Vec<_> = [100, 201].into_iter().map(|symbol| sync::SyncHit { entry, inverted: false, phase: 5, symbol, errors: 0 }).collect();
    assert!(sync::best(&hits).is_none(), "off-grid syncs accepted: {:?}", sync::best(&hits));
}

#[test]
fn wideband_signal_does_not_become_its_own_noise_floor() {
    let mut det = BurstDetector::new(DetectorConfig::live(50.0), "review".into());
    for seq in 1..=2 {
        det.process(&row(seq, &[(0, 4096)]));
    }
    assert!(!det.open_records().is_empty(), "flat occupied span never detected");
}
