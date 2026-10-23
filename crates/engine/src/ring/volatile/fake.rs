//! A deterministic backend for tests: discards only when told to.

use std::collections::{HashSet, VecDeque};

use parking_lot::Mutex;

use super::{OfferPriority, Region, VolatileMemory, heap_alloc, heap_free};

/// Test backend. Heap memory like the fallback; records every offer and lets a
/// test simulate memory pressure with [`FakeVolatile::discard_oldest_offered`].
#[derive(Default)]
pub struct FakeVolatile {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    /// Every offer ever made, in order.
    log: Vec<(usize, OfferPriority)>,
    /// Regions currently offered and not yet discarded: `(addr, bytes)`, oldest first.
    offered: VecDeque<(usize, usize)>,
    /// Regions discarded since their last offer.
    discarded: HashSet<usize>,
}

impl FakeVolatile {
    /// Discard the `n` oldest regions that are still offered: zero-fill them and
    /// make their next `reclaim` report the loss.
    pub fn discard_oldest_offered(&self, n: usize) {
        let mut s = self.state.lock();
        for _ in 0..n {
            let Some((addr, bytes)) = s.offered.pop_front() else {
                break;
            };
            // SAFETY: `addr..addr + bytes` is a live region from `heap_alloc`
            // (offered regions are removed from the queue on reclaim and free),
            // and its owner promised not to touch it while it is offered.
            unsafe { std::ptr::write_bytes(addr as *mut u8, 0, bytes) };
            s.discarded.insert(addr);
        }
    }

    /// Every `(addr, priority)` offer made so far, in order.
    pub fn offered(&self) -> Vec<(usize, OfferPriority)> {
        self.state.lock().log.clone()
    }

    fn forget(s: &mut FakeState, addr: usize) -> bool {
        s.offered.retain(|&(a, _)| a != addr);
        s.discarded.remove(&addr)
    }
}

impl VolatileMemory for FakeVolatile {
    fn alloc(&self, bytes: usize) -> Option<Region> {
        heap_alloc(bytes)
    }

    fn offer(&self, r: &Region, priority: OfferPriority) {
        let mut s = self.state.lock();
        let addr = r.addr();
        let pending = s.offered.iter().any(|&(a, _)| a == addr) || s.discarded.contains(&addr);
        assert!(!pending, "region {addr:#x} offered again before it was reclaimed");
        s.log.push((addr, priority));
        s.offered.push_back((addr, r.bytes()));
    }

    fn reclaim(&self, r: &Region) -> bool {
        let mut s = self.state.lock();
        !Self::forget(&mut s, r.addr())
    }

    fn free(&self, r: Region) {
        Self::forget(&mut self.state.lock(), r.addr());
        heap_free(r)
    }
}
