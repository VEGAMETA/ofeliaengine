//! engine_render's tests that no steady-state frame allocates (CLAUDE.md §5), in one binary
//! (ADR 0077). A counting global allocator observes the process; the tests take turns
//! ([`alone`]), so nothing else allocates while one counts. Driver and layer allocations
//! happen in C and are not counted: this checks the engine's own code.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

mod atmosphere;
mod gpu_scene;
mod graph;
mod graph_async;
mod local_shadows;
mod reflections;
mod skinning;

struct CountingAllocator;

/// Heap allocations (and reallocations) of the whole process so far.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

static ALONE: Mutex<()> = Mutex::new(());

/// Held for a whole test: the tests of this binary count the process's allocations, so
/// they run one at a time (under cargo-nextest each has a process of its own anyway).
fn alone() -> MutexGuard<'static, ()> {
    ALONE.lock().unwrap_or_else(PoisonError::into_inner)
}
