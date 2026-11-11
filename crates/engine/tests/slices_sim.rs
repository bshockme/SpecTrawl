//! The slice tier on the simulator: a record's slice holds its signal from where it should, and the directory stays
//! under its cap (step 4 review focus 7 and 8).

use std::path::{Path, PathBuf};
use std::time::Duration;

use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use dsp::spectrum::SharedFft;
use engine::recorder::SliceConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Complex32;

/// A keyed carrier 312.5 kHz above the tuner (855.25 MHz): half a second on in every second.
const CW_HZ: f64 = 855.5625e6;

fn device() -> Box<dyn sdr_core::Device> {
    let emitters = vec![Emitter {
        freq_hz: CW_HZ,
        level_dbfs: -40.0,
        modulation: Modulation::Cw,
        keying: Keying::Bursts { period_s: 1.0, duty: 0.5, phase: 0.3 },
    }];
    let (d, _) = SimDevice::with_config(SimConfig { emitters, paced: true, report_lag_gaps: false, ..Default::default() });
    Box::new(d)
}

fn engine(slices: SliceConfig) -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 512 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: Some(slices),
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::off(),
    })
}

struct Dir(PathBuf);
impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("spectrawl-slices-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Dir(d)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir_bytes(d: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        let p = e.path();
        total += if p.is_dir() { dir_bytes(&p) } else { e.metadata().map_or(0, |m| m.len()) };
    }
    total
}

fn read_slice(stem: &Path) -> Vec<Complex32> {
    let format = engine::sigmf::Format::of_meta(&stem.with_extension("sigmf-meta")).expect("the slice's metadata");
    let raw = std::fs::read(stem.with_extension("sigmf-data")).expect("the slice's data");
    let mut x = Vec::new();
    format.decode(&raw, &mut x);
    x
}

#[test]
fn a_records_slice_holds_its_signal_from_where_it_should() {
    let dir = Dir::new("signal");
    let e = engine(SliceConfig::new(&dir.0));
    e.add_source(device(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let view = e.records().view();
    let sliced: Vec<_> = view.records.iter().filter(|r| !r.open && r.iq_clip.is_some() && r.first_sample > 2_000_000).collect();
    assert!(!sliced.is_empty(), "no closed record with a slice: {:?}", view.records);
    let (pre_roll, post_roll) = (1_500_000, 1_500_000); // 150 ms each at 10 MS/s
    for r in sliced {
        let clip = r.iq_clip.as_ref().unwrap();
        // Where it starts: within one block (L = 32 768: the block holding the start) plus the channel filter's delay
        // (N/4 = 16 384) of the record's start minus the pre-roll.
        let want = r.first_sample - pre_roll;
        assert!(
            clip.from_sample.abs_diff(want) <= 32_768 + 16_384,
            "slice from {} for a record from {} (want {want})",
            clip.from_sample,
            r.first_sample
        );
        // How long: the record and its pre- and post-roll, give or take a few blocks.
        let secs = clip.samples as f64 / clip.rate;
        let rec_secs = (r.end_sample - r.first_sample) as f64 / 10e6;
        let rolls = (pre_roll + post_roll) as f64 / 10e6;
        assert!((secs - (rec_secs + rolls)).abs() < 0.08, "{secs:.3} s slice for a {rec_secs:.3} s record");
        // Where it is centred: on the record as it closed, not as its first rows had it.
        assert!((clip.center_hz - r.center_hz()).abs() < 1.0, "slice at {} Hz, record at {} Hz", clip.center_hz, r.center_hz());
        // What it holds: the carrier, at the slice's centre (the record's), within one channel bin.
        let x = read_slice(&clip.path);
        assert_eq!(x.len() as u64, clip.samples);
        let n = 4096.min(x.len().next_power_of_two() / 2);
        let mid = x.len() / 2 - n / 2;
        let mut bins = vec![Complex32::default(); n];
        SharedFft::new(n).forward(&x[mid..mid + n], &mut bins);
        let peak = bins.iter().enumerate().max_by(|a, b| a.1.norm_sqr().total_cmp(&b.1.norm_sqr())).unwrap().0;
        let peak_hz = if peak > n / 2 { peak as f64 - n as f64 } else { peak as f64 } * clip.rate / n as f64;
        let offset = CW_HZ - clip.center_hz;
        assert!((peak_hz - offset).abs() <= 2.0 * clip.rate / n as f64, "peak at {peak_hz:.1} Hz, carrier at {offset:.1} Hz");
        // Its metadata is SigMF the rest of the tooling reads.
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(clip.path.with_extension("sigmf-meta")).unwrap()).unwrap();
        assert_eq!(meta["global"]["core:datatype"], "cf32_le");
        // Stored as floats, not rounded to i16 counts: the noise between samples is finer than one count.
        let on_grid = x.iter().filter(|c| ((c.re * 32767.0) - (c.re * 32767.0).round()).abs() < 1e-3).count();
        assert!(on_grid * 10 < x.len(), "{on_grid} of {} samples on the i16 grid", x.len());
        assert_eq!(meta["global"]["core:sample_rate"].as_f64().unwrap(), clip.rate);
        assert_eq!(meta["captures"][0]["core:frequency"].as_f64().unwrap(), clip.center_hz);
    }
}

#[test]
fn the_slice_directory_stays_under_its_cap_oldest_first() {
    let dir = Dir::new("cap");
    let cap = 1_200_000; // about two slices
    let e = engine(SliceConfig { cap_bytes: cap, ..SliceConfig::new(&dir.0) });
    e.add_source(device(), Mode::Live).unwrap();
    let mut max_seen = 0;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(200));
        max_seen = max_seen.max(dir_bytes(&dir.0));
    }
    let view = e.records().view();
    let closed: Vec<_> = view.records.iter().filter(|r| !r.open).collect();
    let evicted: Vec<u64> = closed.iter().filter(|r| r.slice_evicted).map(|r| r.id.0).collect();
    let kept: Vec<u64> = closed.iter().filter(|r| r.iq_clip.is_some()).map(|r| r.id.0).collect();
    assert!(!evicted.is_empty(), "nothing evicted under a {cap} B cap; {} B on disk", dir_bytes(&dir.0));
    assert!(!kept.is_empty());
    assert!(evicted.iter().max() < kept.iter().min(), "evicted {evicted:?}, kept {kept:?}");
    for r in closed.iter().filter(|r| r.slice_evicted) {
        assert!(r.iq_clip.is_none(), "{r:?}");
    }
    // Never more than the cap plus the slice being written and one just finished.
    let slice = 640_000; // ~0.8 s (the burst and its rolls) at 78 kS/s, 8 bytes a sample, with room
    assert!(max_seen <= cap + 2 * slice, "{max_seen} B on disk under a {cap} B cap");
}

