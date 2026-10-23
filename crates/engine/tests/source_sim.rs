mod common;

use common::{cfg, sim, wait_until};
use engine::{Boundary, BudgetConfig, Keeper, Mode, ReadError, RingBudget, Source};
use sdr_core::Complex32;

#[test]
fn streams_into_the_ring_and_reports_seconds() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    // The unpaced sim runs at ~280 MS/s, far beyond any real device, and starves the keeper's provisioning.
    let (d, _c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.stats().held_secs >= 1.0);
    let s = src.stats();
    assert!(s.held_secs <= 64.0 * 1048576.0 / 40e6 + 0.1, "{s:?}"); // never above the ceiling (1.6 s)
    assert!((s.guaranteed_secs - 0.5).abs() < 1e-9);
    assert!(s.ceiling_secs > 1.0);
    assert_eq!(s.gaps, 0);
    let mut out = vec![Complex32::default(); 1000];
    let seg = src.read(src.head() - 2000, &mut out).unwrap();
    assert_eq!(seg.center_hz, 855.25e6);
    assert!(src.time_of(src.head() - 1).is_some());
}

#[test]
fn retune_and_gap_make_segments_and_reads_across_them_are_refused() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() >= 3_000_000);
    let before = src.head();
    src.set_center(860e6).unwrap();
    wait_until(|| src.segment_at(src.head() - 1).center_hz == 860e6);
    let seg = src.segment_at(src.head() - 1);
    assert_eq!(seg.boundary, Boundary::Retune);
    assert!(seg.from_sample >= before && seg.from_sample.is_multiple_of(65_536));
    let mut out = vec![[0i16; 2]; 10];
    assert_eq!(src.read_raw(seg.from_sample - 5, &mut out), Err(ReadError::SpansSegments));
    c.inject_gap(70_000);
    wait_until(|| matches!(src.segment_at(src.head() - 1).boundary, Boundary::Gap { .. }));
    let g = src.segment_at(src.head() - 1);
    assert_eq!(g.boundary, Boundary::Gap { lost_samples: 70_000 });
    assert_eq!(src.stats().gaps, 1);
    assert_eq!(src.stats().lost_samples, 70_000);
    assert_eq!(src.stats().segments, 3);
}

#[test]
fn device_loss_is_flagged_without_a_restart() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig::default()));
    let (d, c) = sim(2.5e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() > 100_000);
    c.inject_overload();
    c.drop_device();
    wait_until(|| src.stats().device_lost);
    assert_eq!(src.stats().overloads, 1);
    assert_eq!(c.starts(), 1);
}
