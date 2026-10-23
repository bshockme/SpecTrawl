mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{cfg, cw_sim, sim, wait_until};
use engine::{BudgetConfig, BusConfig, ChannelBus, Keeper, Mode, RingBudget, Source, Stage};

/// The bus is the only creator of stages: attach `src` to a one-worker bus.
fn stage_of(src: &Arc<Source>) -> (Arc<ChannelBus>, Arc<Stage>) {
    let bus = ChannelBus::new(BusConfig { workers: 1, ..Default::default() });
    let stage = bus.attach(src.clone());
    (bus, stage)
}

#[test]
fn rows_show_the_carrier_in_the_right_bin_and_blocks_reach_the_sink() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
    let (d, _c) = cw_sim(10e6, 855.0e6, -30.0); // centre 855.25 MHz: offset −250 kHz
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let (_bus, stage) = stage_of(&src);
    wait_until(|| stage.rows().len() >= 10);
    let row = stage.rows().latest().unwrap();
    assert_eq!(row.power.len(), 65_536);
    let peak = row.power.iter().enumerate().fold((0, f32::MIN), |m, (i, &p)| if p > m.1 { (i, p) } else { m }).0;
    let expected = 65_536 / 2 - 1638; // −250e3 / 152.5879 = −1638.4
    assert!((peak as i64 - expected as i64).abs() <= 2, "peak at {peak}, expected {expected}");
    let mut sorted = row.power.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    assert!(row.power[peak] > median * 1e2, "carrier not 20 dB above the floor");
    assert!(row.blocks >= 15 && row.blocks <= 16, "{}", row.blocks); // 50 ms at 305 blocks/s
    stage.stop(); // the comparison below needs a thread that has finished
    let s = stage.stats();
    assert_eq!(stage.cache().latest(), s.latest_block);
    assert_eq!(s.skipped_blocks, 0);
    assert!(stage.cache().get(s.latest_block.unwrap()).is_some());
    assert!(stage.cache().get(s.latest_block.unwrap() - 40).is_none(), "cache deeper than configured");
    // A sanity bound only (CI runners have 4 vCPUs); `channels_200` prints the benchmark number.
    assert!(s.block_micros > 0.0 && s.block_micros < 10_000.0, "{} µs per block", s.block_micros);
}

#[test]
fn a_stage_that_falls_behind_skips_to_the_live_edge() {
    // A paced 100 MS/s source outruns the FFT (~1 ms per 32,768-sample hop,
    // about 33 MS/s) while the keeper still provisions for it, so the ring
    // holds a full second: a jump to its tail would leave the stage ~1 s
    // behind; a jump to the live edge leaves it near 0.
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 512 << 20, sweep_secs: 0.1 }));
    let (d, _c) = sim(100e6, true);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let (_bus, stage) = stage_of(&src);
    wait_until(|| stage.stats().skipped_blocks > 0 && stage.stats().blocks > 50);
    let a = stage.stats().latest_block.unwrap();
    wait_until(|| stage.stats().latest_block.unwrap() > a + 20); // still publishing after skipping
    // Within a second of a skip the stage is near the live edge, and still inside the ring.
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut s = stage.stats();
    while s.behind_secs >= 0.5 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        s = stage.stats();
    }
    let (l, oldest) = (stage.hop(), src.oldest());
    eprintln!(
        "behind {:.3} s, skipped {}, latest block {} (sample {}), oldest {oldest}, head {}",
        s.behind_secs,
        s.skipped_blocks,
        s.latest_block.unwrap(),
        s.latest_block.unwrap() * l,
        src.head()
    );
    assert!(s.behind_secs < 0.5, "{s:?}, {:?}", src.stats());
    assert!(s.latest_block.unwrap() * l >= oldest, "{s:?}, oldest {oldest}");
    stage.stop();
}

#[test]
fn a_row_never_spans_a_retune() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, true);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let (_bus, stage) = stage_of(&src);
    wait_until(|| stage.rows().len() >= 5);
    src.set_center(860e6).unwrap();
    wait_until(|| stage.rows().latest().is_some_and(|r| r.segment.center_hz == 860e6));
    let rows = stage.rows().since(0);
    assert!(rows.iter().any(|r| r.segment.center_hz == 855.25e6), "no row from before the retune");
    for r in &rows {
        let (first, last) = (src.segment_at(r.first_sample), src.segment_at(r.end_sample - 1));
        assert_eq!(first.id, last.id, "row {}..{} spans {:?} and {:?}", r.first_sample, r.end_sample, first, last);
        assert_eq!(r.segment.id, first.id);
    }
    stage.stop();
}
