mod common;

use common::{cfg, sim, wait_until};
use engine::{BudgetConfig, Keeper, Mode, RingBudget, Source};

#[test]
fn two_live_sources_hold_equal_seconds_and_a_mode_change_redistributes_without_restart() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.25, ceiling_bytes: 100 << 20, sweep_secs: 0.1 }));
    // The unpaced sim runs at ~280 MS/s, far beyond any real device, and starves the keeper's provisioning.
    let (d1, c1) = sim(10e6, true);
    let (d2, c2) = sim(2.5e6, true);
    let a = Source::start(d1, cfg(), Mode::Live, &keeper).unwrap();
    let b = Source::start(d2, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| a.stats().held_secs >= a.stats().ceiling_secs - 0.1 && b.stats().held_secs >= b.stats().ceiling_secs - 0.1);
    let (sa, sb) = (a.stats(), b.stats());
    assert!((sa.ceiling_secs - sb.ceiling_secs).abs() < 0.15, "{sa:?} {sb:?}"); // 80 and 21 blocks: 2.10 s and 2.20 s
    assert!((sa.held_secs - sb.held_secs).abs() < 0.3, "{sa:?} {sb:?}");
    b.set_mode(Mode::Sweep);
    wait_until(|| b.stats().ceiling_secs < 0.3 && a.stats().ceiling_secs > sa.ceiling_secs * 1.15); // a gains b's share: 99 blocks, 2.60 s
    wait_until(|| a.stats().held_secs >= a.stats().ceiling_secs - 0.1);
    assert!(a.stats().held_secs > sa.held_secs * 1.15);
    assert!(b.stats().held_secs <= 0.3);
    b.set_mode(Mode::Live);
    wait_until(|| (a.stats().ceiling_secs - b.stats().ceiling_secs).abs() < 0.15);
    assert_eq!(c1.starts(), 1);
    assert_eq!(c2.starts(), 1);
    // Ring starvation is not the point of this test (source_sim checks it);
    // the sim reports no lag gaps, so any device gap here is a real fault.
    assert_eq!(a.stats().device_gaps + b.stats().device_gaps, 0);
}
