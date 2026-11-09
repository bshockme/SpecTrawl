//! Land-mobile monitors on the simulator (step 7, `engine::decoders::monitor`): the committed real DMR base station
//! slice and the synthetic FLEX one, played at their own frequencies, get a monitor once the bank's look decodes them. The monitor
//! locks, names the channel (DMR's colour code) or decodes its pages (FLEX), and its claim headlines the emitter. A
//! channel sending FLEX and POCSAG by turns (931.7125 MHz does on air) gets a monitor for each, and each one's claim
//! lands only on the records of its own protocol.

use std::time::{Duration, Instant};

use api::{Evidence, Lamp};
use backend_sim::{Clip, Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};
use sdr_core::Complex32;

/// A committed capture as a clip centred on `on_hz`, and the frequency it was tuned to.
fn clip(name: &str, on_hz: f64, half_width_hz: f64) -> &'static Clip {
    let (x, fs) = capture(name, on_hz);
    Clip::leak(&x, fs, half_width_hz)
}

/// A committed capture's samples, shifted so its signal sits at `on_hz`, and their rate.
fn capture(name: &str, on_hz: f64) -> (Vec<Complex32>, f64) {
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
    (x, fs)
}

/// A POCSAG transmission of three alpha pages at 1200 bit/s, ±4.5 kHz (a 1 bit low), at `fs` and amplitude `a`: the
/// preamble, then batches of a sync and 16 codewords. Made here, as no real POCSAG slice may be committed (pager text).
fn pocsag(fs: f64, a: f32) -> Vec<Complex32> {
    const IDLE: u32 = 0x7A89_C197;
    let encode = detect::pocsag::encode;
    let mut cws = Vec::new();
    for k in 0..3u32 {
        let address = 300_000 + 7_919 * k;
        while cws.len() % 16 != (address & 7) as usize * 2 {
            cws.push(IDLE);
        }
        cws.push(encode((address >> 3) << 2 | 3));
        let bits: Vec<u32> = format!("MONITOR TEST {k}").bytes().flat_map(|c| (0..7).map(move |i| (c >> i & 1) as u32)).collect();
        cws.extend(bits.chunks(20).map(|c| encode(1 << 20 | c.iter().chain(std::iter::repeat(&0)).take(20).fold(0, |a, b| a << 1 | b))));
    }
    cws.push(IDLE);
    while cws.len() % 16 != 0 {
        cws.push(IDLE);
    }
    let mut bits: Vec<bool> = (0..576).map(|i| i % 2 == 0).collect();
    for batch in cws.chunks(16) {
        bits.extend((0..32).rev().map(|i| 0x7CD2_15D8u32 >> i & 1 == 1));
        bits.extend(batch.iter().flat_map(|cw| (0..32).rev().map(move |i| cw >> i & 1 == 1)));
    }
    let mut phase = 0.0f64;
    (0..(bits.len() as f64 * fs / 1200.0) as usize)
        .map(|i| {
            let dev = if bits[(i as f64 * 1200.0 / fs) as usize] { -4500.0 } else { 4500.0 };
            phase += std::f64::consts::TAU * dev / fs;
            Complex32::from_polar(a, phase as f32)
        })
        .collect()
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
    let e = engine(clip("flex_synthetic_929.6125", on, 9_000.0), on, 929.0e6);
    // The 10 s synthetic slice holds five pages; looped, they repeat, and are kept once.
    let d = locked(&e, "FLEX", 40, |s| s.contains(": 5 pages"));
    // The pages are in its newest lines, not its sentence (a sentence is shown and logged everywhere; pages are
    // people's), and none came out damaged.
    assert!(d.recent.iter().any(|l| l.ends_with(": END OF SYNTHETIC SLICE")), "the newest page kept: {:?}", d.recent);
    assert!(!d.summary.contains("END OF SYNTHETIC SLICE") && !d.summary.contains(" | "), "no page in the sentence: {}", d.summary);
    assert!(!d.recent.iter().any(|l| l.contains("[damaged]")), "{} damaged", d.recent.iter().filter(|l| l.contains("[damaged]")).count());
    std::thread::sleep(Duration::from_secs(12));
    let d = e.decoder_views().into_iter().find(|d| d.mode == "FLEX").unwrap();
    assert!(d.summary.contains(": 5 pages"), "a loop's repeats are not new pages: {}", d.summary);
}

