//! Platforms without an offer/reclaim facility: plain heap memory that is
//! never discarded.

use super::{OfferPriority, Region, VolatileMemory, heap_alloc, heap_free};

pub(super) struct FallbackVolatile;

impl VolatileMemory for FallbackVolatile {
    fn alloc(&self, bytes: usize) -> Option<Region> {
        heap_alloc(bytes)
    }

    fn offer(&self, _r: &Region, _priority: OfferPriority) {}

    fn reclaim(&self, _r: &Region) -> bool {
        true
    }

    fn free(&self, r: Region) {
        heap_free(r)
    }
}