#[test]
fn a_saved_slice_outlives_the_cap_and_one_saved_while_open_is_kept_when_written() {
    let dir = Dir::new("saved");
    let saved = dir.0.join("saved");
    let cap = 1_200_000; // about two slices: within a few seconds every slice here is evicted
    let e = engine(SliceConfig { cap_bytes: cap, saved_dir: saved.clone(), ..SliceConfig::new(dir.0.join("slices")) });
    e.add_source(device(), Mode::Live).unwrap();
    // One closed record with its slice written, and one still open.
    let (mut closed, mut open) = (None, None);
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(100));
        let view = e.records().view();
        closed = closed.or_else(|| view.records.iter().find(|r| !r.open && r.iq_clip.is_some()).cloned());
        if closed.is_some() {
            open = view.records.iter().find(|r| r.open && r.rows > 3).cloned();
        }
        if open.is_some() {
            break;
        }
    }
    let (closed, open) = (closed.expect("a closed record with a slice"), open.expect("an open record"));
    let (path, pending) = e.records().save_iq(closed.id).unwrap();
    assert!(!pending && path == saved.join(format!("record-{}", closed.id.0)));
    let (open_path, pending) = e.records().save_iq(open.id).unwrap();
    assert!(pending, "an open record's slice is not written yet");
    // Long enough for both originals to be evicted.
    std::thread::sleep(Duration::from_secs(5));
    let view = e.records().view();
    let now = |id| view.records.iter().find(|r| r.id == id).expect("still in the book");
    for (id, stem) in [(closed.id, &path), (open.id, &open_path)] {
        let r = now(id);
        assert!(r.slice_evicted, "record {} kept its slice under a {cap} B cap", id.0);
        assert!(r.tags.iter().any(|t| t == "saved"), "{:?}", r.tags);
        let x = read_slice(stem);
        assert!(!x.is_empty(), "record {}'s saved slice is empty", id.0);
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(stem.with_extension("sigmf-meta")).unwrap()).unwrap();
        assert!(meta["global"]["core:description"].as_str().unwrap().starts_with(&format!("record {} slice", id.0)));
    }
    // The closed one's copy is the whole slice it had.
    assert_eq!(read_slice(&path).len() as u64, closed.iq_clip.as_ref().unwrap().samples);
    // An evicted record can't be saved, and says why.
    let gone = view.records.iter().find(|r| r.slice_evicted && !r.tags.iter().any(|t| t == "saved")).expect("an evicted record");
    let err = e.records().save_iq(gone.id).unwrap_err();
    assert!(err.contains("evicted"), "{err}");
}
