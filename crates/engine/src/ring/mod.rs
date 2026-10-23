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
//!
//! Holes: a write that starts past `head` (the device lost samples) leaves
//! `head .. first_sample` unwritten. The ring records that range in a small
//! hole table before publishing the new `head`, and a read that intersects a
//! recorded hole returns `Err(Gap)`, so no read ever returns samples the
//! device did not deliver. The table holds the newest [`HOLES`] holes; when a
//! hole is evicted, `oldest` first moves past it.

pub mod block;
pub mod keeper;
pub mod volatile;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use arc_swap::ArcSwapOption;
use sdr_core::{Complex32, Samples};

use self::block::{Block, BlockState};
use self::volatile::{OfferPriority, PAGE, VolatileMemory};

/// Default block size: 64 MiB, 16 Mi samples.
pub const DEFAULT_BLOCK_BYTES: usize = 64 << 20;
/// Blocks provisioned ahead of the writer until [`IqRing::set_provision_ahead`] changes it.
pub const DEFAULT_PROVISION_AHEAD: u32 = 2;
/// Number of block slots: 256 GiB at the default block size.
pub const MAX_SLOTS: usize = 4096;
/// Holes (device gaps) the ring remembers; an older hole is dropped from the
/// held range (`oldest` moves past it) when a newer one needs its entry.
pub const HOLES: usize = 64;

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
    /// Some of the range lies in a gap: the device never delivered it.
    Gap,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WriteOutcome {
    /// Samples skipped because their block was not provisioned.
    pub starved: u64,
    /// First skipped sample (meaningful when `starved > 0`). Everything below
    /// the new `head` is then unreadable (`oldest` moved up to it).
    pub first_starved: u64,
}

