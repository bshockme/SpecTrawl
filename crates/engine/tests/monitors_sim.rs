//! Land-mobile monitors on the simulator (step 7, `engine::decoders::monitor`): the committed real DMR base station and
//! FLEX pager slices, played at their own frequencies, get a monitor once the bank's look decodes them. The monitor
//! locks, names the channel (DMR's colour code) or decodes its pages (FLEX), and its claim headlines the emitter.

use std::time::{Duration, Instant};

use api::{Evidence, Lamp};
use backend_sim::{Clip, Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Complex32;

/// A committed capture as a clip centred on `on_hz`, and the frequency it was tuned to.
fn clip(name: &str, on_hz: f64, half_width_hz: f64) -> &'static Clip {
    let stem = format!("{}/../../testdata/small/{name}", env!("CARGO_MANIFEST_DIR"));
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).expect("the capture")).unwrap();
    let (fs, tuned) = (meta["global"]["core:sample_rate"].as_f64().unwrap(), meta["captures"][0]["core:frequency"].as_f64().unwrap());
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let w = std::f64::consts::TAU * (tuned - on_hz) / fs;
    let x: Vec<Complex32> = raw
        .as_chunks::<8>()
        .0
        .iter()
        .enumerate()
        .map(|(k, c)| {
            Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]]))
                * Complex32::from_polar(1.0, (w * k as f64) as f32)
        })
        .collect();
    Clip::leak(&x, fs, half_width_hz)
}

fn engine(clip: &'static Clip, on_hz: f64, tuner_hz: f64) -> Engine {
    let emitters = vec![Emitter { freq_hz: on_hz, level_dbfs: -45.0, modulation: Modulation::Iq { clip }, keying: Keying::Continuous }];
    let (d, _) =
        SimDevice::with_config(SimConfig { emitters, center_hz: tuner_hz, paced: true, report_lag_gaps: false, ..Default::default() });
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: Default::default(),
    });
    e.add_source(Box::new(d), Mode::Live).unwrap();
    e
}

/// Waits up to `secs` for a locked decoder of `mode` whose summary satisfies `done`.
fn locked(e: &Engine, mode: &str, secs: u64, done: impl Fn(&str) -> bool) -> api::DecoderView {
    let t0 = Instant::now();
    loop {
        if let Some(d) = e.decoder_views().into_iter().find(|d| d.mode == mode && d.lamp == Lamp::Locked && done(&d.summary)) {
            return d;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(secs),
            "no locked {mode} monitor: {:?}\n{:?}",
            e.decoder_views(),
            e.records().view().records
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_dmr_base_station_gets_a_monitor_that_names_its_colour_code() {
    let on = 463.6875e6;
    let e = engine(clip("463.6878MHz_DMR_BS_data", on, 7_000.0), on, 463.0e6);
    let d = locked(&e, "DMR", 25, |s| s.contains("CC 9"));
    assert!(d.blocks_ok >= 4, "{d:?}");
    assert!(d.summary.starts_with("DMR "), "{}", d.summary);
    // Its claim headlines the emitter.
    let t0 = Instant::now();
    loop {
        let view = e.records().view();
        let em = view.emitters.iter().find(|em| em.id == d.emitter).cloned();
        let h = em.as_ref().and_then(|em| em.headline.clone());
        if let Some(h) = h.filter(|h| matches!(&h.by, Evidence::Decoder { mode, .. } if mode == "DMR") && h.confidence > 0.99) {
            assert!(matches!(&h.by, Evidence::Decoder { identity: Some(api::Identity::Other { id }), .. } if id == "CC 9"), "{h:?}");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "{em:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_flex_pager_gets_a_monitor_that_decodes_its_pages_once_each() {
    let on = 929.6125e6;
    let e = engine(clip("flex_929.6127_10s", on, 9_000.0), on, 929.0e6);
    // The 10 s slice holds five pages; looped, they repeat, and are kept once.
    let d = locked(&e, "FLEX", 40, |s| s.contains(": 5 pages"));
    assert!(d.summary.contains("006402344") || d.summary.contains(" | "), "{}", d.summary);
    std::thread::sleep(Duration::from_secs(12));
    let d = e.decoder_views().into_iter().find(|d| d.mode == "FLEX").unwrap();
    assert!(d.summary.contains(": 5 pages"), "a loop's repeats are not new pages: {}", d.summary);
}
