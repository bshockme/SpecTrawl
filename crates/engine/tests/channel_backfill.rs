mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Collector, Event, cfg, cw_sim, keeper, two_tone_sim, wait_until};
use dsp::Ddc;
use engine::{
    BudgetConfig, BusConfig, BusError, ChannelBus, ChannelSpec, ConsumerState, GapReason, Keeper, Mode, Priority, ReadError, RingBudget,
    Source, Start,
};
use sdr_core::Complex32;

const TONE_OFFSET: f64 = 6555.0 * (10e6 / 65_536.0) + 50.0; // odd centre bin, 50 Hz residual
const L: u64 = 32_768;

fn power(x: &[Complex32]) -> f32 {
    10.0 * (x.iter().map(|v| v.norm_sqr()).sum::<f32>() / x.len() as f32).log10()
}

/// The mean frequency of `x` at 78 125 S/s.
fn freq(x: &[Complex32]) -> f32 {
    x.windows(2).map(|w| (w[1] * w[0].conj()).arg()).sum::<f32>() / (x.len() - 1) as f32 * 78_125.0 / (2.0 * std::f32::consts::PI)
}

/// Peak deviation from the mean, relative to the mean.
fn ripple(x: &[Complex32]) -> f32 {
    let m = x.iter().sum::<Complex32>() / x.len() as f32;
    x.iter().map(|v| (v - m).norm()).fold(0.0f32, f32::max) / m.norm()
}

/// Samples delivered after the last `Gap`.
fn samples_after_last_gap(events: &[Event]) -> usize {
    let from = events.iter().rposition(|e| matches!(e, Event::Gap { .. })).map_or(0, |i| i + 1);
    events[from..].iter().map(|e| if let Event::Samples { len, .. } = e { *len } else { 0 }).sum()
}

/// Spec §6 step 2: a channel back-filled from 5 s ago matches a direct DDC to within filter tolerance.
#[test]
fn a_channel_backfilled_from_five_seconds_ago_matches_a_direct_ddc() {
    let fs = 10e6;
    let tone = 855.25e6 + TONE_OFFSET;
    let (d, _c) = two_tone_sim(fs, tone, tone + 25e3, -20.0); // the neighbour is 25 kHz up
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 8.0, ceiling_bytes: 512 << 20, sweep_secs: 0.2 }));
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 2, ..Default::default() });
    bus.attach(src.clone());
    wait_until(|| src.head() >= 60_000_000); // 6 s of history
    let start = ((src.head() - 50_000_000) / L) * L; // 5 s ago, on a hop
    let want = 78_125usize; // 1 s of channel output
    // Reference: the old-style DDC straight off the ring (same rate: 10e6 / 128 = 78125).
    let mut raw = vec![Complex32::default(); 12_000_000]; // 1.2 s: the reference must be ≥ `want` long for the slice below
    src.read(start - L, &mut raw).unwrap();
    let mut ddc = Ddc::new(fs, TONE_OFFSET, 6_250.0, 78_125.0);
    assert_eq!(ddc.fs_out, 78_125.0);
    let mut reference = Vec::new();
    ddc.process(&raw, &mut reference);
    // Back-filled channel.
    let got = Collector::with_limit(want + 4096); // returns Flow::Stop when full
    let spec = ChannelSpec { source: src.id(), center_hz: tone, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None };
    bus.subscribe(spec, Start::At(start), Priority::Listen, Box::new(got.clone())).unwrap();
    wait_until(|| got.samples() >= want);
    assert_eq!(got.gaps(), 0);
    assert!(got.first_delivery_first_sample() == start);
    let (a, b) = (got.tail(want / 2), reference[reference.len() / 2..reference.len() / 2 + want / 2].to_vec());
    let s = bus.stats().consumers[0].clone();
    assert!((power(&a) - power(&b)).abs() < 0.5, "power {} vs {} dB", power(&a), power(&b));
    assert!(freq(&a).abs() < 1.0 && freq(&b).abs() < 1.0, "tone not at DC: {} / {} Hz", freq(&a), freq(&b));
    assert!(ripple(&a) < 0.02 && ripple(&b) < 0.02, "neighbour leaks: {} / {}", ripple(&a), ripple(&b));
    assert!(s.recomputed > 100, "back-fill should have recomputed from the ring: {s:?}");
    // It stops at its limit, seconds behind the live edge: an ended consumer has no lag.
    wait_until(|| bus.stats().consumers[0].state == ConsumerState::Ended);
    let s = bus.stats().consumers[0].clone();
    assert_eq!((s.lag_blocks, s.lag_secs), (0, 0.0), "{s:?}");
}

