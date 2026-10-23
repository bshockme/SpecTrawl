mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Collector, cfg, cw_sim, keeper, wait_until};
use dsp::channelizer::DesignError;
use engine::{
    BudgetConfig, BusConfig, BusError, ChannelBus, ChannelId, ChannelSpec, ConsumerState, Delivery, EndReason, Flow, Keeper, Mode,
    Priority, RingBudget, Source, SourceId, Start,
};
use sdr_core::Complex32;

const TONE_OFFSET: f64 = 6555.0 * (10e6 / 65_536.0) + 50.0; // odd centre bin, 50 Hz residual

fn spec(source: SourceId, center_hz: f64) -> ChannelSpec {
    ChannelSpec { source, center_hz, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None }
}

#[test]
fn a_live_channel_delivers_the_tone_at_dc_in_order_with_small_lag() {
    let (d, _c) = cw_sim(10e6, 855.25e6 + TONE_OFFSET, -20.0);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper()).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 2, ..Default::default() });
    bus.attach(src.clone());
    let got = Collector::new();
    let info = bus
        .subscribe(
            ChannelSpec { source: src.id(), center_hz: 855.25e6 + TONE_OFFSET, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None },
            Start::Live,
            Priority::Listen,
            Box::new(got.clone()),
        )
        .unwrap();
    assert_eq!((info.sample_rate, info.samples_per_block, info.hop), (78_125.0, 256, 32_768));
    wait_until(|| got.samples() >= 2 * 78_125);
    let s = bus.stats().consumers.into_iter().find(|c| c.channel.id == info.id).unwrap();
    assert_eq!(s.gaps, 0);
    assert!(s.lag_secs < 0.2, "{s:?}");
    assert_eq!(s.state, ConsumerState::Running);
    assert!(got.first_samples_are_consecutive_by(32_768));
    let tail = got.tail(78_125 / 2); // skip settling
    let mean = tail.iter().sum::<Complex32>() / tail.len() as f32;
    let ripple = tail.iter().map(|y| (y - mean).norm()).fold(0.0f32, f32::max);
    eprintln!("lag {:.4} s, level {:.3} dBFS, ripple {:.5}", s.lag_secs, 20.0 * mean.norm().log10(), ripple / mean.norm());
    assert!((20.0 * mean.norm().log10() - (-20.0)).abs() < 0.5, "level {} dBFS", 20.0 * mean.norm().log10());
    assert!(ripple < 0.02 * mean.norm());
    assert!(bus.unsubscribe(info.id));
    let n = got.samples();
    std::thread::sleep(Duration::from_millis(200));
    assert!(got.samples() <= n + 256, "deliveries after unsubscribe");
}

#[test]
fn bad_subscriptions_are_refused() {
    let held = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, _c) = cw_sim(10e6, 855.25e6 + TONE_OFFSET, -20.0);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &held).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 1, ..Default::default() });
    bus.attach(src.clone());
    let sub = |spec: ChannelSpec, start: Start| bus.subscribe(spec, start, Priority::Classify, Box::new(Collector::counting_only()));

    let nested = ChannelSpec { parent: Some(ChannelId(1)), ..spec(src.id(), 855.25e6) };
    assert_eq!(sub(nested, Start::Live).unwrap_err(), BusError::NestedChannelsNotSupported);
    assert_eq!(sub(spec(src.id(), 855.25e6 + 4.6e6), Start::Live).unwrap_err(), BusError::Design(DesignError::OutsideSpan));
    let wide = ChannelSpec { bandwidth_hz: 3e6, ..spec(src.id(), 855.25e6) };
    assert_eq!(sub(wide, Start::Live).unwrap_err(), BusError::Design(DesignError::TooWide));
    assert_eq!(sub(spec(SourceId(99_999), 855.25e6), Start::Live).unwrap_err(), BusError::NoSuchSource);

    // 64 MiB holds about 1.6 s at 10 MS/s: wait until the ring has retired its first blocks.
    wait_until(|| src.oldest() > 2 * 32_768);
    let before = src.oldest();
    match sub(spec(src.id(), 855.25e6), Start::At(0)) {
        Err(BusError::NotHeld { oldest }) => assert!(oldest >= before && oldest >= 1, "oldest {oldest}, was {before}"),
        other => panic!("expected NotHeld, got {:?}", other.map(|i| i.id)),
    }
    assert!(bus.stats().consumers.is_empty(), "a refused subscription left an entry");
}