/// One entry of the hole table: `start .. end` was never written. Empty
/// entries are `0 .. 0`, which intersect nothing.
#[derive(Default)]
struct Hole {
    start: AtomicU64,
    end: AtomicU64,
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

/// What one [`IqRing::housekeep`] pass did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Housekeeping {
    /// Blocks allocated ahead of the writer.
    pub provisioned: u32,
    /// Blocks dropped from their slots (past the ceiling, lost, or stale below `oldest`).
    pub retired: u32,
    /// Blocks offered to the OS.
    pub offered: u32,
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
    /// Blocks `housekeep` provisions past the writer's block.
    provision_ahead: AtomicU32,
    guaranteed_samples: AtomicU64,
    blocks_lost_total: AtomicU64,
    starved_total: AtomicU64,
    /// The newest [`HOLES`] holes, entry `k % HOLES` for the `k`-th hole.
    holes: Box<[Hole]>,
    /// Holes recorded so far. Stored by the writer only.
    holes_recorded: AtomicU64,
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
            provision_ahead: AtomicU32::new(DEFAULT_PROVISION_AHEAD),
            guaranteed_samples: AtomicU64::new(0),
            blocks_lost_total: AtomicU64::new(0),
            starved_total: AtomicU64::new(0),
            holes: (0..HOLES).map(|_| Hole::default()).collect::<Vec<_>>().into_boxed_slice(),
            holes_recorded: AtomicU64::new(0),
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
    /// are dropped; a `first_sample` past `head` makes `head` jump and records
    /// `head .. first_sample` as a hole (reads of it return `Err(Gap)`).
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
        if first_sample > head {
            // Recorded before `head` moves past it (the Release store below),
            // so a reader that sees the new `head` also sees the hole.
            self.record_hole(head, first_sample);
        }
        let mut pos = first_sample.max(head);
        let mut starved = 0;
        let mut first_starved = 0;
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
                _ => {
                    if starved == 0 {
                        first_starved = pos;
                    }
                    starved += run as u64;
                }
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
        WriteOutcome { starved, first_starved }
    }

    /// Writer side: record `start .. end` as a hole. If the entry still holds
    /// an older hole, `oldest` moves past that hole first (SeqCst, before the
    /// entry changes): a reader that loads any new value of the entry then
    /// also sees the new `oldest` in its final check, so a torn entry can
    /// never let a read of the old hole succeed.
    fn record_hole(&self, start: u64, end: u64) {
        let k = self.holes_recorded.load(Ordering::Relaxed);
        let h = &self.holes[(k % HOLES as u64) as usize];
        if k >= HOLES as u64 {
            self.oldest.fetch_max(h.end.load(Ordering::Relaxed), Ordering::SeqCst);
        }
        h.start.store(start, Ordering::SeqCst);
        h.end.store(end, Ordering::SeqCst);
        self.holes_recorded.store(k + 1, Ordering::Relaxed);
    }

    /// Whether `start .. end` intersects a recorded hole.
    fn in_hole(&self, start: u64, end: u64) -> bool {
        self.holes.iter().any(|h| {
            let (s, e) = (h.start.load(Ordering::SeqCst), h.end.load(Ordering::SeqCst));
            s < end && start < e
        })
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
        // Every hole below the `head` loaded above was recorded before that
        // `head` was stored, so it is visible here; an evicted one is caught
        // by the `oldest` re-check at the end.
        if self.in_hole(start, end) {
            return Err(ReadError::Gap);
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
        let mut waited = 0u32;
        loop {
            match BlockState::from_u8(block.state.load(Ordering::SeqCst)) {
                BlockState::Live => return Ok(()),
                BlockState::Lost => return Err(ReadError::Lost),
                // Another thread is inside `offer` or `reclaim`; wait for the
                // outcome (bounded spin, then yield so a descheduled winner runs).
                BlockState::Reclaiming | BlockState::Offering => {
                    waited += 1;
                    if waited < SPINS_BEFORE_YIELD {
                        std::hint::spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                }
                BlockState::Offered => {
                    let won = block.state.compare_exchange(
                        BlockState::Offered as u8,
                        BlockState::Reclaiming as u8,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    if won.is_ok() {
                        // If `reclaim` unwinds, nobody knows whether the
                        // contents survived: settle the block as Lost so
                        // waiters do not spin on `Reclaiming` forever.
                        let settle = StateOnUnwind(block, BlockState::Lost);
                        let intact = block.mem().reclaim(block.region());
                        std::mem::forget(settle);
                        if intact {
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

    /// Keeper side: offer `block`'s memory to the OS if no reader is in it.
    /// Returns `true` if the block is now `Offered`.
    ///
    /// Protocol (pairs with the reader's `readers += 1` then state load, all
    /// `SeqCst`): CAS `Live → Offering`; if `readers != 0`, revert to `Live`
    /// and give up; otherwise `mem.offer()` and store `Offered`. A reader
    /// that arrives after the CAS sees `Offering` or `Offered` and waits or
    /// reclaims; a reader that arrived before it is seen in `readers`. So no
    /// reader copies from memory that is offered, and a block is offered at
    /// most once per reclaim (only a `Live` block can be offered, and an
    /// offered block becomes `Live` again only through `reclaim`).
    ///
    /// The keeper must never offer the block holding `head`: the writer does
    /// not look at the state.
    pub(crate) fn try_offer(&self, block: &Block, priority: OfferPriority) -> bool {
        if block.state.compare_exchange(BlockState::Live as u8, BlockState::Offering as u8, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return false;
        }
        if block.readers.load(Ordering::SeqCst) != 0 {
            block.state.store(BlockState::Live as u8, Ordering::SeqCst);
            return false;
        }
        // If `offer` unwinds, treat the offer as not made: back to `Live`.
        let settle = StateOnUnwind(block, BlockState::Live);
        block.mem().offer(block.region(), priority);
        std::mem::forget(settle);
        block.state.store(BlockState::Offered as u8, Ordering::SeqCst);
        true
    }

    /// The block in `b`'s slot, if it is block `b`.
    #[cfg(test)]
    fn block_at(&self, b: u64) -> Option<Arc<Block>> {
        self.slot(b).load_full().filter(|blk| blk.index == b)
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

    /// One keeper pass: provision [`IqRing::set_provision_ahead`] blocks ahead (2 by default), retire blocks beyond the
    /// ceiling (oldest first, never the writer's block), offer every `Live`
    /// block older than the guaranteed window with a priority by age.
    ///
    /// Single keeper: only one thread calls `housekeep` and `provision`.
    ///
    /// The protected range is every block that intersects the guaranteed
    /// window `head - guaranteed_samples .. head` plus the block holding
    /// `head` (the writer's, whose state the writer never looks at). It is
    /// never retired or offered, even when the ceiling is smaller: the ring
    /// then keeps `max(ceiling, protected blocks)`.
    ///
    /// Order: provision first (the writer's next block matters most), then
    /// retire (so nothing about to be dropped is offered and the window the
    /// offer bands are cut from is the one that stays), then offer.
    pub fn housekeep(&self) -> Housekeeping {
        let provisioned = self.provision(self.provision_ahead.load(Ordering::Relaxed));
        let bs = self.block_samples;
        // `head` only grows, so everything below this snapshot's block stays
        // behind the writer for the rest of the pass.
        let head = self.head();
        let guaranteed = self.guaranteed_samples.load(Ordering::Relaxed);
        // First block that intersects the guaranteed window or holds `head`.
        let protected_from = head.saturating_sub(guaranteed) / bs;
        let retired = self.retire(head, protected_from);
        let offered = self.offer_aged(protected_from);
        Housekeeping { provisioned, retired, offered }
    }

    /// Retire blocks below `protected_from`: first every block whose samples
    /// lie entirely below `oldest` (left behind by starvation), then, walking
    /// up from `oldest`, `Lost` blocks at the front and held blocks while
    /// more than `ceiling_blocks` are held. Returns the number retired.
    fn retire(&self, head: u64, protected_from: u64) -> u32 {
        let bs = self.block_samples;
        let mut retired = 0;
        // Stale blocks: not held any more, but still in their slots.
        let oldest = self.oldest();
        for slot in self.slots.iter() {
            let stale = slot.load().as_ref().is_some_and(|b| b.index < protected_from && (b.index + 1) * bs <= oldest);
            if stale {
                slot.store(None);
                retired += 1;
            }
        }
        // Held blocks counted as in `stats`: written, not lost.
        let written_blocks = head.div_ceil(bs);
        let mut held = self
            .slots
            .iter()
            .filter(|slot| {
                slot.load()
                    .as_ref()
                    .is_some_and(|b| b.index < written_blocks && BlockState::from_u8(b.state.load(Ordering::SeqCst)) != BlockState::Lost)
            })
            .count() as u64;
        let ceiling = u64::from(self.ceiling_blocks.load(Ordering::Relaxed));
        let mut b = self.oldest() / bs;
        while b < protected_from {
            let slot = self.slot(b);
            // `None`: nothing held here (never provisioned); just step over it.
            let block = slot.load_full().filter(|blk| blk.index == b);
            if let Some(block) = &block
                && BlockState::from_u8(block.state.load(Ordering::SeqCst)) != BlockState::Lost
            {
                if held <= ceiling {
                    break;
                }
                held -= 1;
            }
            // Move `oldest` past the block before its slot empties: a reader
            // then fails the `oldest` check or finds the slot empty, and one
            // still copying re-checks `oldest` when done. The block's memory
            // is freed when the last guard drops.
            self.oldest.fetch_max((b + 1) * bs, Ordering::AcqRel);
            if block.is_some() {
                slot.store(None);
                retired += 1;
            }
            b += 1;
        }
        retired
    }

    /// Offer every `Live` block from `oldest` up to `protected_from`, oldest
    /// first, with a priority by age: the window is split into four equal
    /// bands from newest to oldest (`Normal`, `BelowNormal`, `Low`,
    /// `VeryLow`), and the oldest block is always `VeryLow`. Blocks already
    /// offered are left alone (a block is offered at most once per reclaim);
    /// a block a reader reclaimed is `Live` again and is offered at its
    /// current band. Returns the number offered.
    fn offer_aged(&self, protected_from: u64) -> u32 {
        let first = self.oldest() / self.block_samples;
        let n = protected_from.saturating_sub(first);
        let mut offered = 0;
        for b in first..protected_from {
            let Some(block) = self.slot(b).load_full().filter(|blk| blk.index == b) else { continue };
            if BlockState::from_u8(block.state.load(Ordering::SeqCst)) != BlockState::Live {
                continue;
            }
            // 0 for the newest block of the window, n - 1 for the oldest.
            let age = protected_from - 1 - b;
            let band = if age == n - 1 { 3 } else { age * 4 / n };
            let priority = match band {
                0 => OfferPriority::Normal,
                1 => OfferPriority::BelowNormal,
                2 => OfferPriority::Low,
                _ => OfferPriority::VeryLow,
            };
            if self.try_offer(&block, priority) {
                offered += 1;
            }
        }
        offered
    }

    /// How many blocks past the writer's `housekeep` provisions (default 2).
    /// The keeper sizes this to cover 250 ms of samples at the source's rate,
    /// so a keeper stalled for a tick or more (20 ms each) never starves a
    /// real device even with small blocks.
    pub fn set_provision_ahead(&self, blocks: u32) {
        self.provision_ahead.store(blocks, Ordering::Relaxed);
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
                    BlockState::Live | BlockState::Reclaiming | BlockState::Offering => live += 1,
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
        // SeqCst with the state load that follows, against `try_offer`'s
        // `Live → Offering` CAS then `readers` load: either the keeper sees
        // this reader and backs off, or this reader sees `Offering`/`Offered`.
        block.readers.fetch_add(1, Ordering::SeqCst);
        ReaderGuard(block)
    }
}

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// While a transient state's owner is inside `offer`/`reclaim`: if that call
/// unwinds, store the settling state so waiters are released. Forgotten on
/// the normal path.
struct StateOnUnwind<'a>(&'a Block, BlockState);

impl Drop for StateOnUnwind<'_> {
    fn drop(&mut self) {
        self.0.state.store(self.1 as u8, Ordering::SeqCst);
    }
}

/// Spins in `make_live` before it starts yielding the thread.
const SPINS_BEFORE_YIELD: u32 = 64;

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

    use super::block::BlockState;
    use super::volatile::{FakeVolatile, OfferPriority};
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
    fn housekeep_provisions_the_configured_number_of_blocks_ahead() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        r.set_provision_ahead(5);
        assert_eq!(r.housekeep().provisioned, 6); // blocks 0..=5
        for b in 0..=5 {
            assert!(r.block_at(b).is_some(), "block {b}");
        }
        assert!(r.block_at(6).is_none());
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
    fn a_jump_past_head_is_a_hole_that_never_reads_as_samples() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        r.provision(3); // blocks 0..=3: the hole lands in provisioned (zeroed) memory
        r.write(0, Samples::Ci16(&ramp(0, 1000)));
        assert_eq!(r.write(1500, Samples::Ci16(&ramp(1500, 1000))).starved, 0);
        let mut out = vec![[0i16; 2]; 10];
        assert_eq!(r.read_raw(1200, &mut out), Err(ReadError::Gap)); // inside
        assert_eq!(r.read_raw(995, &mut out), Err(ReadError::Gap)); // data, then hole
        assert_eq!(r.read_raw(1495, &mut out), Err(ReadError::Gap)); // hole, then data
        assert_eq!(r.read_raw(990, &mut out), Ok(())); // ends at the hole
        assert_eq!(out[9], [999, 7]);
        r.read_raw(1500, &mut out).unwrap(); // after it
        assert_eq!(out[0], [1500, 7]);
        let mut f = vec![Complex32::default(); 600];
        assert_eq!(r.read(900, &mut f), Err(ReadError::Gap));
    }

    #[test]
    fn an_evicted_hole_moves_oldest_past_it() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        r.provision(200);
        let mut at = 0;
        for _ in 0..=HOLES {
            r.write(at, Samples::Ci16(&ramp(at, 10)));
            at += 20; // a 10-sample hole after every write
        }
        // `HOLES + 1` writes made `HOLES` holes: none evicted yet.
        assert_eq!(r.oldest(), 0);
        r.write(at, Samples::Ci16(&ramp(at, 10)));
        // The first hole (10 .. 20) was evicted: `oldest` moved past it.
        assert_eq!(r.oldest(), 20);
        let mut out = vec![[0i16; 2]; 5];
        assert_eq!(r.read_raw(12, &mut out), Err(ReadError::Overwritten));
        assert_eq!(r.read_raw(32, &mut out), Err(ReadError::Gap));
        r.read_raw(20, &mut out).unwrap();
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

    fn state(b: &Block) -> BlockState {
        BlockState::from_u8(b.state.load(Ordering::SeqCst))
    }

    /// A ring with blocks 0 and 1 full of ramp; returns its fake memory too.
    fn two_full_blocks() -> (Arc<IqRing>, Arc<FakeVolatile>) {
        let mem = Arc::new(FakeVolatile::default());
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, mem.clone());
        r.provision(1);
        r.write(0, Samples::Ci16(&ramp(0, 2048)));
        (r, mem)
    }

    #[test]
    fn try_offer_backs_off_while_a_reader_is_in_the_block() {
        let (r, mem) = two_full_blocks();
        let b0 = r.block_at(0).unwrap();
        let reading = ReaderGuard::enter(&b0);
        assert!(!r.try_offer(&b0, OfferPriority::Low));
        assert_eq!(state(&b0), BlockState::Live);
        assert!(mem.offered().is_empty());
        drop(reading);
        assert!(r.try_offer(&b0, OfferPriority::Low));
        assert!(!r.try_offer(&b0, OfferPriority::Low), "an offered block is not offered twice");
    }

    #[test]
    fn offered_block_is_reclaimed_by_a_read_or_reported_lost() {
        let (r, mem) = two_full_blocks();
        let b0 = r.block_at(0).unwrap();
        assert!(r.try_offer(&b0, OfferPriority::Low));
        assert_eq!(state(&b0), BlockState::Offered);
        assert_eq!(mem.offered().len(), 1);
        let mut out = vec![[0i16; 2]; 4];
        r.read_raw(10, &mut out).unwrap(); // reclaims intact
        assert_eq!(out[0], [10, 7]);
        assert_eq!(state(&b0), BlockState::Live);

        assert!(r.try_offer(&b0, OfferPriority::Low));
        mem.discard_oldest_offered(1);
        assert_eq!(r.read_raw(10, &mut out), Err(ReadError::Lost));
        assert_eq!(state(&b0), BlockState::Lost);
        assert_eq!(r.read_raw(10, &mut out), Err(ReadError::Lost));
        assert!(!r.try_offer(&b0, OfferPriority::Low));
        assert_eq!(r.stats().blocks_lost_total, 1);
        r.read_raw(1100, &mut out).unwrap(); // block 1 untouched
    }

    /// The fake for a single offered block, with `offer` delayed on entry
    /// (widening the window between the keeper deciding to offer and the
    /// offer taking effect) and a check that `reclaim` only ever sees a region
    /// whose offer has completed.
    struct SlowOffer(Arc<FakeVolatile>, std::sync::atomic::AtomicBool);

    impl VolatileMemory for SlowOffer {
        fn alloc(&self, bytes: usize) -> Option<volatile::Region> {
            self.0.alloc(bytes)
        }
        fn offer(&self, r: &volatile::Region, priority: OfferPriority) {
            let t = std::time::Instant::now();
            while t.elapsed() < std::time::Duration::from_micros(2) {
                std::hint::spin_loop();
            }
            self.0.offer(r, priority);
            self.1.store(true, Ordering::SeqCst);
        }
        fn reclaim(&self, r: &volatile::Region) -> bool {
            assert!(self.1.swap(false, Ordering::SeqCst), "reclaim of a region whose offer had not completed");
            self.0.reclaim(r)
        }
        fn free(&self, r: volatile::Region) {
            self.0.free(r)
        }
    }

    #[test]
    fn concurrent_offers_and_reads_never_copy_offered_memory() {
        const CYCLES: u64 = 5000;
        let mem = Arc::new(FakeVolatile::default());
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(SlowOffer(mem.clone(), Default::default())));
        r.provision(1);
        r.write(0, Samples::Ci16(&ramp(0, 2048)));
        let go = Arc::new(Barrier::new(2));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let keeper = {
            let (r, mem, go, done) = (r.clone(), mem.clone(), go.clone(), done.clone());
            std::thread::spawn(move || {
                let b0 = r.block_at(0).unwrap();
                let mut out = vec![[0i16; 2]; 64];
                go.wait();
                for i in 0..CYCLES {
                    r.try_offer(&b0, OfferPriority::Low);
                    if i == CYCLES * 4 / 5 {
                        // Zero-fill whatever is offered: a reader copying offered
                        // memory now would see zeros in an `Ok` read.
                        mem.discard_oldest_offered(1);
                    }
                    let _ = r.read_raw(i % 900, &mut out);
                }
                done.store(true, Ordering::Release);
            })
        };
        let reader = {
            let (r, go, done) = (r.clone(), go.clone(), done.clone());
            std::thread::spawn(move || {
                let mut rng = 0x2545_F491_4F6C_DD1Du64;
                let mut out = vec![[0i16; 2]; 512];
                let (mut ok, mut lost) = (0u64, 0u64);
                go.wait();
                // Read for as long as the keeper runs.
                while !done.load(Ordering::Acquire) {
                    let start = xorshift(&mut rng) % (1024 - 512 + 1);
                    match r.read_raw(start, &mut out) {
                        Ok(()) => {
                            for (i, s) in out.iter().enumerate() {
                                assert_eq!(*s, [(start + i as u64) as i16, 7], "read offered memory at {start}+{i}");
                            }
                            ok += 1;
                        }
                        Err(ReadError::Lost) => lost += 1,
                        Err(e) => panic!("unexpected {e:?}"),
                    }
                }
                (ok, lost)
            })
        };
        keeper.join().expect("keeper panicked (double offer?)");
        let (ok, _lost) = reader.join().expect("reader panicked (copied offered memory?)");
        assert!(ok > 0, "no read succeeded");
        assert!(mem.offered().len() > 1, "the keeper never offered more than once");
    }

    /// The fake, with a `reclaim` that panics.
    struct PanickyReclaim(FakeVolatile);

    impl VolatileMemory for PanickyReclaim {
        fn alloc(&self, bytes: usize) -> Option<volatile::Region> {
            self.0.alloc(bytes)
        }
        fn offer(&self, r: &volatile::Region, priority: OfferPriority) {
            self.0.offer(r, priority)
        }
        fn reclaim(&self, _: &volatile::Region) -> bool {
            panic!("reclaim failed")
        }
        fn free(&self, r: volatile::Region) {
            self.0.free(r)
        }
    }

    #[test]
    fn a_reclaim_that_unwinds_leaves_the_block_lost_not_stuck() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(PanickyReclaim(FakeVolatile::default())));
        r.provision(1);
        r.write(0, Samples::Ci16(&ramp(0, 2048)));
        let b0 = r.block_at(0).unwrap();
        assert!(r.try_offer(&b0, OfferPriority::Low));
        let mut out = vec![[0i16; 2]; 4];
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.read_raw(0, &mut out)));
        assert!(unwound.is_err());
        assert_eq!(state(&b0), BlockState::Lost);
        assert_eq!(b0.readers.load(Ordering::SeqCst), 0);
        assert_eq!(r.read_raw(0, &mut out), Err(ReadError::Lost)); // returns, does not spin
    }

    #[test]
    fn oldest_offered_blocks_are_lost_first_and_the_guaranteed_window_never_is() {
        let mem = Arc::new(FakeVolatile::default());
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, mem.clone());
        r.set_ceiling_blocks(12);
        r.set_guaranteed_samples(3 * 1024);
        for b in 0..12u64 {
            r.housekeep();
            r.write(b * 1024, Samples::Ci16(&ramp(b * 1024, 1024)));
        }
        r.housekeep();
        let s = r.stats();
        assert_eq!(s.blocks_live + s.blocks_offered, 12);
        assert!(s.blocks_offered >= 8, "{s:?}"); // 12 held, 3 guaranteed, writer's block live
        let pri: Vec<_> = mem.offered().iter().map(|o| o.1).collect();
        assert_eq!(pri[0], OfferPriority::VeryLow); // oldest offered lowest
        assert_eq!(*pri.last().unwrap(), OfferPriority::Normal);
        mem.discard_oldest_offered(2); // the OS took blocks 0 and 1
        let mut out = vec![[0i16; 2]; 10];
        assert_eq!(r.read_raw(100, &mut out), Err(ReadError::Lost));
        assert_eq!(r.read_raw(1100, &mut out), Err(ReadError::Lost));
        r.read_raw(2100, &mut out).unwrap(); // block 2 reclaimed intact
        r.read_raw(11 * 1024 + 5, &mut out[..1]).unwrap(); // newest, never offered
        r.housekeep();
        assert_eq!(r.stats().blocks_lost_total, 2);
        assert_eq!(r.oldest(), 2 * 1024);
        assert_eq!(r.read_raw(100, &mut out), Err(ReadError::Overwritten));
    }

    #[test]
    fn ceiling_retires_oldest_and_shrinking_takes_effect_on_the_next_pass() {
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
        r.set_ceiling_blocks(6);
        r.set_guaranteed_samples(1024);
        for b in 0..10u64 {
            r.housekeep();
            r.write(b * 1024, Samples::Ci16(&ramp(b * 1024, 1024)));
        }
        r.housekeep();
        assert_eq!(r.oldest(), 4 * 1024);
        assert_eq!(r.stats().held_samples, 6 * 1024);
        r.set_ceiling_blocks(3);
        let h = r.housekeep();
        assert_eq!(h.retired, 3);
        assert_eq!(r.oldest(), 7 * 1024);
    }

    #[test]
    fn guaranteed_window_is_never_offered_even_under_a_tiny_ceiling() {
        let mem = Arc::new(FakeVolatile::default());
        let r = IqRing::new(RingConfig { block_bytes: 4096 }, mem.clone());
        r.set_ceiling_blocks(2);
        r.set_guaranteed_samples(4 * 1024);
        for b in 0..6u64 {
            r.housekeep();
            r.write(b * 1024, Samples::Ci16(&ramp(b * 1024, 1024)));
        }
        r.housekeep();
        assert!(mem.offered().is_empty());
        assert!(r.stats().held_samples >= 4 * 1024);
    }
}
