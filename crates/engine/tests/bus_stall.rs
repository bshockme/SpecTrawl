//! A stall of every worker pushes 200 subscriptions past the stage's cache
//! together: the bus must recompute each missed block once, shared by all of
//! them, and catch up. The count scales with the worker pool
//! (`common::load_channels`): 200 at 14 workers, 14 on a one-worker CI runner.

mod common;

use std::time::{Duration, Instant};

use backend_sim::{SimConfig, SimDevice};
use common::{Collector, cfg, load_bus_config, load_channels, wait_until};
use engine::{BudgetConfig, ChannelSpec, Engine, EngineConfig, Mode, Priority, SourceId, Start};

fn spec(source: SourceId, center_hz: f64) -> ChannelSpec {
    ChannelSpec { source, center_hz, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None }
}

#[test]
fn two_hundred_channels_recover_from_a_stall_of_every_worker() {
    let bus_config = load_bus_config();
    let n = load_channels(bus_config.workers);
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: bus_config,
        source: cfg(),
        store: None,
    });
    let (d, _c) = SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() }); // the default 800 MHz scenario
    let src = engine.add_source(Box::new(d), Mode::Live).unwrap();
    let bus = engine.bus();
    let stage = bus.stage(src.id()).unwrap();
    let collectors: Vec<Collector> = (0..n).map(|_| Collector::counting_only()).collect();
    for (i, c) in collectors.iter().enumerate() {
        bus.subscribe(spec(src.id(), 851.3e6 + i as f64 * 12_500.0), Start::Live, Priority::Classify, Box::new(c.clone())).unwrap();
    }
    let max_lag = || bus.stats().consumers.iter().map(|c| c.lag_secs).fold(0.0, f64::max);
    wait_until(|| collectors.iter().all(|c| c.samples() > 0) && max_lag() < 0.05);

    // One sleeper per worker: their first deliveries hold every worker for 200 ms.
    let workers = bus.stats().workers;
    let sleepers: Vec<Collector> = (0..workers).map(|_| Collector::sleeping_once(Duration::from_millis(200))).collect();
    let blocks_before = stage.stats().blocks;
    let t0 = Instant::now();
    for (i, c) in sleepers.iter().enumerate() {
        bus.subscribe(spec(src.id(), 851.3e6 + i as f64 * 12_500.0), Start::Live, Priority::Listen, Box::new(c.clone())).unwrap();
    }
    wait_until(|| sleepers.iter().all(|c| c.deliveries() >= 1));
    let stall_blocks = stage.stats().blocks - blocks_before;

    let mut worst = 0.0f64;
    let recovered = loop {
        let lag = max_lag();
        worst = worst.max(lag);
        if lag < 0.2 {
            break Some(t0.elapsed());
        }
        if t0.elapsed() > Duration::from_secs(3) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let s = bus.stats();
    let recomputed_by = |p: Priority| s.consumers.iter().filter(|c| c.priority == p).map(|c| c.recomputed).sum::<u64>();
    let recomputed: u64 = s.consumers.iter().map(|c| c.recomputed).sum();
    println!(
        "stall of {workers} workers for 200 ms: max lag {worst:.3} s, recovered {:?} s after subscribing, recomputed {recomputed} blocks ({} by the {n} Classify, {} by the {workers} Listen), stage produced {stall_blocks} blocks during the stall",
        recovered.map(|d| d.as_secs_f64()),
        recomputed_by(Priority::Classify),
        recomputed_by(Priority::Listen),
    );
    let Some(recovered) = recovered else { panic!("max lag still {:.3} s 3 s after the stall", max_lag()) };
    assert!(recovered < Duration::from_secs(3));
    assert_eq!(s.shed_total, 0, "{s:?}");
    assert!(recomputed < 2 * stall_blocks, "recomputed {recomputed} blocks for a stall of {stall_blocks}: not shared");
}
