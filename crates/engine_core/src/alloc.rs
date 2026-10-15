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
#[cfg(feature = "mimalloc")]
use core::sync::atomic::AtomicBool;
#[cfg(any(debug_assertions, feature = "mimalloc"))]
use core::sync::atomic::Ordering;
#[cfg(debug_assertions)]
use core::sync::atomic::{AtomicU64, AtomicUsize};

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
    fn mi_thread_init();
    fn mi_collect(force: bool);
}

/// Set by a thread that collected its heap in [`release_thread_memory`]: a purge has to
/// start after that.
#[cfg(feature = "mimalloc")]
static PURGE_ASKED: AtomicBool = AtomicBool::new(false);
/// Held by the one thread purging for [`release_thread_memory`].
#[cfg(feature = "mimalloc")]
static PURGING: AtomicBool = AtomicBool::new(false);

/// Gives the memory the calling thread's heap holds back to the OS: what it freed, and
/// what other threads freed into it. mimalloc keeps it until the thread allocates again.
/// Lock-free, and cheap: under a microsecond with nothing to give, about 5 µs after a
/// burst of jobs (ref-1). Does nothing without the `mimalloc` feature.
///
/// A forced collect frees the heap's empty pages and segments, then purges (decommits)
/// the free parts of mimalloc's arenas. mimalloc lets one thread purge at a time; one
/// asking meanwhile goes on without purging. So when threads gave memory back together
/// (the job pool's workers on `ThreadPool::release_memory`), what one freed after the
/// purging one had passed stayed committed until some thread allocated again: 64–128 MiB
/// in `tests/alloc_release.rs`, in 11 of 72 runs with 24 copies running at once (ADR 0086).
/// Here the purges run one at a time, and again until one has started after every
/// caller's collect.
pub fn release_thread_memory() {
    #[cfg(feature = "mimalloc")]
    {
        // SAFETY: plain calls into the linked mimalloc. The first sets up the calling
        // thread's heap if it never allocated (a collect skips a thread without one, purge
        // included); the second collects that heap.
        unsafe {
            mi_thread_init();
            mi_collect(true);
        }
        // SeqCst throughout: either the purging thread sees this request once it has let
        // go, or this thread takes the purge over.
        PURGE_ASKED.store(true, Ordering::SeqCst);
        while PURGE_ASKED.load(Ordering::SeqCst) && !PURGING.swap(true, Ordering::SeqCst) {
            PURGE_ASKED.store(false, Ordering::SeqCst);
            // SAFETY: as above; a forced collect is mimalloc's public way to purge.
            unsafe { mi_collect(true) };
            PURGING.store(false, Ordering::SeqCst);
        }
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
