//! Windows: `OfferVirtualMemory` / `ReclaimVirtualMemory`.

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::{NonNull, null};

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::ERROR_BUSY;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, OFFER_PRIORITY, OfferVirtualMemory, PAGE_READWRITE, ReclaimVirtualMemory, VirtualAlloc,
    VirtualFree, VmOfferPriorityBelowNormal, VmOfferPriorityLow, VmOfferPriorityNormal, VmOfferPriorityVeryLow,
};

use super::{OfferPriority, Region, VolatileMemory, valid_size};

/// Regions are `VirtualAlloc` ranges (64 KiB aligned, so page aligned).
#[derive(Default)]
pub(super) struct WindowsVolatile {
    /// Addresses whose last `OfferVirtualMemory` succeeded and that have not
    /// been reclaimed since. A failed offer leaves the region live and intact,
    /// so it is simply not recorded here.
    offered: Mutex<HashSet<usize>>,
}

fn os_priority(p: OfferPriority) -> OFFER_PRIORITY {
    match p {
        OfferPriority::VeryLow => VmOfferPriorityVeryLow,
        OfferPriority::Low => VmOfferPriorityLow,
        OfferPriority::BelowNormal => VmOfferPriorityBelowNormal,
        OfferPriority::Normal => VmOfferPriorityNormal,
    }
}

impl VolatileMemory for WindowsVolatile {
    fn alloc(&self, bytes: usize) -> Option<Region> {
        if !valid_size(bytes) {
            return None;
        }
        // SAFETY: plain reservation + commit of fresh memory at an address the
        // OS chooses; no existing memory is affected. The OS zero-fills it and
        // backs pages lazily on first touch.
        let p = unsafe { VirtualAlloc(null(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        NonNull::new(p.cast::<u8>()).map(|ptr| Region { ptr, bytes })
    }

    fn offer(&self, r: &Region, priority: OfferPriority) -> bool {
        if self.offered.lock().contains(&r.addr()) {
            log::warn!("region offered again before it was reclaimed; ignoring the repeat offer");
            return true;
        }
        // SAFETY: `r` is a whole committed `VirtualAlloc` range owned by this
        // backend; the caller does not touch it until `reclaim`.
        let rc = unsafe { OfferVirtualMemory(r.ptr.as_ptr().cast::<c_void>(), r.bytes, os_priority(priority)) };
        if rc == 0 {
            self.offered.lock().insert(r.addr());
            true
        } else {
            log::warn!("OfferVirtualMemory refused the offer (error {rc}); region stays committed");
            false
        }
    }

    fn reclaim(&self, r: &Region) -> bool {
        if !self.offered.lock().remove(&r.addr()) {
            return true;
        }
        // SAFETY: `r` is a range this backend successfully offered and has not
        // reclaimed since; reclaiming makes it committed and writable again.
        let rc = unsafe { ReclaimVirtualMemory(r.ptr.as_ptr().cast::<c_void>(), r.bytes) };
        match rc {
            0 => true,
            ERROR_BUSY => false,
            other => {
                log::error!("ReclaimVirtualMemory failed ({other}); treating contents as lost");
                false
            }
        }
    }

    fn free(&self, r: Region) {
        self.offered.lock().remove(&r.addr());
        // SAFETY: `r.ptr` is the base of a `VirtualAlloc` reservation owned by
        // this backend (size must be 0 with `MEM_RELEASE`), and `free` consumes
        // the only handle to it. Offered memory may be released directly.
        let ok = unsafe { VirtualFree(r.ptr.as_ptr().cast::<c_void>(), 0, MEM_RELEASE) };
        if ok == 0 {
            log::error!("VirtualFree failed; region leaked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::PAGE;
    use super::*;

    // `ReclaimVirtualMemory` on a range that is not currently offered returns
    // ERROR_INVALID_PARAMETER (87), not success, so the backend must only call
    // it for ranges whose offer succeeded.
    //
    // The OS may refuse an offer, and under load it may discard a `VeryLow`
    // offer at once (`ReclaimVirtualMemory` then returns ERROR_BUSY, 170), so
    // neither acceptance nor an intact first reclaim is asserted.
    #[test]
    fn only_successful_offers_are_reclaimed_from_the_os() {
        let m = WindowsVolatile::default();
        let r = m.alloc(2 * PAGE).unwrap();
        assert!(m.reclaim(&r), "never-offered region must reclaim intact");
        let accepted = (0..5).any(|_| m.offer(&r, OfferPriority::VeryLow));
        assert_eq!(m.offered.lock().contains(&r.addr()), accepted, "only an accepted offer is recorded");
        if accepted {
            // Goes to the OS once: intact, or discarded under load.
            let _intact = m.reclaim(&r);
            assert!(!m.offered.lock().contains(&r.addr()));
        } else {
            eprintln!("OfferVirtualMemory refused 5 offers; checking the refusal path");
        }
        // Refused, or already reclaimed: the region is not offered, so this
        // must not reach the OS (which would fail with 87 and report a loss).
        assert!(m.reclaim(&r), "a reclaim of a region that is not offered must not reach the OS");
        m.free(r);
    }
}
