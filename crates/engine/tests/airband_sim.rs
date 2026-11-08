//! Air band voice on the simulator (step 7, `engine::decoders::airband`): an AM transmitter keying on a raster channel
//! gets the bank's AM, then the air band's claim naming its channel, and a CHANNELS row; a bare carrier on another
//! raster point gets neither, and is not a channel.

use std::time::{Duration, Instant};

use api::{Evidence, Identity};
use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};

const AM_HZ: f64 = 124.85e6;
const CW_HZ: f64 = 124.80e6;

#[test]
fn an_am_call_on_the_raster_is_air_band_voice_and_a_bare_carrier_is_not() {
    let emitters = vec![
        Emitter {
            freq_hz: AM_HZ + 300.0,
            level_dbfs: -40.0,
            modulation: Modulation::AmTone { tone_hz: 1000.0, depth: 0.7 },
            keying: Keying::Bursts { period_s: 2.5, duty: 0.6, phase: 0.2 },
        },
        Emitter {
            freq_hz: CW_HZ,
            level_dbfs: -45.0,
            modulation: Modulation::Cw,
            keying: Keying::Bursts { period_s: 3.0, duty: 0.5, phase: 0.5 },
        },
    ];
    let (d, _) =
        SimDevice::with_config(SimConfig { emitters, center_hz: 124.5e6, paced: true, report_lag_gaps: false, ..Default::default() });
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 10.0, ceiling_bytes: 1 << 30, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: Default::default(),
    });
    let _src = e.add_source(Box::new(d), Mode::Live).unwrap();
    let t0 = Instant::now();
    let row = loop {
        let view = e.records().view();
        let channels = engine::decoders::airband::channels(&view);
        if let Some(row) = channels.channels.iter().find(|c| c.name == "124.850" && c.voice >= 2) {
            break row.clone();
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{channels:?}\n{:?}", view.records);
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!((row.plan.as_str(), row.hz), ("Air band", AM_HZ));
    assert_eq!(row.summary, "AM voice");
    let view = e.records().view();
    let channels = engine::decoders::airband::channels(&view);
    assert!(channels.channels.iter().all(|c| c.name != "124.800"), "a bare carrier is not a channel: {channels:?}");
    // The calls' records carry the claim, and their emitter headlines as the channel.
    for id in &row.records {
        let r = view.records.iter().find(|r| r.id == *id).unwrap();
        if let Some(c) = r.claims.iter().find(|c| matches!(&c.by, Evidence::Decoder { mode, .. } if mode == "Air band AM")) {
            assert_eq!(c.label, "Air band voice");
            assert!(
                matches!(&c.by, Evidence::Decoder { identity: Some(Identity::Channel { name, .. }), .. } if name == "124.850"),
                "{c:?}"
            );
        }
    }
    let em = view.emitters.iter().find(|em| (em.center_hz - AM_HZ).abs() < 3e3).expect("the AM emitter");
    assert_eq!(em.headline.as_ref().map(|h| h.label.as_str()), Some("Air band voice"), "{em:?}");
    for r in view.records.iter().filter(|r| (r.center_hz() - CW_HZ).abs() < 3e3) {
        assert!(r.claims.iter().all(|c| !matches!(&c.by, Evidence::Decoder { .. })), "carrier: {:?}", r.claims);
    }
}
