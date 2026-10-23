//! The look-back IQ ring: a chain of fixed-size blocks reached through atomics.
//!
//! One writer (the device callback) appends samples by absolute index without
//! taking a lock; any thread reads by absolute index; a keeper thread
//! provisions blocks ahead of the writer (and, from Task 11, offers and retires
//! old ones). A read that races the writer or the keeper returns
//! `Err(Overwritten)` and the reader skips ahead.
//!
//! Memory ordering: the writer copies samples into block memory through raw
//! pointers and only then stores `head` with `Release`; readers load `head`
//! with `Acquire` and copy only below it, so every sample a reader copies was
//! fully written before the `head` it observed. The writer never writes below
//! `head`, so within a block the writer and readers touch disjoint ranges.
//! Blocks are published and retired through `ArcSwapOption` slots; a reader's
//! guard keeps a retired block's memory alive until its copy ends.

pub mod block;
pub mod volatile;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use arc_swap::ArcSwapOption;
use sdr_core::{Complex32, Samples};

use self::block::{Block, BlockState};
use self::volatile::{PAGE, VolatileMemory};

/// Default block size: 64 MiB, 16 Mi samples.
pub const DEFAULT_BLOCK_BYTES: usize = 64 << 20;
/// Number of block slots: 256 GiB at the default block size.
pub const MAX_SLOTS: usize = 4096;

#[derive(Clone, Debug)]
pub struct RingConfig {
    /// Bytes per block; a non-zero multiple of [`PAGE`] (and so of 4).
    pub block_bytes: usize,
}

