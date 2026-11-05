mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{cfg, cw_sim, sim, wait_until};
use engine::{BudgetConfig, BusConfig, ChannelBus, Keeper, Mode, RingBudget, Row, RowSink, Source, SourceId, Stage};

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
    assert_eq!(row.blocks, 7, "{}", row.blocks); // 20 ms at 305 blocks/s, ceil'd
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
    // The stage falls a few blocks behind on a 100 MS/s source (the FFT does
    // about 33 MS/s) and is overwritten within a fraction of a second. The
    // jump must land within 0.1 s of the head, not further: a jump to the
    // ring's tail would leave it about the ring's depth (~0.3 s) behind.
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.1, ceiling_bytes: 128 << 20, sweep_secs: 0.1 }));
    let (d, _c) = sim(100e6, true);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let (_bus, stage) = stage_of(&src);
    wait_until(|| stage.stats().skipped_blocks > 0 && stage.stats().blocks > 50);
    let a = stage.stats().latest_block.unwrap();
    wait_until(|| stage.stats().latest_block.unwrap() > a + 20); // still publishing after skipping
    // Within a second of a skip the stage is near the live edge, and still inside the ring.
    // Well under the ring's ~0.33 s, which a jump to the tail would leave: 0.1 s, not 0.5 s.
    const MAX_BEHIND: f64 = 0.1;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut s = stage.stats();
    while s.behind_secs >= MAX_BEHIND && Instant::now() < deadline {
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
    assert!(s.behind_secs < MAX_BEHIND, "{s:?}, {:?}", src.stats());
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

/// Collects what a row sink is told, and can be told to panic on its first row.
#[derive(Default)]
struct Rows {
    seen: std::sync::Mutex<Vec<(SourceId, Arc<Row>, String)>>,
    panic_first: bool,
}
impl RowSink for Rows {
    fn on_row(&self, source: SourceId, row: &Arc<Row>) {
        let thread = std::thread::current().name().unwrap_or("").to_string();
        let first = {
            let mut seen = self.seen.lock().unwrap();
            seen.push((source, row.clone(), thread));
            seen.len() == 1
        };
        if self.panic_first && first {
            panic!("a row sink that panics");
        }
    }
}

fn stage_with_sink(src: &Arc<Source>, sink: Arc<Rows>) -> (Arc<ChannelBus>, Arc<Stage>) {
    let bus = ChannelBus::new(BusConfig { workers: 1, row_sink: Some(sink), ..Default::default() });
    let stage = bus.attach(src.clone());
    (bus, stage)
}

#[test]
fn the_row_sink_gets_every_row_in_order_on_the_stage_thread() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, true);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let sink = Arc::new(Rows::default());
    let (_bus, stage) = stage_with_sink(&src, sink.clone());
    wait_until(|| sink.seen.lock().unwrap().len() >= 30);
    stage.stop();
    let seen = sink.seen.lock().unwrap();
    let held = stage.rows().since(0);
    for w in seen.windows(2) {
        assert_eq!(w[1].1.first_sample, w[0].1.end_sample - stage.fft_size() as u64 + stage.hop(), "rows out of order or with a hole");
    }
    for (id, row, thread) in seen.iter() {
        assert_eq!(*id, src.id());
        assert_eq!(thread, &format!("fft-{}", src.id().0), "not on the stage thread");
        if let Some(h) = held.iter().find(|h| h.first_sample == row.first_sample) {
            assert!(Arc::ptr_eq(h, row), "the sink got a copy, not the held row");
        }
    }
    // Rows every 20 ms of samples, ceil'd to whole blocks: 7 blocks of 32 768 at 10 MS/s.
    let r = &seen[seen.len() - 1].1;
    assert_eq!(r.blocks, 7);
}

#[test]
fn a_panicking_row_sink_does_not_stop_the_stage() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, true);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let sink = Arc::new(Rows { panic_first: true, ..Default::default() });
    let (_bus, stage) = stage_with_sink(&src, sink.clone());
    wait_until(|| sink.seen.lock().unwrap().len() >= 10);
    assert!(stage.rows().len() >= 10);
    stage.stop();
}
