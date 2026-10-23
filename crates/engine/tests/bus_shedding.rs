//! Spec §4.3: a saturated pool sheds the lowest-priority laggard.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Collector, cfg, keeper, sim, wait_until};
use engine::{BusConfig, ChannelBus, ChannelSpec, ConsumerState, Mode, Priority, Source, Start};

#[test]
fn a_saturated_pool_sheds_the_lowest_priority_laggard_and_the_rest_recover() {
    let (d, _c) = sim(10e6, true);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper()).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 1, max_lag_secs: 0.5, ..Default::default() });
    bus.attach(src.clone());
    let spec = |hz: f64| ChannelSpec { source: src.id(), center_hz: hz, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None };
    let light = Collector::new();
    let slow = Collector::sleeping(Duration::from_millis(15)); // 15 ms per 3.3 ms block: cannot keep up
    let a = bus.subscribe(spec(852.0e6), Start::Live, Priority::Listen, Box::new(light.clone())).unwrap();
    let b = bus.subscribe(spec(853.0e6), Start::Live, Priority::Recorder, Box::new(slow.clone())).unwrap();
    wait_until(|| bus.stats().shed_total == 1);
    let s = bus.stats();
    assert!(s.saturated);
    assert!(s.consumers.iter().all(|c| c.channel.id != b.id || c.state == ConsumerState::Shed));
    wait_until(|| slow.shed_deliveries() == 1);
    wait_until(|| bus.stats().consumers.iter().find(|c| c.channel.id == a.id).unwrap().lag_secs < 0.2);
    assert_eq!(bus.stats().shed_total, 1, "shed more than the one laggard");
}