#[test]
fn a_bus_has_at_least_one_worker() {
    assert_eq!(ChannelBus::new(BusConfig { workers: 0, ..Default::default() }).stats().workers, 1);
}

#[test]
fn a_panicking_consumer_is_ended_and_the_others_continue() {
    let (d, _c) = cw_sim(10e6, 855.25e6 + TONE_OFFSET, -20.0);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper()).unwrap());
    // One worker: deliveries after the panic prove the worker that caught it survived.
    let bus = ChannelBus::new(BusConfig { workers: 1, ..Default::default() });
    assert_eq!(bus.stats().workers, 1);
    bus.attach(src.clone());
    let mut seen = 0;
    let bad = move |d: Delivery<'_>| {
        if let Delivery::Samples { .. } = d {
            seen += 1;
            if seen == 3 {
                panic!("a test consumer panicking on its third block");
            }
        }
        Flow::Continue
    };
    let bad = bus.subscribe(spec(src.id(), 855.25e6 + TONE_OFFSET), Start::Live, Priority::Classify, Box::new(bad)).unwrap();
    let got = Collector::counting_only();
    let good = bus.subscribe(spec(src.id(), 855.25e6 + TONE_OFFSET), Start::Live, Priority::Classify, Box::new(got.clone())).unwrap();
    let state = |id: ChannelId| bus.stats().consumers.into_iter().find(|c| c.channel.id == id).unwrap();
    wait_until(|| state(bad.id).state == ConsumerState::Ended && bus.stats().panicked_total == 1);
    let n = got.samples();
    wait_until(|| got.samples() >= n + 78_125);
    assert_eq!(bus.stats().panicked_total, 1);
    assert_eq!(state(bad.id).end_reason, Some(EndReason::Panicked));
    let g = state(good.id);
    assert_eq!(g.state, ConsumerState::Running);
    assert_eq!(g.end_reason, None);
    assert!(g.lag_secs < 0.5, "{g:?}");
    assert!(bus.unsubscribe(bad.id), "an ended subscription keeps its entry until unsubscribed");
    assert!(!bus.unsubscribe(bad.id));
}

#[test]
fn consumers_are_scheduled_by_priority_then_round_robin() {
    let (d, _c) = cw_sim(10e6, 855.25e6 + TONE_OFFSET, -20.0);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper()).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 1, ..Default::default() });
    bus.attach(src.clone());
    let log: Arc<Mutex<Vec<(char, u64, Instant)>>> = Arc::default();
    let recorder = |name: char| {
        let log = log.clone();
        Box::new(move |d: Delivery<'_>| {
            if let Delivery::Samples { block, .. } = d {
                log.lock().unwrap().push((name, block, Instant::now()));
            }
            Flow::Continue
        })
    };
    let at = |p: Priority, name: char| bus.subscribe(spec(src.id(), 855.25e6 + TONE_OFFSET), Start::Live, p, recorder(name)).unwrap();
    let ids = [at(Priority::Classify, 'A').id, at(Priority::Classify, 'B').id, at(Priority::Listen, 'C').id];
    std::thread::sleep(Duration::from_secs(1));
    for id in ids {
        assert!(bus.unsubscribe(id));
    }
    let log = log.lock().unwrap().clone();
    let count = |n: char| log.iter().filter(|e| e.0 == n).count();
    let (a, b, c) = (count('A'), count('B'), count('C'));
    assert!(c > 100, "C got {c} blocks in 1 s");
    assert!(a.abs_diff(b) as f64 <= 0.1 * a.max(b) as f64, "A {a} / B {b}");
    // C never waits behind A or B: for every block all three received, C's delivery came first.
    // (Ordering, not a wall-clock bound on C's inter-delivery gaps: those follow the sim's
    // bursty pacing and the stage's 1 ms poll, and passed 10 ms under load on Windows.)
    let when = |n: char, block: u64| log.iter().find(|e| e.0 == n && e.1 == block).map(|e| e.2);
    let mut compared = 0;
    for &(_, block, tc) in log.iter().filter(|e| e.0 == 'C') {
        if let (Some(ta), Some(tb)) = (when('A', block), when('B', block)) {
            assert!(tc <= ta && tc <= tb, "block {block}: C after A or B");
            compared += 1;
        }
    }
    eprintln!("A {a}, B {b}, C {c} blocks; {compared} compared");
    assert!(compared > 100, "only {compared} blocks reached all three");
}
