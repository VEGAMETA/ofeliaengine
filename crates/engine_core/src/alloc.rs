//! The engine's global allocator (ADR 0086): mimalloc 2.3.2, which runs allocation-heavy
//! work (loading, cooking, the editor, tools) 1.5–4.7× as fast as the Windows heap; in
//! debug builds with a counting layer, so a game's CPU memory can be accounted for
//! ([`heap_stats`]). A library cannot choose the global allocator: executables set it.
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOCATOR: engine_core::alloc::EngineAllocator =
//!     engine_core::alloc::EngineAllocator::new();
//! ```
//!
//! mimalloc keeps the memory a thread's heap holds (what it freed, and what other threads
//! freed into it) until that thread allocates again. The job pool's workers give theirs
//! back when they go idle after running jobs (`engine_core::jobs`); other long-lived
//! threads call [`release_thread_memory`] at quiet moments (after a level load).
//!
//! Without the `mimalloc` feature (on by default) the allocator is the system heap.

use core::alloc::{GlobalAlloc, Layout};
#[cfg(debug_assertions)]
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[cfg(feature = "mimalloc")]
const INNER: mimalloc::MiMalloc = mimalloc::MiMalloc;
#[cfg(not(feature = "mimalloc"))]
const INNER: std::alloc::System = std::alloc::System;

/// The engine's global allocator (see the module documentation).
#[derive(Debug, Default)]
pub struct EngineAllocator {
    _private: (),
}

impl EngineAllocator {
    /// The allocator, for a `#[global_allocator]` static.
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(debug_assertions)]
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
#[cfg(debug_assertions)]
static PEAK: AtomicUsize = AtomicUsize::new(0);
#[cfg(debug_assertions)]
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// Counts `bytes` more (or fewer) held: debug builds only.
#[inline]
fn counted(_added: usize, _removed: usize) {
    #[cfg(debug_assertions)]
    {
        if _added > 0 {
            let now = ALLOCATED.fetch_add(_added, Ordering::Relaxed) + _added;
            PEAK.fetch_max(now, Ordering::Relaxed);
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        if _removed > 0 {
            ALLOCATED.fetch_sub(_removed, Ordering::Relaxed);
        }
    }
}

// SAFETY: every call is forwarded unchanged to mimalloc (or the system heap), which
// upholds `GlobalAlloc`'s contract; the counting is arithmetic on the sizes only.
unsafe impl GlobalAlloc for EngineAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { INNER.alloc(layout) };
        if !p.is_null() {
            counted(layout.size(), 0);
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as above.
        let p = unsafe { INNER.alloc_zeroed(layout) };
        if !p.is_null() {
            counted(layout.size(), 0);
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        counted(0, layout.size());
        // SAFETY: the caller's block and layout, forwarded.
        unsafe { INNER.dealloc(ptr, layout) };
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as above.
        let p = unsafe { INNER.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            counted(new_size, layout.size());
        }
        p
    }
}

/// What Rust holds through [`EngineAllocator`] (debug builds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes allocated and not freed.
    pub allocated: usize,
    /// The most bytes allocated at once.
    pub peak: usize,
    /// Allocations made (reallocations count as one more).
    pub allocations: u64,
}

/// What Rust holds through [`EngineAllocator`]: debug builds only (`None` in release
/// builds, which count nothing), and only when it is the global allocator.
pub fn heap_stats() -> Option<HeapStats> {
    #[cfg(debug_assertions)]
    {
        Some(HeapStats {
            allocated: ALLOCATED.load(Ordering::Relaxed),
            peak: PEAK.load(Ordering::Relaxed),
            allocations: ALLOCATIONS.load(Ordering::Relaxed),
        })
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

#[cfg(feature = "mimalloc")]
unsafe extern "C" {
    // mimalloc's C API (part of every build of the library `libmimalloc-sys` links).
    fn mi_collect(force: bool);
}

/// Gives the memory the calling thread's heap holds back to the OS: what it freed, and
/// what other threads freed into it. mimalloc keeps it until the thread allocates again.
/// Two forced collects: the first takes in what other threads freed and frees the pages
/// that became empty, the second gives back the segments those left empty (one alone left
/// 64–96 of 385 MiB behind in `tests/alloc_release.rs`). Cheap: under a microsecond with
/// nothing to give, about 5 µs after a burst of jobs (ref-1). Does nothing without the
/// `mimalloc` feature.
pub fn release_thread_memory() {
    #[cfg(feature = "mimalloc")]
    // SAFETY: plain calls into the linked mimalloc; they collect the calling thread's heap.
    unsafe {
        mi_collect(true);
        mi_collect(true);
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use super::*;

    static TEST_ALLOCATOR: EngineAllocator = EngineAllocator::new();

    #[test]
    fn allocates_reallocates_and_frees() {
        let layout = Layout::from_size_align(64, 16).unwrap();
        // SAFETY: a valid layout; the block is used within it and freed once.
        unsafe {
            let p = TEST_ALLOCATOR.alloc_zeroed(layout);
            assert!(!p.is_null());
            assert_eq!(p as usize % 16, 0);
            assert!((0..64).all(|i| *p.add(i) == 0));
            *p = 7;
            let q = TEST_ALLOCATOR.realloc(p, layout, 4096);
            assert!(!q.is_null());
            assert_eq!(*q, 7);
            TEST_ALLOCATOR.dealloc(q, Layout::from_size_align(4096, 16).unwrap());
        }
        release_thread_memory();
        #[cfg(debug_assertions)]
        assert!(heap_stats().is_some_and(|s| s.allocations >= 2));
    }
}
