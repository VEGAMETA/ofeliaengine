//! CLAUDE.md §5: zero heap allocations per frame in steady state, for GPU memory
//! allocation (resource streaming allocates and frees while frames run).
//!
//! A counting global allocator observes the process while the allocator serves a churn
//! of requests after a warm-up. Blocks stay alive (a base population keeps them in use),
//! so the steady state is pure sub-allocation. This file holds a single test so that no
//! other test allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use engine_rhi::ash::vk;
use engine_rhi::{AllocationRequest, Allocator, MemoryLocation, ResourceKind};
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
fn steady_state_allocation_does_not_touch_the_heap() {
    let Some((_instance, device)) = common::gpu(false) else {
        return;
    };
    let mut allocator = Allocator::new(&device);
    let request = |size: u64, align: u64| AllocationRequest {
        requirements: vk::MemoryRequirements {
            size,
            alignment: align,
            memory_type_bits: u32::MAX,
        },
        location: MemoryLocation::Device,
        kind: ResourceKind::Buffer,
        prefers_dedicated: false,
        requires_dedicated: false,
        dedicated_to: None,
    };
    // Deterministic sizes from 256 B to 256 KiB.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut live = Vec::with_capacity(4096);
    for _ in 0..2048 {
        let size = 256 << (next() % 11);
        live.push(allocator.allocate(&request(size, 256)).unwrap());
    }
    let churn = |allocator: &mut Allocator, live: &mut Vec<_>, next: &mut dyn FnMut() -> u64| {
        for _ in 0..20_000 {
            let victim = (next() % live.len() as u64) as usize;
            allocator.free(live.swap_remove(victim));
            let size = 256 << (next() % 11);
            let align = 256 << (next() % 3);
            live.push(allocator.allocate(&request(size, align)).unwrap());
        }
    };
    // Warm-up: the node pools grow (amortized) to the working set's high-water mark.
    for _ in 0..4 {
        churn(&mut allocator, &mut live, &mut next);
    }
    let blocks = allocator.device_memory_count();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    churn(&mut allocator, &mut live, &mut next);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!(
        "20000 free+allocate pairs: {allocations} heap allocations, {} device memory objects",
        allocator.device_memory_count()
    );
    assert_eq!(allocations, 0);
    assert_eq!(allocator.device_memory_count(), blocks);
    for allocation in live {
        allocator.free(allocation);
    }
}
