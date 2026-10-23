//! Page-aligned memory regions that can be *offered* to the OS and later
//! *reclaimed*.
//!
//! An offered region is the first thing the OS discards under memory pressure;
//! reclaiming it either returns the contents intact or reports that they were
//! lost (the region is then zero-filled and writable again). The ring keeps a
//! guaranteed window committed and offers older blocks with a priority that
//! falls with age. Memory is never pinned: no `VirtualLock` / `mlock`.

use std::alloc::Layout;
use std::ptr::NonNull;
use std::sync::Arc;

mod fake;
#[cfg(not(any(windows, target_os = "linux")))]
mod fallback;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

pub use fake::FakeVolatile;

/// Granularity of every region: sizes are multiples of this and every region
/// starts on a `PAGE` boundary.
pub const PAGE: usize = 4096;

/// How readily the OS may discard an offered region (lowest goes first).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OfferPriority {
    VeryLow,
    Low,
    BelowNormal,
    Normal,
}

/// A page-aligned block of memory owned by a [`VolatileMemory`] backend.
///
/// It is not `Clone`: it must be handed back to the backend that allocated it
/// with [`VolatileMemory::free`], which consumes it, so no `Region` survives
/// its own release. Dropping a `Region` without freeing it leaks the memory.
#[derive(Debug)]
pub struct Region {
    ptr: NonNull<u8>,
    bytes: usize,
}

// SAFETY: a `Region` is a raw, address-stable block of process memory with no
// thread affinity. The ring hands it to one writer and many readers under its
// own synchronisation protocol, so moving or sharing the handle between threads
// is sound; all access goes through `unsafe` pointer operations whose callers
// uphold that protocol.
unsafe impl Send for Region {}
// SAFETY: see `Send` above; `&Region` only exposes the pointer and the length.
unsafe impl Sync for Region {}

impl Region {
    /// Start of the region; aligned to [`PAGE`].
    pub fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    /// Length in bytes; a non-zero multiple of [`PAGE`].
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn addr(&self) -> usize {
        self.ptr.as_ptr() as usize
    }
}

/// A source of offerable memory.
pub trait VolatileMemory: Send + Sync {
    /// A zero-filled, page-aligned region of `bytes` (a non-zero multiple of
    /// [`PAGE`]), committed lazily by the OS. `None` if `bytes` is not such a
    /// size or the allocation fails.
    fn alloc(&self, bytes: usize) -> Option<Region>;
    /// Let the OS discard `r` under memory pressure. The caller must not touch
    /// the region's memory until it has been reclaimed.
    ///
    /// A region must not be offered again until it has been reclaimed; a
    /// second offer could hide a discard. Backends may panic in debug builds
    /// (the test fake always panics); the OS backends ignore the repeat offer
    /// and log a warning.
    fn offer(&self, r: &Region, priority: OfferPriority);
    /// Take an offered region back. `true`: contents intact. `false`: they were
    /// discarded and the region is writable again. A region that was never
    /// offered (or whose offer failed) reclaims as `true`.
    fn reclaim(&self, r: &Region) -> bool;
    /// Release the region. It must have come from this backend.
    fn free(&self, r: Region);
}

/// The platform backend: Windows offer/reclaim, Linux `MADV_FREE`, or a heap
/// fallback that never discards.
pub fn system() -> Arc<dyn VolatileMemory> {
    #[cfg(windows)]
    return Arc::new(windows::WindowsVolatile::default());
    #[cfg(target_os = "linux")]
    return Arc::new(linux::LinuxVolatile::default());
    #[cfg(not(any(windows, target_os = "linux")))]
    return Arc::new(fallback::FallbackVolatile);
}

fn valid_size(bytes: usize) -> bool {
    bytes != 0 && bytes.is_multiple_of(PAGE)
}

/// Zero-filled, page-aligned heap allocation (fallback and fake backends).
fn heap_alloc(bytes: usize) -> Option<Region> {
    if !valid_size(bytes) {
        return None;
    }
    let layout = Layout::from_size_align(bytes, PAGE).ok()?;
    // SAFETY: `layout` has a non-zero size (checked above).
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    NonNull::new(ptr).map(|ptr| Region { ptr, bytes })
}

/// Release a region from [`heap_alloc`].
fn heap_free(r: Region) {
    let layout = Layout::from_size_align(r.bytes, PAGE).expect("layout was valid at alloc");
    // SAFETY: `r` came from `heap_alloc` with this same layout, and `free`
    // consumes the only handle to it.
    unsafe { std::alloc::dealloc(r.ptr.as_ptr(), layout) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_backend_round_trips_on_an_idle_machine() {
        let m = system();
        let r = m.alloc(8 * PAGE).unwrap();
        unsafe { std::ptr::write_bytes(r.ptr().as_ptr(), 0xA5, r.bytes()) };
        m.offer(&r, OfferPriority::Low);
        assert!(m.reclaim(&r), "offered memory was discarded on an idle machine");
        assert_eq!(unsafe { *r.ptr().as_ptr().add(5 * PAGE + 17) }, 0xA5);
        m.free(r);
    }

    #[test]
    fn fake_backend_discards_oldest_offered_first() {
        let m = FakeVolatile::default();
        let a = m.alloc(PAGE).unwrap();
        let b = m.alloc(PAGE).unwrap();
        let c = m.alloc(PAGE).unwrap();
        unsafe {
            *a.ptr().as_ptr() = 1;
            *b.ptr().as_ptr() = 2;
            *c.ptr().as_ptr() = 3
        };
        m.offer(&a, OfferPriority::VeryLow);
        m.offer(&b, OfferPriority::Low);
        m.offer(&c, OfferPriority::Normal);
        m.discard_oldest_offered(2);
        assert!(!m.reclaim(&a));
        assert!(!m.reclaim(&b));
        assert!(m.reclaim(&c));
        assert_eq!(unsafe { *a.ptr().as_ptr() }, 0);
        assert_eq!(unsafe { *c.ptr().as_ptr() }, 3);
        assert_eq!(
            m.offered().iter().map(|o| o.1).collect::<Vec<_>>(),
            vec![OfferPriority::VeryLow, OfferPriority::Low, OfferPriority::Normal]
        );
    }

    #[test]
    #[should_panic(expected = "offered again before it was reclaimed")]
    fn fake_backend_rejects_a_second_offer_before_reclaim() {
        let m = FakeVolatile::default();
        let a = m.alloc(PAGE).unwrap();
        m.offer(&a, OfferPriority::Low);
        m.discard_oldest_offered(1);
        m.offer(&a, OfferPriority::Low);
    }
}