#[test]
fn backfill_over_retired_history_reports_one_gap_and_continues() {
    // 16 MiB holds about 0.4 s at 10 MS/s (1 MiB blocks).
    let small = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.2, ceiling_bytes: 16 << 20, sweep_secs: 0.2 }));
    let (d, _c) = cw_sim(10e6, 855.25e6 + TONE_OFFSET, -20.0);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &small).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 1, ..Default::default() });
    bus.attach(src.clone());
    wait_until(|| src.oldest() > 0);
    // A consumer that sleeps 50 ms per delivery, so the ring retires blocks under it.
    let got = Collector::sleeping(Duration::from_millis(50));
    let spec =
        ChannelSpec { source: src.id(), center_hz: 855.25e6 + TONE_OFFSET, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None };
    // The ring may retire the start point between reading `oldest` and subscribing: try again.
    let mut tries = 0;
    loop {
        let start = src.oldest() / L * L + L;
        match bus.subscribe(spec.clone(), Start::At(start), Priority::Listen, Box::new(got.clone())) {
            Ok(_) => break,
            Err(BusError::NotHeld { .. }) if tries < 10 => tries += 1,
            Err(e) => panic!("subscribe: {e:?}"),
        }
    }
    let samples_deliveries = || got.events().iter().filter(|e| matches!(e, Event::Samples { .. })).count();
    wait_until(|| samples_deliveries() >= 5);
    let events = got.events();
    assert!(got.gaps() >= 1, "{events:?}");
    for (i, e) in events.iter().enumerate() {
        let Event::Gap { from_sample, to_sample, reason } = *e else { continue };
        assert_eq!(reason, GapReason::Unreadable(ReadError::Overwritten), "{e:?}");
        assert!(from_sample < to_sample, "{e:?}");
        if let Some(Event::Samples { first_sample, .. }) = events[i + 1..].iter().find(|e| matches!(e, Event::Samples { .. })) {
            assert!(first_sample / L >= to_sample / L, "Samples at {first_sample} after {e:?}");
        }
    }
}

