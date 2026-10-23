//! Platforms without an offer/reclaim facility: plain heap memory that is
//! never discarded.

use super::{OfferPriority, Region, VolatileMemory, heap_alloc, heap_free};

pub(super) struct FallbackVolatile;

impl VolatileMemory for FallbackVolatile {
    fn alloc(&self, bytes: usize) -> Option<Region> {
        heap_alloc(bytes)
    }

    /// No offer facility: every offer is refused (not an error, so not
    /// logged), the region stays committed and the ring keeps it `Live`.
    fn offer(&self, _r: &Region, _priority: OfferPriority) -> bool {
        false
    }

    fn reclaim(&self, _r: &Region) -> bool {
        true
    }

    fn free(&self, r: Region) {
        heap_free(r)
    }
}
