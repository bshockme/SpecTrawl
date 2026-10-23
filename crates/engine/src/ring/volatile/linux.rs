//! Linux: `madvise(MADV_FREE)` with per-page hashes to detect discards.
//!
//! `MADV_FREE` lets the kernel drop clean anonymous pages lazily but never
//! says whether it did; a dropped page reads back as zeros. So `offer` hashes
//! every page first, and `reclaim` writes one byte of each page back to itself
//! (which cancels the lazy free for that page) and then compares hashes.
//! The priority is ignored: `MADV_FREE` has none.

use std::collections::HashMap;
use std::ptr::{self, NonNull};

use parking_lot::Mutex;

use super::{OfferPriority, PAGE, Region, VolatileMemory, valid_size};

#[derive(Default)]
pub(super) struct LinuxVolatile {
    /// Region address -> hash of each of its pages at offer time.
    hashes: Mutex<HashMap<usize, Vec<u64>>>,
}

/// FxHash-style multiply-xor over the page's `u64` words.
///
/// # Safety
/// `page` must point to `PAGE` readable, 8-byte aligned bytes.
unsafe fn hash_page(page: *const u8) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let words = page.cast::<u64>();
    let mut h = 0u64;
    for i in 0..PAGE / 8 {
        // SAFETY: in bounds and aligned per the function contract; volatile so
        // the read is not elided or merged with the write-back.
        let w = unsafe { ptr::read_volatile(words.add(i)) };
        h = (h.rotate_left(5) ^ w).wrapping_mul(K);
    }
    h
}

impl VolatileMemory for LinuxVolatile {
    fn alloc(&self, bytes: usize) -> Option<Region> {
        if !valid_size(bytes) {
            return None;
        }
        // SAFETY: fresh private anonymous mapping at an address the kernel
        // chooses; no existing memory is affected. It is zero-filled and page
        // aligned.
        let p = unsafe {
            libc::mmap(ptr::null_mut(), bytes, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        if p == libc::MAP_FAILED {
            return None;
        }
        NonNull::new(p.cast::<u8>()).map(|ptr| Region { ptr, bytes })
    }

    fn offer(&self, r: &Region, _priority: OfferPriority) {
        if self.hashes.lock().contains_key(&r.addr()) {
            // Re-hashing now could record pages the kernel already dropped and
            // hide the loss, so keep the hashes from the first offer.
            log::warn!("region offered again before it was reclaimed; ignoring the repeat offer");
            return;
        }
        let base = r.ptr.as_ptr();
        let hashes = (0..r.bytes / PAGE)
            // SAFETY: each page lies inside the mapping, which is page aligned.
            .map(|i| unsafe { hash_page(base.add(i * PAGE)) })
            .collect();
        self.hashes.lock().insert(r.addr(), hashes);
        // SAFETY: `r` is a whole mapping owned by this backend; MADV_FREE only
        // lets the kernel drop its pages, which then read as zeros.
        let rc = unsafe { libc::madvise(base.cast(), r.bytes, libc::MADV_FREE) };
        if rc != 0 {
            log::warn!("madvise(MADV_FREE) failed ({}); region stays resident", std::io::Error::last_os_error());
        }
    }

    fn reclaim(&self, r: &Region) -> bool {
        let Some(hashes) = self.hashes.lock().remove(&r.addr()) else {
            return true;
        };
        let base = r.ptr.as_ptr();
        let mut intact = true;
        for (i, &h) in hashes.iter().enumerate() {
            // SAFETY: the page lies inside the mapping. Writing a byte back
            // first makes the page dirty again, so the kernel can no longer
            // drop it; hashing afterwards sees its final contents. Checking
            // before the write-back would race: a drop between the read and the
            // write would leave a zeroed page with one restored byte, unseen.
            let now = unsafe {
                let page = base.add(i * PAGE);
                ptr::write_volatile(page, ptr::read_volatile(page));
                hash_page(page)
            };
            // Contents are not touched while offered, so any change means the
            // kernel dropped the page (it now reads as zeros).
            if now != h {
                intact = false;
            }
        }
        intact
    }

    fn free(&self, r: Region) {
        self.hashes.lock().remove(&r.addr());
        // SAFETY: `r` is a whole mapping from `alloc`, and `free` consumes the
        // only handle to it.
        let rc = unsafe { libc::munmap(r.ptr.as_ptr().cast(), r.bytes) };
        if rc != 0 {
            log::error!("munmap failed: {}", std::io::Error::last_os_error());
        }
    }
}
