//! The one FFT thread per source: it walks the ring a hop at a time, caches
//! each block's bins for the channelizer and averages Blackman-Harris power
//! derived from those same bins into waterfall rows.
//!
//! Block `b` is the `N`-sample window starting at `(b − 1)·L` (`L = N/2`), so
//! it is complete once the head reaches `(b + 1)·L`. The thread polls the
//! head with a 1 ms park: it is the ring's only poller, a block takes about
//! 3.3 ms at 10 MS/s, and the write path stays free of any waker.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dsp::channelizer::{FFT_SIZE, HOP};
use dsp::spectrum::{SharedFft, bh_power};
use parking_lot::{Mutex, MutexGuard};
use sdr_core::Complex32;

use crate::budget::SourceId;
use crate::cache::SpectrumCache;
use crate::rows::{Row, RowHistory};
use crate::segments::Segment;
use crate::source::{ReadError, Source};

/// How long the thread parks when the next block is not complete yet.
const POLL: Duration = Duration::from_millis(1);
/// Weight of the newest block in [`StageStats::block_micros`].
const EMA_ALPHA: f64 = 0.05;

#[derive(Debug, Clone)]
pub struct StageConfig {
    pub fft_size: usize,
    /// Blocks of bins the [`SpectrumCache`] keeps.
    pub cache_blocks: usize,
    /// Seconds of blocks averaged into one waterfall row.
    pub row_period_secs: f64,
    /// Rows the [`RowHistory`] keeps.
    pub row_history: usize,
}

impl Default for StageConfig {
    fn default() -> Self {
        StageConfig { fft_size: FFT_SIZE, cache_blocks: 32, row_period_secs: 0.05, row_history: 64 }
    }
}

/// Told about each block once it is in the cache.
pub trait BlockSink: Send + Sync {
    fn on_block(&self, source: SourceId, block: u64);
}

#[derive(Debug, Clone, Default)]
pub struct StageStats {
    /// Blocks transformed and cached.
    pub blocks: u64,
    /// Ring refusals: one per jump after an overwritten or lost block (the
    /// jump lands at the live edge, however many blocks it passes over), and
    /// one per block skipped over a gap or a segment boundary.
    pub skipped_blocks: u64,
    /// EMA (α = 0.05) of the FFT + rows time per block.
    pub block_micros: f64,
    /// The last block inserted into the cache.
    pub latest_block: Option<u64>,
    /// How far the stage trails the source: `(head − (latest_block + 1)·L) / fs`
    /// seconds at the block's sample rate, 0 when caught up. Updated per block.
    pub behind_secs: f64,
    /// Blocks the recompute LRU holds ([`Stage::bins`] fills it, [`Stage::trim_recompute`] empties it).
    pub recompute_held: usize,
}

/// What the stage and its thread share.
struct Shared {
    source: Arc<Source>,
    cache: Arc<SpectrumCache>,
    rows: Arc<RowHistory>,
    stats: Mutex<StageStats>,
    /// Taken by [`Stage::bins`] and [`Stage::trim_recompute`] callers (bus
    /// workers, outside the scheduler lock); the stage thread only
    /// `try_lock`s it, to trim.
    recompute: Mutex<Recompute>,
    stop: AtomicBool,
}

/// [`Stage::bins`]' path for blocks that have left the cache: one FFT plan,
/// one input buffer (both made on first use and kept) and the last few
/// recomputed blocks, shared by every subscription of the source. An entry
/// idle for a second is dropped when the stage publishes a row.
#[derive(Default)]
struct Recompute {
    fft: Option<SharedFft>,
    input: Vec<Complex32>,
    /// Oldest first; each entry carries when it was last used.
    lru: VecDeque<(u64, Segment, Arc<[Complex32]>, Instant)>,
}

/// How long a recomputed block may sit unused before the stage thread drops it.
const RECOMPUTE_IDLE: Duration = Duration::from_secs(1);

impl Shared {
    /// Drops the LRU entries unused for `idle`; `r` is the locked recompute state.
    fn trim(&self, r: &mut Recompute, idle: Duration) {
        let now = Instant::now();
        let before = r.lru.len();
        r.lru.retain(|e| now.duration_since(e.3) < idle);
        if r.lru.len() != before {
            self.stats.lock().recompute_held = r.lru.len();
        }
    }
}

