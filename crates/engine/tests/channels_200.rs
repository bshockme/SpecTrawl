//! Spec §6 step 2: 200 simultaneous channels at 10 MS/s, with measured lag.
//! The count scales with the worker pool (`common::load_channels`): 200 at
//! 14 workers, 14 on a one-worker CI runner.

mod common;

use std::time::{Duration, Instant};

use backend_sim::{SimConfig, SimDevice};
use common::{Collector, cfg, load_bus_config, load_channels};
use engine::{BudgetConfig, ChannelSpec, ConsumerState, Engine, EngineConfig, Mode, Priority, Start};

#[test]
fn two_hundred_channels_at_ten_megasamples_keep_up() {
    let bus_config = load_bus_config();
    let n = load_channels(bus_config.workers);
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: bus_config,
        source: cfg(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
    });
    let (d, _c) = SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() }); // the default 800 MHz scenario
    let src = engine.add_source(Box::new(d), Mode::Live).unwrap();
    let collectors: Vec<Collector> = (0..n).map(|_| Collector::counting_only()).collect();
    for (i, c) in collectors.iter().enumerate() {
        engine
            .bus()
            .subscribe(
                ChannelSpec {
                    source: src.id(),
                    center_hz: 851.3e6 + i as f64 * 12_500.0,
                    bandwidth_hz: 12_500.0,
                    min_rate: 48_000.0,
                    parent: None,
                },
                Start::Live,
                Priority::Classify,
                Box::new(c.clone()),
            )
            .unwrap();
    }
    let (t0, head_before) = (Instant::now(), src.head());
    std::thread::sleep(Duration::from_secs(5));
    let (elapsed, head_after) = (t0.elapsed().as_secs_f64(), src.head());
    let s = engine.bus().stats();
    let stage = engine.bus().stage(src.id()).unwrap().stats();
    let delivered_secs = (head_after - head_before) as f64 / 10e6;
    let lags: Vec<f64> = s.consumers.iter().map(|c| c.lag_secs).collect();
    let max_lag = lags.iter().cloned().fold(0.0, f64::max);
    println!(
        "{n} channels @ 10 MS/s: max lag {max_lag:.3} s, mean lag {:.3} s, stage {:.0} µs/block, stage behind {:.3} s, recomputed {} blocks, workers {}",
        lags.iter().sum::<f64>() / n as f64,
        stage.block_micros,
        stage.behind_secs,
        s.consumers.iter().map(|c| c.recomputed).sum::<u64>(),
        s.workers
    );
    // The source delivered real time, and the stage kept up with it.
    assert!((delivered_secs - elapsed).abs() <= 0.1 * elapsed, "{delivered_secs:.3} s of samples in {elapsed:.3} s");
    assert!(stage.behind_secs < 0.2, "{stage:?}");
    assert_eq!(s.shed_total, 0, "{s:?}");
    assert!(max_lag < 1.0, "max lag {max_lag} s");
    assert!(s.consumers.iter().all(|c| c.gaps == 0 && c.state == ConsumerState::Running));
    assert!(collectors.iter().all(|c| c.samples() >= 3 * 78_125), "a channel received under 3 s of samples in 5 s");
    assert_eq!(src.stats().starved_samples, 0);
    assert_eq!(stage.skipped_blocks, 0);
}
