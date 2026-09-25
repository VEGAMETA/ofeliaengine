//! CLAUDE.md §5: zero heap allocations per frame in steady state.
//!
//! A counting global allocator observes every allocation in the process while the pool
//! runs `join`, `scope` and `parallel_for` repeatedly after a warm-up. This file holds a
//! single test so that no other test allocates concurrently.

use engine_core::jobs::ThreadPool;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

fn fib(pool: &ThreadPool, n: u64) -> u64 {
    if n < 10 {
        return n;
    }
    let (a, b) = pool.join(|| fib(pool, n - 1), || fib(pool, n - 2));
    a + b
}

fn frame(pool: &ThreadPool, data: &mut [u64], sums: &[AtomicU64]) {
    std::hint::black_box(fib(pool, 18));
    pool.scope(|s| {
        for chunk in data.chunks_mut(64) {
            s.spawn(move || {
                for v in chunk {
                    *v = v.wrapping_mul(3).wrapping_add(1);
                }
            });
        }
    });
    pool.parallel_for(data.len(), 256, |chunk, range| {
        let sum = data[range].iter().fold(0u64, |a, &v| a.wrapping_add(v));
        sums[chunk].store(sum, Ordering::Relaxed);
    });
}

#[test]
fn steady_state_does_not_allocate() {
    let pool = ThreadPool::builder().workers(3).build().unwrap();
    pool.register_current_thread().unwrap();
    let mut data = vec![1u64; 16 * 1024];
    let sums: Vec<AtomicU64> = (0..data.len().div_ceil(256))
        .map(|_| AtomicU64::new(0))
        .collect();

    // Warm-up: worker start-up, first parks, lazily initialized thread-locals.
    for _ in 0..100 {
        frame(&pool, &mut data, &sums);
    }

    let before = ALLOCATIONS.load(Ordering::SeqCst);
    for _ in 0..500 {
        frame(&pool, &mut data, &sums);
    }
    let after = ALLOCATIONS.load(Ordering::SeqCst);
    assert_eq!(after - before, 0, "allocations in steady state");
}
