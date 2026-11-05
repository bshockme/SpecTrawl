//! The `Engine` composes the keeper, the bus and its sources; removing a
//! source ends its consumers once and stops its stage.

mod common;

use std::time::Duration;

use common::{Collector, cfg, sim, wait_until};
use engine::{BudgetConfig, BusConfig, BusError, ChannelSpec, EndReason, Engine, EngineConfig, Mode, Priority, Start};

#[test]
fn removing_a_source_ends_its_consumers_once_and_stops_its_stage() {
    let engine = Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 128 << 20, sweep_secs: 0.2 },
        bus: BusConfig { workers: 2, ..Default::default() },
        source: cfg(),
        store: None,
    });
    let (d, _c) = sim(10e6, true);
    let src = engine.add_source(d, Mode::Live).unwrap();
    let spec = ChannelSpec { source: src.id(), center_hz: 852.0e6, bandwidth_hz: 12_500.0, min_rate: 48e3, parent: None };
    let got = Collector::new();
    let info = engine.bus().subscribe(spec.clone(), Start::Live, Priority::Classify, Box::new(got.clone())).unwrap();
    wait_until(|| got.samples() > 78_125);
    let stage = engine.bus().stage(src.id()).unwrap();
    assert!(engine.remove_source(src.id()));
    wait_until(|| got.ended() == Some(EndReason::SourceDetached));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(got.end_deliveries(), 1);
    let entry = engine.bus().stats().consumers.into_iter().find(|c| c.channel.id == info.id).unwrap();
    assert_eq!(entry.end_reason, Some(EndReason::SourceDetached));
    assert!(engine.bus().stage(src.id()).is_none());
    // The stage thread has exited: no block is transformed any more.
    let blocks = stage.stats().blocks;
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(stage.stats().blocks, blocks, "the stage still runs after its source was removed");
    assert!(matches!(
        engine.bus().subscribe(spec, Start::Live, Priority::Classify, Box::new(Collector::new())),
        Err(BusError::NoSuchSource)
    ));
    assert_eq!(engine.keeper().tick_panics(), 0);
}
