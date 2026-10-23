//! The last few FFT blocks of one source, kept for the channelizer.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::segments::Segment;

/// The unwindowed, natural-order bins of the most recent `depth` blocks,
/// shared by `Arc` so readers never copy them.
pub struct SpectrumCache {
    depth: usize,
    blocks: Mutex<VecDeque<Cached>>,
}

struct Cached {
    block: u64,
    segment: Segment,
    bins: Arc<[Complex32]>,
}

impl SpectrumCache {
    pub fn new(depth: usize) -> SpectrumCache {
        SpectrumCache { depth, blocks: Mutex::new(VecDeque::with_capacity(depth + 1)) }
    }

    /// Block `block`'s segment and bins, if it is still cached.
    pub fn get(&self, block: u64) -> Option<(Segment, Arc<[Complex32]>)> {
        let blocks = self.blocks.lock();
        blocks.iter().rev().find(|c| c.block == block).map(|c| (c.segment.clone(), c.bins.clone()))
    }

    /// The most recently inserted block.
    pub fn latest(&self) -> Option<u64> {
        self.blocks.lock().back().map(|c| c.block)
    }

    /// Add `block` (block numbers only increase), evicting the oldest past `depth`.
    pub(crate) fn insert(&self, block: u64, segment: Segment, bins: Arc<[Complex32]>) {
        let mut blocks = self.blocks.lock();
        blocks.push_back(Cached { block, segment, bins });
        while blocks.len() > self.depth {
            blocks.pop_front();
        }
    }
}
