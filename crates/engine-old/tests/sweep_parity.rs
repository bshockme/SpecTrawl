//! Spec §6 step 4a's test: a survey pass on the simulator produces the same row as the old sweep (`engine-old`).
//! Same plan, same simulated carriers; the rows must agree bin for bin in where the carriers are, how strong, and the
//! floor between them. (Four hops: below the eight a hop profile needs, so neither side reads or writes one.)

use std::time::{Duration, Instant};

use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::sweep::{STITCHED, SweepPlan};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, SourceConfig};

const CARRIERS: [(f64, f64); 3] = [(851.3e6, -50.0), (858.77e6, -62.0), (870.5e6, -45.0)];

fn device() -> Box<dyn sdr_core::Device> {
    let emitters = CARRIERS
        .iter()
        .map(|&(freq_hz, level_dbfs)| Emitter { freq_hz, level_dbfs, modulation: Modulation::Cw, keying: Keying::Continuous })
        .collect();
    let (d, _) = SimDevice::with_config(SimConfig { emitters, paced: true, report_lag_gaps: false, ..Default::default() });
    Box::new(d)
}

fn old_row() -> Vec<f32> {
    let plan = engine_old::SweepPlan { start_hz: 845e6, stop_hz: 875e6, spur_grid_hz: Some(5e6), ..Default::default() };
    let src = engine_old::Source::start(device(), engine_old::SourceConfig::default(), engine_old::Mode::Sweep(plan)).unwrap();
    let t0 = Instant::now();
    loop {
        let rows = src.rows_after(None);
        if rows.len() >= 3 {
            return rows.last().unwrap().db.clone();
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "the old sweep made {} rows", rows.len());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn new_row() -> Vec<f32> {
    let e = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 512 << 20, sweep_secs: 0.5 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
    });
    let src = e.add_source(device(), engine::Mode::Live).unwrap();
    src.set_sweep_plan(SweepPlan { start_hz: 845e6, stop_hz: 875e6, spur_grid_hz: Some(5e6), ..Default::default() });
    src.set_mode(engine::Mode::Sweep);
    let stage = e.bus().stage(src.id()).unwrap();
    let t0 = Instant::now();
    loop {
        let rows: Vec<_> = stage.rows().since(0).into_iter().filter(|r| r.segment.id.0 & STITCHED != 0).collect();
        if rows.len() >= 3 {
            return rows.last().unwrap().power.iter().map(|p| if p.is_nan() { -200.0 } else { 10.0 * p.log10() }).collect();
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "the new sweep made {} rows", rows.len());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn median(v: &[f32]) -> f32 {
    let mut s = v.to_vec();
    s.sort_by(f32::total_cmp);
    s[s.len() / 2]
}

#[test]
fn a_survey_pass_is_the_same_row_as_the_old_sweeps() {
    let (old, new) = (old_row(), new_row());
    assert_eq!(old.len(), new.len(), "different bin counts");
    assert!(old.iter().chain(&new).all(|v| *v > -199.0), "an unmeasured hop");
    // The same axis: 4 hops of 9 MHz from 845 MHz.
    let bin_hz = 36e6 / new.len() as f64;
    let bin = |f: f64| ((f - 845e6) / bin_hz) as usize;
    let (fo, fnew) = (median(&old), median(&new));
    assert!((fo - fnew).abs() < 1.5, "floors {fo:.1} vs {fnew:.1} dB");
    for (f, _) in CARRIERS {
        let k = bin(f);
        let peak = |r: &[f32]| (k - 4..k + 5).max_by(|&a, &b| r[a].total_cmp(&r[b])).unwrap();
        let (po, pn) = (peak(&old), peak(&new));
        assert!(po.abs_diff(pn) <= 1, "{f}: old peak at bin {po}, new at {pn}");
        assert!((old[po] - new[pn]).abs() < 1.5, "{f}: old {:.1} dB, new {:.1} dB", old[po], new[pn]);
        assert!(new[pn] > fnew + 25.0, "{f}: {:.1} dB over a {fnew:.1} dB floor", new[pn]);
    }
    // Away from the carriers the rows are the same noise: their quiet stretches agree on average.
    let quiet = |r: &[f32]| {
        let v: Vec<f32> =
            (0..r.len()).filter(|&i| CARRIERS.iter().all(|(f, _)| (i as f64 - bin(*f) as f64).abs() > 50.0)).map(|i| r[i]).collect();
        v.iter().sum::<f32>() / v.len() as f32
    };
    assert!((quiet(&old) - quiet(&new)).abs() < 1.0, "quiet bins {:.2} vs {:.2} dB", quiet(&old), quiet(&new));
}