impl Default for RingConfig {
    fn default() -> Self {
        RingConfig { block_bytes: DEFAULT_BLOCK_BYTES }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ReadError {
    /// Some of the range is no longer (or was never) held.
    Overwritten,
    /// The OS discarded an offered block that holds part of the range.
    Lost,
    /// Some of the range has not been written yet.
    Future,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WriteOutcome {
    /// Samples skipped because their block was not provisioned.
    pub starved: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RingStats {
    pub head: u64,
    pub oldest: u64,
    pub held_samples: u64,
    pub block_samples: u64,
    pub ceiling_blocks: u32,
    pub guaranteed_samples: u64,
    /// Live blocks holding written samples (blocks provisioned ahead of the head are not counted).
    pub blocks_live: u32,
    /// Offered blocks holding written samples.
    pub blocks_offered: u32,
    pub blocks_lost_total: u64,
    pub starved_total: u64,
}

pub struct IqRing {
    cfg: RingConfig,
    block_samples: u64,
    mem: Arc<dyn VolatileMemory>,
    slots: Box<[ArcSwapOption<Block>]>,
    /// Absolute index of the next sample to be written. Stored by the writer only.
    head: AtomicU64,
    /// Oldest sample that might still be readable. Monotone (`fetch_max`).
    oldest: AtomicU64,
    /// The block the writer needs next; the keeper provisions from here.
    wanted_block: AtomicU64,
    ceiling_blocks: AtomicU32,
    guaranteed_samples: AtomicU64,
    blocks_lost_total: AtomicU64,
    starved_total: AtomicU64,
}

impl IqRing {
    pub fn new(cfg: RingConfig, mem: Arc<dyn VolatileMemory>) -> Arc<IqRing> {
        IqRing::with_slots(cfg, mem, MAX_SLOTS)
    }

    /// A ring with `slots` block slots instead of [`MAX_SLOTS`] (tests use a
    /// handful to exercise slot reuse).
    pub(crate) fn with_slots(cfg: RingConfig, mem: Arc<dyn VolatileMemory>, slots: usize) -> Arc<IqRing> {
        assert!(
            cfg.block_bytes != 0 && cfg.block_bytes.is_multiple_of(PAGE) && cfg.block_bytes.is_multiple_of(4),
            "RingConfig::block_bytes must be a non-zero multiple of {PAGE} bytes, got {}",
            cfg.block_bytes
        );
        assert!(slots >= 2, "a ring needs at least two slots");
        Arc::new(IqRing {
            block_samples: (cfg.block_bytes / 4) as u64,
            cfg,
            mem,
            slots: (0..slots).map(|_| ArcSwapOption::empty()).collect::<Vec<_>>().into_boxed_slice(),
            head: AtomicU64::new(0),
            oldest: AtomicU64::new(0),
            wanted_block: AtomicU64::new(0),
            ceiling_blocks: AtomicU32::new(slots as u32),
            guaranteed_samples: AtomicU64::new(0),
            blocks_lost_total: AtomicU64::new(0),
            starved_total: AtomicU64::new(0),
        })
    }

    pub fn block_samples(&self) -> u64 {
        self.block_samples
    }

    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// Oldest sample that might still be readable.
    pub fn oldest(&self) -> u64 {
        self.oldest.load(Ordering::Acquire)
    }

    fn slot(&self, block: u64) -> &ArcSwapOption<Block> {
        &self.slots[(block % self.slots.len() as u64) as usize]
    }

    /// Append `samples` starting at absolute index `first_sample`. Single
    /// writer; never blocks, never allocates. Samples already behind `head`
    /// are dropped; a `first_sample` past `head` makes `head` jump (no fill).
    /// Samples whose block was not provisioned are skipped and counted as
    /// starved; `head` still advances past them, and `oldest` moves up to the
    /// new `head` because the held range is no longer continuous.
    pub fn write(&self, first_sample: u64, samples: Samples<'_>) -> WriteOutcome {
        let bs = self.block_samples;
        // Only this thread stores `head`.
        let head = self.head.load(Ordering::Relaxed);
        let end = first_sample + samples.len() as u64;
        if end <= head {
            return WriteOutcome::default();
        }
        let mut pos = first_sample.max(head);
        let mut starved = 0;
        while pos < end {
            let b = pos / bs;
            let off = (pos % bs) as usize;
            let run = (end - pos).min(bs - off as u64) as usize;
            let src = (pos - first_sample) as usize;
            let guard = self.slot(b).load();
            match &*guard {
                Some(block) if block.index == b => {
                    // SAFETY: `off + run <= block_samples`, so the range lies
                    // inside the block's region, which the guard keeps
                    // allocated. Every sample in it is at or above `head`
                    // (`pos >= head`), and readers only copy below the `head`
                    // they loaded, so no reader touches this range while we
                    // write it; this single writer is the only thread that
                    // writes block memory.
                    let dst = unsafe { std::slice::from_raw_parts_mut(block.samples().add(off), run) };
                    convert(dst, samples, src);
                }
                _ => starved += run as u64,
            }
            pos += run as u64;
        }
        if starved > 0 {
            self.starved_total.fetch_add(starved, Ordering::Relaxed);
            // Store `oldest` before `head`: a reader that sees the new head
            // also sees that nothing below it is held, so a block the keeper
            // provisions later for the skipped range never reads as data.
            self.oldest.fetch_max(end, Ordering::AcqRel);
        }
        // The writer needs the block holding the next sample (on starvation
        // that is at or past the skipped block, which is no longer worth filling).
        self.wanted_block.store(end / bs, Ordering::Release);
        self.head.store(end, Ordering::Release);
        WriteOutcome { starved }
    }

    /// Copy the stored i16 samples `start .. start + out.len()` into `out`.
    pub fn read_raw(&self, start: u64, out: &mut [[i16; 2]]) -> Result<(), ReadError> {
        self.read_with(start, out.len(), |at, src| out[at..at + src.len()].copy_from_slice(src))
    }

    /// Copy `start .. start + out.len()` scaled to ±1.0 (by 1/32768).
    pub fn read(&self, start: u64, out: &mut [Complex32]) -> Result<(), ReadError> {
        const K: f32 = 1.0 / 32768.0;
        self.read_with(start, out.len(), |at, src| {
            for (d, s) in out[at..at + src.len()].iter_mut().zip(src) {
                *d = Complex32::new(s[0] as f32 * K, s[1] as f32 * K);
            }
        })
    }

    /// Walk the blocks covering `start .. start + len`, handing each run to
    /// `sink(offset_in_output, samples)`. Lock-free.
    fn read_with(&self, start: u64, len: usize, mut sink: impl FnMut(usize, &[[i16; 2]])) -> Result<(), ReadError> {
        let end = start + len as u64;
        // Acquire pairs with the writer's Release store: every sample below
        // `head` was fully written before we copy it.
        let head = self.head.load(Ordering::Acquire);
        if end > head {
            return Err(ReadError::Future);
        }
        if start < self.oldest.load(Ordering::Acquire) {
            return Err(ReadError::Overwritten);
        }
        let bs = self.block_samples;
        let mut pos = start;
        while pos < end {
            let b = pos / bs;
            let off = (pos % bs) as usize;
            let run = (end - pos).min(bs - off as u64) as usize;
            // The guard keeps the block (and its memory) alive until the copy
            // ends, even if the keeper retires or replaces it meanwhile.
            let guard = self.slot(b).load();
            let Some(block) = &*guard else { return Err(ReadError::Overwritten) };
            if block.index != b {
                return Err(ReadError::Overwritten);
            }
            let _reading = ReaderGuard::enter(block);
            self.make_live(block)?;
            // SAFETY: `off + run <= block_samples`, inside the region, which
            // the guard keeps allocated. Every sample in the range is below
            // the `head` loaded above (Acquire), so the writer finished it
            // before publishing that `head` and never writes below `head`
            // again: the writer and this reader touch disjoint ranges. The
            // block is Live (reclaimed if it had been offered), and the
            // `readers` count stops the keeper offering it during the copy.
            let src = unsafe { std::slice::from_raw_parts(block.samples().add(off).cast_const(), run) };
            sink((pos - start) as usize, src);
            pos += run as u64;
        }
        // The samples were valid, but the caller must know the range is gone.
        if self.oldest.load(Ordering::Acquire) > start {
            return Err(ReadError::Overwritten);
        }
        Ok(())
    }

    /// Bring an offered block back before copying from it. The caller holds a
    /// [`ReaderGuard`] on `block`.
    fn make_live(&self, block: &Block) -> Result<(), ReadError> {
        loop {
            match BlockState::from_u8(block.state.load(Ordering::SeqCst)) {
                BlockState::Live => return Ok(()),
                BlockState::Lost => return Err(ReadError::Lost),
                // Another reader is reclaiming it; the result is moments away.
                BlockState::Reclaiming => std::hint::spin_loop(),
                BlockState::Offered => {
                    let won = block.state.compare_exchange(
                        BlockState::Offered as u8,
                        BlockState::Reclaiming as u8,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    if won.is_ok() {
                        if block.mem().reclaim(block.region()) {
                            block.state.store(BlockState::Live as u8, Ordering::SeqCst);
                            return Ok(());
                        }
                        block.state.store(BlockState::Lost as u8, Ordering::SeqCst);
                        self.blocks_lost_total.fetch_add(1, Ordering::Relaxed);
                        return Err(ReadError::Lost);
                    }
                }
            }
        }
    }

    /// Keeper side: make sure blocks `wanted_block ..= wanted_block + ahead`
    /// exist (`ahead` is capped at one less than the slot count so the
    /// writer's block is never replaced). A slot that holds an older block is
    /// overwritten and `oldest` moves past that block. Returns the number of
    /// blocks allocated.
    pub fn provision(&self, ahead: u32) -> u32 {
        let bs = self.block_samples;
        let ahead = u64::from(ahead).min(self.slots.len() as u64 - 1);
        let first = self.wanted_block.load(Ordering::Acquire);
        let mut allocated = 0;
        for b in first..=first + ahead {
            let slot = self.slot(b);
            let old_index = slot.load().as_ref().map(|old| old.index);
            match old_index {
                Some(i) if i >= b => continue,
                // Before the slot changes hands: a reader that then finds the
                // new block fails the index check, and one still copying from
                // the old block re-checks `oldest` when it is done.
                Some(i) => {
                    self.oldest.fetch_max((i + 1) * bs, Ordering::AcqRel);
                }
                None => {}
            }
            let Some(region) = self.mem.alloc(self.cfg.block_bytes) else {
                log::warn!("ring: could not allocate a {} byte block", self.cfg.block_bytes);
                break;
            };
            slot.store(Some(Arc::new(Block::new(b, region, self.mem.clone()))));
            allocated += 1;
        }
        allocated
    }

    pub fn set_ceiling_blocks(&self, n: u32) {
        self.ceiling_blocks.store(n, Ordering::Relaxed);
    }

    pub fn set_guaranteed_samples(&self, n: u64) {
        self.guaranteed_samples.store(n, Ordering::Relaxed);
    }

    pub fn stats(&self) -> RingStats {
        let head = self.head();
        let oldest = self.oldest();
        // Blocks below this index hold written samples; later ones are
        // provisioned ahead and not counted.
        let written_blocks = head.div_ceil(self.block_samples);
        let (mut live, mut offered) = (0, 0);
        for slot in self.slots.iter() {
            if let Some(block) = &*slot.load()
                && block.index < written_blocks
            {
                match BlockState::from_u8(block.state.load(Ordering::Relaxed)) {
                    BlockState::Live | BlockState::Reclaiming => live += 1,
                    BlockState::Offered => offered += 1,
                    BlockState::Lost => {}
                }
            }
        }
        RingStats {
            head,
            oldest,
            held_samples: head.saturating_sub(oldest),
            block_samples: self.block_samples,
            ceiling_blocks: self.ceiling_blocks.load(Ordering::Relaxed),
            guaranteed_samples: self.guaranteed_samples.load(Ordering::Relaxed),
            blocks_live: live,
            blocks_offered: offered,
            blocks_lost_total: self.blocks_lost_total.load(Ordering::Relaxed),
            starved_total: self.starved_total.load(Ordering::Relaxed),
        }
    }
}

/// Counts a reader in `Block::readers` for as long as it lives.
struct ReaderGuard<'a>(&'a Block);

impl<'a> ReaderGuard<'a> {
    fn enter(block: &'a Block) -> ReaderGuard<'a> {
        // SeqCst with the state load that follows: the keeper (Task 11) must
        // mark a block Offered before checking `readers`, so either it sees
        // this reader or this reader sees `Offered` and reclaims.
        block.readers.fetch_add(1, Ordering::SeqCst);
        ReaderGuard(block)
    }
}

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Convert `src[from..from + dst.len()]` to stored i16 IQ, as the old ring did:
/// Ci16 copied, Ci8 shifted left by 8, Cf32 scaled by 32767 and clamped.
fn convert(dst: &mut [[i16; 2]], src: Samples<'_>, from: usize) {
    let n = dst.len();
    match src {
        Samples::Ci16(s) => dst.copy_from_slice(&s[from..from + n]),
        Samples::Ci8(s) => {
            for (d, x) in dst.iter_mut().zip(&s[from..from + n]) {
                *d = [(x[0] as i16) << 8, (x[1] as i16) << 8];
            }
        }
        Samples::Cf32(s) => {
            for (d, x) in dst.iter_mut().zip(&s[from..from + n]) {
                *d = [to_i16(x.re), to_i16(x.im)];
            }
        }
    }
}

fn to_i16(v: f32) -> i16 {
    (v * 32767.0).clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU64, Ordering};

    use sdr_core::{Complex32, Samples};

    use super::volatile::FakeVolatile;
    use super::*;

    fn ramp(start: u64, n: usize) -> Vec<[i16; 2]> {
        (0..n).map(|i| [((start + i as u64) % 30000) as i16, 7]).collect()
    }

    #[test]
    fn writes_across_blocks_and_reads_back_by_absolute_index() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        assert_eq!(r.provision(3), 4); // blocks 0..=3
        assert_eq!(r.write(0, Samples::Ci16(&ramp(0, 1500))).starved, 0);
        assert_eq!(r.write(1500, Samples::Ci16(&ramp(1500, 1500))).starved, 0);
        assert_eq!(r.head(), 3000);
        let mut out = vec![[0i16; 2]; 2048];
        r.read_raw(900, &mut out).unwrap(); // spans blocks 0, 1, 2
        assert_eq!(out[0], [900, 7]);
        assert_eq!(out[2047], [2947, 7]);
        assert_eq!(r.read_raw(2999, &mut out[..2]), Err(ReadError::Future));
        let mut f = vec![Complex32::default(); 4];
        r.read(100, &mut f).unwrap();
        assert!((f[0].re - 100.0 / 32768.0).abs() < 1e-7);
    }

    #[test]
    fn out_of_order_block_is_ignored() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        r.provision(2);
        r.write(0, Samples::Ci16(&ramp(0, 1000)));
        r.write(500, Samples::Ci16(&vec![[-1, -1]; 700])); // overlaps 500..1000, adds 1000..1200
        assert_eq!(r.head(), 1200);
        let mut out = vec![[0i16; 2]; 2];
        r.read_raw(999, &mut out).unwrap();
        assert_eq!(out, [[999, 7], [-1, -1]]);
    }

    #[test]
    fn large_gap_starves_until_provisioned() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        r.provision(1); // blocks 0, 1
        r.write(0, Samples::Ci16(&ramp(0, 1000)));
        let w = r.write(10_000, Samples::Ci16(&ramp(10_000, 100))); // block 9 does not exist
        assert_eq!(w.starved, 100);
        assert_eq!(r.head(), 10_100);
        assert_eq!(r.stats().starved_total, 100);
        r.provision(1); // keeper catches up: blocks 9, 10
        assert_eq!(r.write(10_100, Samples::Ci16(&ramp(10_100, 100))).starved, 0);
        let mut out = vec![[0i16; 2]; 1];
        assert_eq!(r.read_raw(10_050, &mut out), Err(ReadError::Overwritten)); // never stored
        r.read_raw(10_150, &mut out).unwrap();
    }

    #[test]
    fn converts_ci8_and_cf32_like_the_old_ring() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        r.provision(0);
        r.write(0, Samples::Ci8(&[[1, -2]]));
        r.write(1, Samples::Cf32(&[Complex32::new(0.5, -2.0)]));
        let mut out = vec![[0i16; 2]; 2];
        r.read_raw(0, &mut out).unwrap();
        assert_eq!(out, [[256, -512], [16383, -32768]]);
    }

