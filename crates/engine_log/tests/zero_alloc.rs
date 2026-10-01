//! CLAUDE.md §5: logging does not allocate on the logging thread (formatting goes into a
//! buffer on the stack, the record into the preallocated ring). One test per file.

#![allow(clippy::unwrap_used)]

use engine_log::LogConfig;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Counts only the test thread: the writer thread owns its buffers.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn logging_does_not_allocate() {
    let _guard = engine_log::init(&LogConfig {
        filter: Some(String::from("debug")),
        stderr: false,
        debugger: false,
        file: None,
        tracy: true,
        capacity: 256,
    })
    .unwrap();
    let name = "forward";
    let long = "y".repeat(3000);
    // Warm-up: the thread handle, lazily created statics.
    log::warn!("warm-up {name}");
    COUNTING.with(|c| c.set(true));
    for i in 0..10_000u32 {
        log::info!("frame {i}: pass {name} took {:.3} ms", f64::from(i) * 0.01);
        log::debug!(target: "engine_render::graph", "{i:>8} {name:?}");
        log::trace!("filtered out {i}");
        if i % 100 == 0 {
            log::warn!("long {long}");
        }
    }
    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
}