#[test]
fn a_channel_sending_flex_and_pocsag_gets_a_monitor_for_each_whose_claims_stay_on_their_own_records() {
    let on = 931.7125e6;
    // The FLEX slice's transmission (0.2-8.1 s; its quiet tail is noise, a pedestal in the simulator that would join
    // the two into one record), 2 s quiet, a POCSAG transmission at the FLEX signal's level, 2 s quiet.
    let (mut x, fs) = capture("flex_synthetic_929.6125", on);
    x.truncate((8.1 * fs) as usize);
    let sent = &x[(0.2 * fs) as usize..];
    let a = (sent.iter().map(|v| v.norm_sqr()).sum::<f32>() / sent.len() as f32).sqrt();
    let quiet = || std::iter::repeat_n(Complex32::default(), (2.0 * fs) as usize);
    x.extend(quiet());
    x.extend(pocsag(fs, a));
    x.extend(quiet());
    let e = engine(Clip::leak(&x, fs, 9_000.0), on, 931.0e6);
    // Both monitors decode: the four FLEX frames and their five pages (the loop repeats them, and the monitor counts
    // each once and then doesn't lock on it again, so its lamp is not waited for), and the three POCSAG pages, a page
    // cut by a block's end not counting as a fourth.
    let decoded = |mode: &str, frames: u64, pages: &str| {
        e.decoder_views().into_iter().find(|d| d.mode == mode && d.blocks_ok >= frames && d.summary.contains(pages))
    };
    let t0 = Instant::now();
    let (flex, pocsag) = loop {
        if let (Some(f), Some(p)) = (decoded("FLEX", 4, ": 5 pages in 4 frames"), decoded("POCSAG", 3, ": 3 pages")) {
            break (f, p);
        }
        let modes: Vec<_> = e.decoder_views().iter().map(|d| d.summary.clone()).collect();
        assert!(t0.elapsed() < Duration::from_secs(60), "monitors: {modes:?}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(flex.emitter, pocsag.emitter, "one emitter");
    std::thread::sleep(Duration::from_secs(15));
    let p = e.decoder_views().into_iter().find(|d| d.mode == "POCSAG").unwrap();
    assert!(p.summary.contains(": 3 pages"), "a page decoded again is not a new page: {}", p.summary);
    // Each record a monitor's claim is on was decoded as that protocol: no record carries decoder claims of both (the
    // bank's look files FLEX decoder claims too), nor one monitor's claim beside the bank's sync claim for the other.
    let t0 = Instant::now();
    loop {
        let view = e.records().view();
        let mut named = [0, 0];
        for r in view.records.iter().filter(|r| r.emitter == Some(flex.emitter)) {
            let mut decoded: Vec<&str> = r
                .claims
                .iter()
                .filter_map(|c| match &c.by {
                    Evidence::Decoder { mode, .. } => Some(mode.as_str()),
                    _ => None,
                })
                .collect();
            let synced: Vec<&str> = r
                .claims
                .iter()
                .filter_map(|c| match &c.by {
                    Evidence::SyncBank { protocol, .. } => Some(protocol.as_str()),
                    _ => None,
                })
                .collect();
            decoded.sort();
            decoded.dedup();
            assert!(decoded.len() <= 1, "record {} carries {decoded:?}", r.id.0);
            for (i, (mine, other)) in [("FLEX", "POCSAG"), ("POCSAG", "FLEX")].into_iter().enumerate() {
                if decoded.contains(&mine) {
                    named[i] += 1;
                    assert!(
                        !synced.iter().any(|p| p.starts_with(other)),
                        "record {} ({:.2} s) has a {mine} decoder claim beside a {other} sync claim: {:?}",
                        r.id.0,
                        (r.end_sample - r.first_sample) as f64 / 10e6,
                        r.claims
                    );
                }
            }
        }
        if named[0] > 0 && named[1] > 0 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "records named by each monitor: {named:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}
