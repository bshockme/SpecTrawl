mod common;

use common::{cfg, sim, wait_until};
use engine::{Boundary, BudgetConfig, ExportError, Keeper, Mode, ReadError, RingBudget, Source};
use std::time::{Duration, Instant};

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
    // The ring is the point here: the keeper kept it provisioned. (The sim
    // reports no lag gaps, so device gaps are zero too.)
    assert_eq!(s.starved_samples, 0, "{s:?}");
    assert_eq!(s.device_gaps, 0);
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
    assert_eq!(src.stats().device_gaps, 1);
    assert_eq!(src.stats().device_lost_samples, 70_000);
    assert_eq!(src.stats().segments, 3);
}

#[test]
fn reads_and_exports_touching_a_device_gap_are_refused() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() >= 1_000_000);
    // Shorter than the keeper's provision-ahead window (10 blocks), so the
    // hole lands in provisioned, zero-filled memory.
    c.inject_gap(70_000);
    wait_until(|| matches!(src.segment_at(src.head() - 1).boundary, Boundary::Gap { .. }));
    let g = src.segment_at(src.head() - 1);
    let hole = g.hole().unwrap();
    assert_eq!(hole.end - hole.start, 70_000);
    assert_eq!(g.data_from(), hole.end);
    wait_until(|| src.head() >= g.data_from() + 500_000);
    let mut out = vec![[0i16; 2]; 1000];
    assert_eq!(src.read_raw(hole.start + 100, &mut out), Err(ReadError::Gap)); // inside the hole
    assert_eq!(src.read_raw(hole.start - 500, &mut out), Err(ReadError::Gap)); // data, then hole
    assert_eq!(src.read_raw(hole.end - 500, &mut out), Err(ReadError::Gap)); // hole, then data
    let mut f = vec![Complex32::default(); 1000];
    assert_eq!(src.read(hole.start + 100, &mut f), Err(ReadError::Gap));
    assert_eq!(src.read_raw(g.data_from(), &mut out).unwrap().id, g.id); // entirely after
    assert_ne!(src.read_raw(hole.start - 1000, &mut out).unwrap().id, g.id); // entirely before
    // A range reaching past `head` is refused as Future before its segment is
    // resolved (a boundary may still land inside it), even though it also
    // touches the hole.
    let mut long = vec![[0i16; 2]; (src.head() + 1_000_000 - hole.start) as usize];
    assert_eq!(src.read_raw(hole.start, &mut long), Err(ReadError::Future));
    let dir = std::env::temp_dir().join(format!("spectrawl-export-gap-{}", std::process::id()));
    let over = src.export_wideband(hole.start - 100_000, hole.end + 100_000, &dir, "over");
    assert!(matches!(over, Err(ExportError::Ring(engine::ring::ReadError::Gap))), "{over:?}");
    let ahead = src.export_wideband(hole.start, src.head() + 1_000_000, &dir, "ahead");
    assert!(matches!(ahead, Err(ExportError::Ring(engine::ring::ReadError::Future))), "{ahead:?}");
    let inside = src.export_wideband(hole.start + 10, hole.start + 20, &dir, "inside");
    assert!(matches!(inside, Err(ExportError::Ring(engine::ring::ReadError::Gap))), "{inside:?}");
    assert!(!dir.join("over.sigmf-data").exists() && !dir.join("inside.sigmf-data").exists());
    let after = src.export_wideband(g.data_from(), g.data_from() + 100_000, &dir, "after").unwrap();
    assert_eq!(after.segment.id, g.id);
    let _ = std::fs::remove_dir_all(&dir);
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

#[test]
fn silent_driver_death_is_detected_by_the_watchdog() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, c) = sim(2.5e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() > 100_000);
    c.stall();
    let t0 = Instant::now();
    wait_until(|| src.stats().device_lost);
    // The watchdog counts 2 s from the last sample, which can precede `t0` by up to one
    // 65,536-sample sim block (26 ms at 2.5 MS/s).
    let block = Duration::from_secs_f64(65_536.0 / 2.5e6);
    assert!(t0.elapsed() >= Duration::from_secs(2) - block, "{:?}", t0.elapsed());
    assert_eq!(c.starts(), 1);
}

#[test]
fn a_source_keeps_its_keeper_alive() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(2.5e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    drop(keeper); // the caller lets go; the source still has one
    wait_until(|| src.stats().held_secs >= 0.5); // still housekept (blocks keep being provisioned)
    assert_eq!(src.stats().starved_samples, 0);
    assert_eq!(src.keeper().tick_panics(), 0);
}

#[test]
fn export_error_is_a_std_error() {
    fn is_error<E: std::error::Error + Send + Sync + 'static>() {}
    is_error::<ExportError>();
    let e = ExportError::Ring(engine::ring::ReadError::Gap);
    assert_eq!(e.to_string(), "the range touches a gap: samples the device never delivered");
}
