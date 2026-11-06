//! Step 5 on air: `replay_air <serial> <MHz> <secs> <out dir>`.
//!
//! Tunes an Airspy-protocol radio to `MHz` for `secs` with slices on, and writes every closed record of at least a
//! second (and 10 dB SNR) as a WAV from the ring while the ring holds it. After the run, with the source gone, it
//! writes each of them again from its slice. For each record it prints the length both ways against the record's, and
//! how alike the two are (the peak normalised cross-correlation within ±50 ms). The strongest signals still on the air
//! at the end (open records: an ATIS, say) are written as 6 s regions of the ring, `region-<MHz>.wav`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use api::{PlayId, PlayTarget, PlayTo, RecordId, SignalRecord};
use engine::audio::{AudioConfig, RATE};
use engine::recorder::SliceConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: replay_air <serial> <MHz> <secs> <out dir>");
        std::process::exit(2);
    }
    let (serial, center, secs, out) =
        (&a[1], a[2].parse::<f64>().expect("MHz") * 1e6, a[3].parse::<u64>().expect("secs"), PathBuf::from(&a[4]));
    std::fs::create_dir_all(&out).expect("the out dir");
    let device = backend_airspy::AirspyBackend::new().open(serial).expect("open the radio");
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 20.0, ceiling_bytes: 2 << 30, sweep_secs: 1.0 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: Some(SliceConfig::new(out.join("slices"))),
        audio: AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::default(),
    });
    let src = e.add_source(device, Mode::Live).expect("start the radio");
    src.set_center(center).expect("tune");
    let t0 = Instant::now();
    let mut ring: BTreeMap<RecordId, (std::sync::Arc<SignalRecord>, PlayId)> = BTreeMap::new();
    let mut refused = std::collections::BTreeSet::new();
    let half = 4.5e6;
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(500));
        e.audio_view(); // queues finished WAVs
        for r in e.records().view().records.iter() {
            let keep = !r.open
                && r.tuner_hz == center
                && (r.center_hz() - center).abs() < half
                && r.duration() >= Duration::from_secs(1)
                && r.max_snr_db >= 10.0;
            if keep && !ring.contains_key(&r.id) && !refused.contains(&r.id) {
                let to = PlayTo::Wav(out.join(format!("rec-{}-ring.wav", r.id.0)));
                match e.play(PlayTarget::Record(r.id), None, 1.0, to) {
                    Ok(id) => {
                        println!(
                            "{:7.1} s  record {} at {:.4} MHz, {:.1} s, {:.0} dB: from the ring",
                            t0.elapsed().as_secs_f64(),
                            r.id.0,
                            r.center_hz() / 1e6,
                            r.duration().as_secs_f64(),
                            r.max_snr_db
                        );
                        ring.insert(r.id, (r.clone(), id));
                    }
                    Err(err) => {
                        println!("record {}: {err}", r.id.0);
                        refused.insert(r.id);
                    }
                }
            }
        }
    }
    // The strongest signals still on: 6 s of each from the ring.
    let mut open: Vec<_> = e
        .records()
        .view()
        .records
        .iter()
        .filter(|r| r.open && (r.center_hz() - center).abs() < half && r.max_snr_db >= 15.0 && r.bandwidth_hz() < 25e3)
        .cloned()
        .collect();
    open.sort_by(|a, b| b.max_snr_db.total_cmp(&a.max_snr_db));
    let head = src.head();
    let mut regions = Vec::new();
    for r in open.iter().take(3) {
        let target = PlayTarget::Region {
            source: src.id(),
            center_hz: r.center_hz(),
            bandwidth_hz: r.bandwidth_hz(),
            from_sample: head - 60_000_000,
            to_sample: head,
        };
        let to = PlayTo::Wav(out.join(format!("region-{:.4}.wav", r.center_hz() / 1e6)));
        match e.play(target, None, 1.0, to) {
            Ok(id) => {
                println!(
                    "region at {:.4} MHz ({:.1} kHz, {:.0} dB), 6 s from the ring",
                    r.center_hz() / 1e6,
                    r.bandwidth_hz() / 1e3,
                    r.max_snr_db
                );
                regions.push(id);
            }
            Err(err) => println!("region at {:.4} MHz: {err}", r.center_hz() / 1e6),
        }
    }
    wait_written(&e, regions);
    wait_written(&e, ring.values().map(|v| v.1).collect());
    // The slices are written when each record closes; then, from the slices alone.
    std::thread::sleep(Duration::from_secs(1));
    e.remove_source(src.id());
    drop(src);
    let mut slice = Vec::new();
    for (id, (_, _)) in &ring {
        match e.play(PlayTarget::Record(*id), None, 1.0, PlayTo::Wav(out.join(format!("rec-{}-slice.wav", id.0)))) {
            Ok(p) => slice.push(p),
            Err(err) => println!("record {}: no slice play: {err}", id.0),
        }
    }
    wait_written(&e, slice);
    println!("\n  record      MHz   rec s   ring s  slice s   alike   mode  rms dBFS");
    for (id, (r, _)) in &ring {
        let (a, b) = (read_wav(&out.join(format!("rec-{}-ring.wav", id.0))), read_wav(&out.join(format!("rec-{}-slice.wav", id.0))));
        let (Some(a), Some(b)) = (a, b) else {
            println!("{:8} {:9.4}  a WAV is missing", id.0, r.center_hz() / 1e6);
            continue;
        };
        let rms = (a.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / a.len().max(1) as f64).sqrt();
        println!(
            "{:8} {:9.4} {:7.2} {:8.2} {:8.2} {:7.3} {:>6} {:9.1}",
            id.0,
            r.center_hz() / 1e6,
            r.duration().as_secs_f64(),
            a.len() as f64 / RATE,
            b.len() as f64 / RATE,
            alike(&a, &b),
            engine::replay::default_mode(r.center_hz()).name(),
            20.0 * rms.log10()
        );
    }
    println!("WAVs in {}", out.display());
}

fn wait_written(e: &Engine, ids: Vec<PlayId>) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(30) {
        let v = e.audio_view();
        if ids.iter().all(|id| v.plays.iter().find(|p| p.id == *id).is_none_or(|p| p.state.is_over())) {
            for p in v.plays.iter().filter(|p| ids.contains(&p.id)) {
                if let api::PlayState::EndedEarly(why) = &p.state {
                    println!("play {}: ended early: {why}", p.id.0);
                }
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("some WAVs were not written within 30 s");
}

fn read_wav(p: &Path) -> Option<Vec<f32>> {
    let b = std::fs::read(p).ok()?;
    Some(b.get(44..)?.as_chunks::<2>().0.iter().map(|s| f32::from(i16::from_le_bytes(*s)) / 32767.0).collect())
}

/// Peak normalised cross-correlation of `a` and `b` within ±50 ms (1 = the same audio).
fn alike(a: &[f32], b: &[f32]) -> f64 {
    let max_lag = (0.05 * RATE) as isize;
    let energy = |x: &[f32]| x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
    let norm = (energy(a) * energy(b)).sqrt().max(1e-12);
    (-max_lag..=max_lag)
        .step_by(4)
        .map(|lag| {
            let mut s = 0.0;
            for (i, &x) in a.iter().enumerate() {
                let j = i as isize + lag;
                if j >= 0 && (j as usize) < b.len() {
                    s += f64::from(x) * f64::from(b[j as usize]);
                }
            }
            s / norm
        })
        .fold(f64::MIN, f64::max)
}
