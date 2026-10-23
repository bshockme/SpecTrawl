//! One fixed-size block of the ring: a region of volatile memory holding
//! `block_samples` interleaved i16 IQ pairs.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32};

use super::volatile::{Region, VolatileMemory};

/// Where a block's memory stands with the OS.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockState {
    /// Committed; reads and writes go straight through.
    Live = 0,
    /// Offered to the OS; a reader must reclaim it before copying.
    Offered = 1,
    /// The OS discarded the contents; the samples are gone for good.
    Lost = 2,
    /// A reader won the `Offered → Reclaiming` race and is reclaiming it; other
    /// readers wait for `Live` or `Lost`. Transient; the keeper cannot offer it.
    Reclaiming = 3,
    /// The keeper won `Live → Offering` and is offering it (see
    /// `IqRing::try_offer`); readers wait for `Live` (offer abandoned) or
    /// `Offered` (then reclaim it). Transient.
    Offering = 4,
}

impl BlockState {
    pub(super) fn from_u8(v: u8) -> BlockState {
        match v {
            0 => BlockState::Live,
            1 => BlockState::Offered,
            2 => BlockState::Lost,
            3 => BlockState::Reclaiming,
            4 => BlockState::Offering,
            _ => unreachable!("invalid block state {v}"),
        }
    }
}

/// A ring block. Reached through an `ArcSwapOption` slot; the last `Arc`
/// to go frees the region, so a reader holding a slot guard keeps the memory
/// valid even after the keeper has retired or replaced the block.
pub struct Block {
    /// Block number: holds samples `index * block_samples .. (index + 1) * block_samples`.
    pub index: u64,
    pub(super) state: AtomicU8,
    /// Readers currently copying out of this block (the keeper must not offer it while non-zero).
    pub(super) readers: AtomicU32,
    region: ManuallyDrop<Region>,
    mem: Arc<dyn VolatileMemory>,
}

impl Block {
    pub(super) fn new(index: u64, region: Region, mem: Arc<dyn VolatileMemory>) -> Block {
        Block { index, state: AtomicU8::new(BlockState::Live as u8), readers: AtomicU32::new(0), region: ManuallyDrop::new(region), mem }
    }

    /// First sample of the block. The region is page-aligned, which satisfies
    /// `[i16; 2]`'s alignment, and holds `region().bytes() / 4` samples.
    pub(super) fn samples(&self) -> *mut [i16; 2] {
        self.region.ptr().as_ptr().cast::<[i16; 2]>()
    }

    pub(super) fn region(&self) -> &Region {
        &self.region
    }

    pub(super) fn mem(&self) -> &Arc<dyn VolatileMemory> {
        &self.mem
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: `region` is taken exactly once, here, and `self` is never
        // used again after `drop`.
        let region = unsafe { ManuallyDrop::take(&mut self.region) };
        self.mem.free(region);
    }
}