pub struct Stage {
    shared: Arc<Shared>,
    fft_size: usize,
    hop: u64,
    /// Blocks the recompute LRU keeps.
    recompute_depth: usize,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Stage {
    /// Starts thread "fft-<source id>". `sink.on_block` is called after each
    /// block is cached. The bus is the only creator (`ChannelBus::attach`);
    /// apps reach a stage through `ChannelBus::stage`.
    pub(crate) fn start(source: Arc<Source>, cfg: StageConfig, sink: Arc<dyn BlockSink>) -> Arc<Stage> {
        let fft_size = cfg.fft_size;
        let hop = if fft_size == FFT_SIZE { HOP as u64 } else { (fft_size / 2) as u64 };
        let shared = Arc::new(Shared {
            cache: Arc::new(SpectrumCache::new(cfg.cache_blocks)),
            rows: Arc::new(RowHistory::new(cfg.row_history)),
            stats: Mutex::new(StageStats::default()),
            recompute: Mutex::new(Recompute::default()),
            stop: AtomicBool::new(false),
            source,
        });
        let name = format!("fft-{}", shared.source.id().0);
        let worker = Worker { shared: shared.clone(), sink, fft_size, hop, row_period_secs: cfg.row_period_secs };
        let handle = std::thread::Builder::new().name(name).spawn(move || worker.run()).expect("spawn the FFT stage thread");
        // The recompute LRU is as deep as the cache: same-priority subscriptions drift up to a cache-depth apart while they recover.
        let recompute_depth = cfg.cache_blocks.max(1);
        Arc::new(Stage { shared, fft_size, hop, recompute_depth, thread: Mutex::new(Some(handle)) })
    }

    pub fn source(&self) -> &Arc<Source> {
        &self.shared.source
    }

    pub fn cache(&self) -> &Arc<SpectrumCache> {
        &self.shared.cache
    }

    pub fn rows(&self) -> &Arc<RowHistory> {
        &self.shared.rows
    }

    /// Samples between consecutive blocks (`L`).
    pub fn hop(&self) -> u64 {
        self.hop
    }

    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    /// The last block inserted into the cache.
    pub fn latest_block(&self) -> Option<u64> {
        self.shared.stats.lock().latest_block
    }

    pub fn stats(&self) -> StageStats {
        self.shared.stats.lock().clone()
    }

    /// Block `block`'s segment and bins (`block ≥ 1`), and whether this call
    /// recomputed them: from the cache; else from the last `cache_blocks`
    /// blocks recomputed (kept only once something fell behind the cache); else read from the ring
    /// (`[(b−1)·L, (b+1)·L)`) and transformed once under the recompute
    /// lock, so subscriptions that miss the cache together wait for one
    /// FFT per block instead of each computing it. A ring error is returned
    /// as is and nothing is kept. Block 0's window would start before sample
    /// 0, so it is always `Err(Overwritten)`.
    pub fn bins(&self, block: u64) -> Result<(Segment, Arc<[Complex32]>, bool), ReadError> {
        if block == 0 {
            return Err(ReadError::Overwritten);
        }
        if let Some((seg, bins)) = self.shared.cache.get(block) {
            return Ok((seg, bins, false));
        }
        let mut guard = self.shared.recompute.lock();
        let r = &mut *guard;
        if let Some((_, seg, bins, used)) = r.lru.iter_mut().rev().find(|c| c.0 == block) {
            *used = Instant::now();
            return Ok((seg.clone(), bins.clone(), false));
        }
        let n = self.fft_size;
        if r.input.len() != n {
            r.input = vec![Complex32::default(); n];
        }
        let seg = self.shared.source.read((block - 1) * self.hop, &mut r.input)?;
        let fft = r.fft.get_or_insert_with(|| SharedFft::new(n));
        let mut bins: Arc<[Complex32]> = std::iter::repeat_n(Complex32::default(), n).collect();
        fft.forward(&r.input, Arc::get_mut(&mut bins).expect("a fresh Arc is unique"));
        r.lru.push_back((block, seg.clone(), bins.clone(), Instant::now()));
        while r.lru.len() > self.recompute_depth {
            r.lru.pop_front();
        }
        self.shared.stats.lock().recompute_held = r.lru.len();
        // Hand the lock to the callers that waited for this block before
        // this one can come back for the next: a barging caller would run
        // ahead of the rest by more than the LRU and make them recompute.
        MutexGuard::unlock_fair(guard);
        Ok((seg, bins, true))
    }

    /// Drops the recomputed blocks unused for `idle`. The stage thread calls
    /// it with one second each time it publishes a row.
    pub fn trim_recompute(&self, idle: Duration) {
        let mut guard = self.shared.recompute.lock();
        self.shared.trim(&mut guard, idle);
    }

    /// Stop the thread and join it (unless called on the thread itself). Idempotent.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let handle = self.thread.lock().take();
        if let Some(h) = handle {
            if h.thread().id() == std::thread::current().id() {
                return;
            }
            h.thread().unpark();
            if h.join().is_err() {
                log::warn!("fft stage: the stage thread panicked");
            }
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A row being accumulated: the sum of its blocks' power.
struct RowAcc {
    first_sample: u64,
    end_sample: u64,
    blocks: u32,
    segment: Segment,
    sum: Vec<f32>,
}

/// The stage thread's state.
struct Worker {
    shared: Arc<Shared>,
    sink: Arc<dyn BlockSink>,
    fft_size: usize,
    hop: u64,
    row_period_secs: f64,
}

impl Worker {
    fn run(self) {
        let (n, l) = (self.fft_size, self.hop);
        let source = &self.shared.source;
        let id = source.id();
        let mut fft = SharedFft::new(n);
        let mut buf = vec![Complex32::default(); n];
        let mut power = vec![0.0f32; n];
        let mut row: Option<RowAcc> = None;
        let mut b = (source.oldest() / l + 1).max(1);
        while !self.shared.stop.load(Ordering::SeqCst) {
            let head = source.head();
            if head < (b + 1) * l {
                std::thread::park_timeout(POLL);
                continue;
            }
            let first = (b - 1) * l;
            match source.read(first, &mut buf) {
                Ok(seg) => {
                    let t0 = Instant::now();
                    let mut bins: Arc<[Complex32]> = std::iter::repeat_n(Complex32::default(), n).collect();
                    fft.forward(&buf, Arc::get_mut(&mut bins).expect("a fresh Arc is unique"));
                    bh_power(&bins, &mut power);
                    self.shared.cache.insert(b, seg.clone(), bins);
                    self.accumulate(&mut row, first, first + n as u64, &seg, &power);
                    let micros = t0.elapsed().as_secs_f64() * 1e6;
                    {
                        let mut s = self.shared.stats.lock();
                        s.block_micros = if s.blocks == 0 { micros } else { s.block_micros + EMA_ALPHA * (micros - s.block_micros) };
                        s.blocks += 1;
                        s.latest_block = Some(b);
                        s.behind_secs = head.saturating_sub((b + 1) * l) as f64 / seg.sample_rate;
                    }
                    self.sink.on_block(id, b);
                    b += 1;
                }
                Err(ReadError::Future) => std::thread::park_timeout(POLL),
                Err(ReadError::Overwritten | ReadError::Lost) => {
                    // A row never spans the samples jumped over.
                    if let Some(r) = row.take() {
                        self.publish(r);
                    }
                    b = next_block_after_jump(b, source.oldest(), source.head(), l);
                    self.shared.stats.lock().skipped_blocks += 1;
                }
                Err(ReadError::Gap | ReadError::SpansSegments) => {
                    b += 1;
                    self.shared.stats.lock().skipped_blocks += 1;
                }
            }
        }
    }

    /// Add one block's power to the row in progress, publishing the partial
    /// row first if the block belongs to another segment, and the row itself
    /// once it holds `ceil(row_period·fs/L)` blocks.
    fn accumulate(&self, row: &mut Option<RowAcc>, first: u64, end: u64, seg: &Segment, power: &[f32]) {
        if let Some(r) = row.take_if(|r| r.segment.id != seg.id) {
            self.publish(r);
        }
        let want = self.blocks_per_row(seg.sample_rate);
        let acc = row.get_or_insert_with(|| RowAcc {
            first_sample: first,
            end_sample: end,
            blocks: 0,
            segment: seg.clone(),
            sum: vec![0.0; power.len()],
        });
        for (s, p) in acc.sum.iter_mut().zip(power) {
            *s += p;
        }
        acc.blocks += 1;
        acc.end_sample = end;
        if acc.blocks >= want
            && let Some(r) = row.take()
        {
            self.publish(r);
        }
    }

    /// `ceil(row_period·fs/L)`, at least 1.
    fn blocks_per_row(&self, rate: f64) -> u32 {
        let blocks = (self.row_period_secs * rate / self.hop as f64).ceil();
        if blocks.is_finite() && blocks >= 1.0 { blocks as u32 } else { 1 }
    }

    /// Push the row's average power.
    fn publish(&self, acc: RowAcc) {
        // Never wait for a `bins` caller's FFT: the next row trims instead.
        if let Some(mut r) = self.shared.recompute.try_lock() {
            self.shared.trim(&mut r, RECOMPUTE_IDLE);
        }
        let k = 1.0 / acc.blocks as f32;
        let power = acc.sum.into_iter().map(|s| s * k).collect();
        self.shared.rows.push(Row {
            first_sample: acc.first_sample,
            end_sample: acc.end_sample,
            blocks: acc.blocks,
            segment: acc.segment,
            power,
        });
    }
}

/// Where to resume after the ring refused block `b` as overwritten or lost:
/// at the live edge, `head/L − 1` (the newest block whose input is complete
/// at `head`), or at least just above `oldest` (`oldest/L + 2`), and never at
/// or before `b`. A block the OS discarded inside the held range reads `Lost`
/// while `oldest` is still below it, and a formula without `b + 1` would
/// rewind into blocks already cached, fed to rows and passed to the sink.
fn next_block_after_jump(b: u64, oldest: u64, head: u64, hop: u64) -> u64 {
    (b + 1).max(oldest / hop + 2).max((head / hop).saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use backend_sim::{SimConfig, SimDevice};

    use super::{BlockSink, Stage, StageConfig, next_block_after_jump};
    use crate::budget::{BudgetConfig, Mode, RingBudget, SourceId};
    use crate::ring::keeper::Keeper;
    use crate::source::{ReadError, Source, SourceConfig};

    struct NoSink;
    impl BlockSink for NoSink {
        fn on_block(&self, _: SourceId, _: u64) {}
    }

    /// A stage on an unpaced noise sim with a one-block cache, so any older block misses it.
    fn stage() -> (Arc<Stage>, Arc<Source>) {
        let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
        let (d, _c) =
            SimDevice::with_config(SimConfig { emitters: vec![], rate: 2e6, paced: false, report_lag_gaps: false, ..Default::default() });
        let src = Arc::new(Source::start(Box::new(d), SourceConfig::default(), Mode::Live, &keeper).unwrap());
        let cfg = StageConfig { cache_blocks: 1, row_period_secs: 0.01, ..Default::default() };
        (Stage::start(src.clone(), cfg, Arc::new(NoSink)), src)
    }

    fn wait(what: &str, f: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(20), "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn bins_of_block_zero_is_an_error() {
        let (st, _src) = stage();
        assert!(matches!(st.bins(0), Err(ReadError::Overwritten)));
    }

    #[test]
    fn a_recomputed_block_is_dropped_once_idle() {
        let (st, _src) = stage();
        wait("the stage never produced blocks", || st.latest_block().is_some_and(|b| b > 8));
        // A block just behind the one-block cache misses it and is recomputed from the ring.
        let (_, _, recomputed) = st.bins(st.latest_block().unwrap() - 4).unwrap();
        assert!(recomputed);
        assert_eq!(st.stats().recompute_held, 1);
        // Rows keep being published; the first one after the entry has idled for a second trims it.
        std::thread::sleep(Duration::from_millis(1200));
        wait("the idle entry was never trimmed", || st.stats().recompute_held == 0);
    }

    #[test]
    fn trim_recompute_keeps_entries_used_within_the_idle_time() {
        let (st, _src) = stage();
        wait("the stage never produced blocks", || st.latest_block().is_some_and(|b| b > 8));
        st.bins(st.latest_block().unwrap() - 4).unwrap();
        st.trim_recompute(Duration::from_secs(60));
        assert_eq!(st.stats().recompute_held, 1);
        st.trim_recompute(Duration::ZERO);
        assert_eq!(st.stats().recompute_held, 0);
    }

    #[test]
    fn a_jump_lands_at_the_live_edge_and_never_goes_backwards() {
        const L: u64 = 32_768;
        // Overwritten: `oldest` has moved past `b` and the head is far ahead: jump to the head.
        assert_eq!(next_block_after_jump(10, 100 * L + 5, 400 * L + 7, L), 399);
        // The head just above `oldest`: no further than just above `oldest`.
        assert_eq!(next_block_after_jump(10, 100 * L + 5, 101 * L, L), 102);
        // Lost above `oldest` with the head close by: the formula alone would rewind.
        assert_eq!(next_block_after_jump(50, 5 * L, 51 * L, L), 51);
        assert_eq!(next_block_after_jump(50, 48 * L, 50 * L, L), 51);
        assert_eq!(next_block_after_jump(50, 49 * L, 52 * L, L), 51);
        assert_eq!(next_block_after_jump(50, 50 * L, 52 * L, L), 52);
        // Lost with the head far ahead: to the live edge.
        assert_eq!(next_block_after_jump(50, 5 * L, 90 * L + 3, L), 89);
        // Head 0 (nothing written yet) never underflows.
        assert_eq!(next_block_after_jump(1, 0, 0, L), 2);
    }
}