    #[test]
    fn provisioning_over_an_older_block_moves_oldest_past_it() {
        let r = IqRing::with_slots(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()), 8);
        let bs = r.block_samples();
        for k in 0..10 {
            assert_eq!(r.provision(0), 1);
            assert_eq!(r.write(k * bs, Samples::Ci16(&ramp(k * bs, bs as usize))).starved, 0);
        }
        // Block 8 landed on block 0's slot and block 9 on block 1's.
        assert_eq!(r.oldest(), 2 * bs);
        let mut out = vec![[0i16; 2]; 4];
        assert_eq!(r.read_raw(0, &mut out), Err(ReadError::Overwritten));
        assert_eq!(r.read_raw(bs + 5, &mut out), Err(ReadError::Overwritten));
        r.read_raw(2 * bs, &mut out).unwrap();
        assert_eq!(out[3], [(2 * bs + 3) as i16, 7]);
        let s = r.stats();
        assert_eq!(s.blocks_live, 8);
        assert_eq!(s.held_samples, 8 * bs);
    }

    #[test]
    fn blocks_provisioned_ahead_of_head_are_not_counted() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        assert_eq!(r.stats().blocks_live, 0);
        r.provision(3);
        assert_eq!(r.stats().blocks_live, 0);
        r.write(0, Samples::Ci16(&ramp(0, 1025)));
        assert_eq!(r.stats().blocks_live, 2);
        assert_eq!(r.provision(3), 1); // wanted block is now 1: only block 4 is new
    }

    /// Xorshift64: enough randomness for read offsets without a dependency.
    fn xorshift(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    #[test]
    fn concurrent_reader_sees_ramp_or_overwritten() {
        // Eight slots so provisioning wraps and overwrites blocks while the reader runs.
        const BLOCKS: u64 = 200;
        const READS: u64 = 2000;
        const LEN: u64 = 1500;
        let r = IqRing::with_slots(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()), 8);
        let bs = r.block_samples();
        let reads_done = Arc::new(AtomicU64::new(0));
        let go = Arc::new(Barrier::new(2));

        let writer = {
            let (r, reads_done, go) = (r.clone(), reads_done.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                for k in 0..BLOCKS {
                    r.provision(2);
                    // Several writes per block so reads land mid-block too.
                    for c in 0..4 {
                        let s = k * bs + c * bs / 4;
                        assert_eq!(r.write(s, Samples::Ci16(&ramp(s, (bs / 4) as usize))).starved, 0);
                    }
                    // Keep the two threads interleaved: let the reader catch up a little.
                    while k >= 2 && reads_done.load(Ordering::Relaxed) < (k - 1) * (READS / BLOCKS) {
                        std::thread::yield_now();
                    }
                }
            })
        };

        let reader = {
            let (r, reads_done, go) = (r.clone(), reads_done.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                let mut rng = 0x9E37_79B9_7F4A_7C15u64;
                let (mut ok, mut over) = (0u64, 0u64);
                let mut out = vec![[0i16; 2]; LEN as usize];
                let mut window = (0, 0);
                for n in 0..READS {
                    // Stay at most a block or two ahead of the writer.
                    let want_head = ((n / (READS / BLOCKS) + 2) * bs).min(BLOCKS * bs);
                    while r.head() < want_head {
                        std::thread::yield_now();
                    }
                    // Like a real consumer, pick the readable window, then read
                    // from it a few times while the writer and keeper move on.
                    if n % 4 == 0 {
                        window = loop {
                            let (o, h) = (r.oldest(), r.head());
                            if h >= o + LEN {
                                break (o, h);
                            }
                            std::thread::yield_now();
                        };
                    }
                    let (oldest, head) = window;
                    // Every other read starts in the oldest block, the one the
                    // keeper replaces next, to make the race likely.
                    let room = head - LEN - oldest + 1;
                    let span = if n % 2 == 0 { bs.min(room) } else { room };
                    let start = oldest + xorshift(&mut rng) % span;
                    match r.read_raw(start, &mut out) {
                        Ok(()) => {
                            for (i, s) in out.iter().enumerate() {
                                assert_eq!(*s, [((start + i as u64) % 30000) as i16, 7], "torn read at {start}+{i}");
                            }
                            ok += 1;
                        }
                        Err(ReadError::Overwritten) => over += 1,
                        Err(e) => panic!("unexpected {e:?} reading {start}"),
                    }
                    reads_done.fetch_add(1, Ordering::Relaxed);
                }
                (ok, over)
            })
        };

        writer.join().unwrap();
        let (ok, over) = reader.join().unwrap();
        assert!(ok > 0, "no read succeeded");
        assert!(over > 0, "no read was overwritten ({ok} ok)");
        assert_eq!(r.head(), BLOCKS * bs);
    }
}