#[test]
fn a_retune_delivers_a_gap_then_continues_on_the_new_tuning() {
    let tone = 855.25e6 + TONE_OFFSET;
    let (d, _c) = cw_sim(10e6, tone, -20.0);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper()).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 2, ..Default::default() });
    bus.attach(src.clone());
    let got = Collector::new();
    let spec = ChannelSpec { source: src.id(), center_hz: tone, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None };
    let info = bus.subscribe(spec, Start::Live, Priority::Listen, Box::new(got.clone())).unwrap();
    let stats = || bus.stats().consumers.into_iter().find(|c| c.channel.id == info.id).unwrap();
    wait_until(|| got.samples() >= 78_125);
    let before = got.tail(78_125 / 2);

    // Retune: the tone is now at −3.75 MHz, inside ±4.5 MHz.
    src.set_center(860e6).unwrap();
    wait_until(|| got.gaps() >= 1 && samples_after_last_gap(&got.events()) >= 78_125);
    let after = got.tail(78_125 / 2);
    let events = got.events();
    let gaps: Vec<usize> = events.iter().enumerate().filter(|(_, e)| matches!(e, Event::Gap { .. })).map(|(i, _)| i).collect();
    assert_eq!(gaps.len(), 1, "{:?}", gaps.iter().map(|&i| events[i]).collect::<Vec<_>>());
    let i = gaps[0];
    let Event::Gap { from_sample, to_sample, reason } = events[i] else { unreachable!() };
    assert!(matches!(reason, GapReason::Unreadable(ReadError::SpansSegments | ReadError::Gap)), "reason {reason:?}");
    let (Event::Samples { first_sample: prev, .. }, Event::Samples { first_sample: next, .. }) = (events[i - 1], events[i + 1]) else {
        panic!("the gap is not between two Samples: {:?}", &events[i - 1..=i + 1]);
    };
    assert_eq!((from_sample, to_sample), (prev + L, next), "the gap does not tile the deliveries around it");
    assert_eq!(src.segment_at(next - L).center_hz, 860e6);
    let level = |x: &[Complex32]| 20.0 * (x.iter().sum::<Complex32>() / x.len() as f32).norm().log10();
    assert!((level(&after) - level(&before)).abs() < 0.5, "level {} -> {} dBFS", level(&before), level(&after));
    assert!(freq(&before).abs() < 1.0 && freq(&after).abs() < 1.0, "tone not at DC: {} / {} Hz", freq(&before), freq(&after));

    // Out of span: deliveries stop and the subscription waits, with no lag.
    src.set_center(870e6).unwrap();
    wait_until(|| stats().state == ConsumerState::OutOfSpan);
    let (n, parked) = (got.samples(), stats());
    let delivered = got.events();
    std::thread::sleep(Duration::from_millis(300));
    let s = stats();
    assert!(got.samples() <= n + 512, "{} samples while out of span", got.samples() - n);
    assert_eq!((s.lag_blocks, s.lag_secs), (0, 0.0), "{s:?}");
    assert_eq!(s.next_block, parked.next_block, "an out-of-span subscription should wait, not step block by block");

    // Back in span: one OutOfSpan gap covers the stretch, then Samples.
    src.set_center(855.25e6).unwrap();
    wait_until(|| stats().state == ConsumerState::Running && samples_after_last_gap(&got.events()) >= 2560);
    let events = got.events();
    let Some(&Event::Samples { first_sample: last_before, .. }) = delivered.iter().rev().find(|e| matches!(e, Event::Samples { .. }))
    else {
        panic!("no Samples before the retune out of span");
    };
    let k = events.iter().position(|e| matches!(e, Event::Samples { first_sample, .. } if *first_sample == last_before)).unwrap();
    let first_after = events[k + 1..].iter().position(|e| matches!(e, Event::Samples { .. })).unwrap() + k + 1;
    let between = &events[k + 1..first_after];
    let oos: Vec<&Event> = between.iter().filter(|e| matches!(e, Event::Gap { reason: GapReason::OutOfSpan, .. })).collect();
    assert_eq!(oos.len(), 1, "{between:?}");
    for e in between {
        assert!(
            matches!(e, Event::Gap { reason: GapReason::OutOfSpan | GapReason::Unreadable(ReadError::SpansSegments | ReadError::Gap), .. }),
            "{between:?}"
        );
    }
    // The deliveries tile the input: no sample is skipped unreported.
    let mut at = last_before + L;
    for e in between {
        let Event::Gap { from_sample, to_sample, .. } = *e else { unreachable!() };
        assert_eq!(from_sample, at, "{between:?}");
        at = to_sample;
    }
    let Event::Samples { first_sample: resumed, .. } = events[first_after] else { unreachable!() };
    assert_eq!(resumed, at, "{between:?}");
    let Event::Gap { from_sample, to_sample, .. } = *oos[0] else { unreachable!() };
    assert_eq!(src.segment_at(from_sample - L).center_hz, 870e6, "the OutOfSpan gap does not start with the 870 MHz stretch");
    assert_eq!(src.segment_at(to_sample - L).center_hz, 855.25e6, "the OutOfSpan gap does not end where the 855.25 MHz stretch starts");
}
