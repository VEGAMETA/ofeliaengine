//! CLAUDE.md §5: zero heap allocations per frame in steady state, for streaming (ADR
//! 0076): the scene streamer's frame (feedback folded in, instances observed, both
//! streamers planning within the budget, nothing to read), the budget's update, the
//! feedback pass and its readback, with the forward passes they serve.
//!
//! A counting global allocator observes the process after streaming has settled (driver
//! allocations happen in C and are not counted). One test per file, so nothing else
//! allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use common::{Bed, gpu, synthetic_pack};
use engine_core::math::Vec3;
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
fn settled_streaming_frames_do_not_allocate() {
    let pack = synthetic_pack(4);
    let Some((instance, device)) = gpu(false, false) else {
        return;
    };
    let mut bed = Bed::new(instance, device, &pack, 64 << 20);
    // Close to the first quad: its finest detail levels and texture levels.
    let camera = bed.camera(Vec3::new(-2.5, 0.0, -2.8), Vec3::new(-2.5, 0.0, -4.0));
    let frames = bed.settle(camera, 600);
    assert!(frames < 600, "streaming did not settle");
    let stats = bed.streamer.as_ref().unwrap().stats();
    println!(
        "settled in {frames} frames: {} levels arrived ({} of meshes), textures {:.1} MiB",
        stats.arrived,
        stats.mesh_levels,
        bed.textures.memory().0 as f64 / (1 << 20) as f64
    );
    assert!(stats.mesh_levels > 0, "mesh levels streamed");
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..60 {
        bed.frame(camera);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("allocations in 60 settled frames: {allocations}");
    assert_eq!(allocations, 0);
    bed.finish();
    let _ = std::fs::remove_file(pack);
}
