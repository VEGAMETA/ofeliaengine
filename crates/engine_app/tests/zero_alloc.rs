//! CLAUDE.md §5: zero heap allocations per frame in steady state, for the player's whole
//! frame (ADR 0099): the window's events, latency control, the slot and swapchain image,
//! the runtime's ticks and extraction, the game's hooks, the frame renderer and graph,
//! submission and presentation. One test per file, so nothing else allocates
//! concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

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

#[test]
fn the_players_steady_state_frames_do_not_allocate() {
    let Some((mut player, mut game)) = common::player() else {
        return;
    };
    // Warm-up: pipelines, the view's state, the graph's buffers and heaps.
    for _ in 0..120 {
        player.frame(&mut game).unwrap();
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut rendered = 0;
    for _ in 0..120 {
        if player.frame(&mut game).unwrap().is_some() {
            rendered += 1;
        }
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("{allocations} allocations in {rendered} frames");
    assert!(rendered >= 115, "{rendered} frames rendered");
    assert_eq!(allocations, 0, "allocations per frame in steady state");
}
