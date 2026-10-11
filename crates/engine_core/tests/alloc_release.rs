//! The engine allocator gives memory back (ADR 0086): what the job pool's workers allocated
//! and another thread freed stays in the workers' heaps while they sleep, until
//! `ThreadPool::release_memory` asks them for it. Its own process, so its private bytes
//! are its own.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::alloc::{EngineAllocator, release_thread_memory};
use engine_core::jobs::ThreadPool;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOCATOR: EngineAllocator = EngineAllocator::new();

/// `PROCESS_MEMORY_COUNTERS_EX` (psapi.h).
#[repr(C)]
#[derive(Default)]
struct Counters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn K32GetProcessMemoryInfo(
        process: *mut core::ffi::c_void,
        counters: *mut Counters,
        cb: u32,
    ) -> i32;
}

/// The process's private bytes, MiB.
fn private_mib() -> f64 {
    let mut c = Counters {
        cb: size_of::<Counters>() as u32,
        ..Counters::default()
    };
    // SAFETY: the pseudo handle of this process; `c` has its size in `cb`.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &raw mut c, c.cb) };
    c.private_usage as f64 / (1 << 20) as f64
}

/// Waits `ms` milliseconds (the workers wake, collect and sleep again meanwhile).
fn spin_wait(ms: u64) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        std::thread::yield_now();
    }
}

#[test]
fn memory_workers_allocated_goes_back_when_the_pool_is_asked() {
    let pool = ThreadPool::builder().workers(4).build().unwrap();
    release_thread_memory();
    let before = private_mib();
    // Each of 64 jobs allocates 4 MiB in small blocks, kept by the main thread: a level's
    // data, loaded on the pool.
    let kept: Mutex<Vec<Vec<Box<[u8]>>>> = Mutex::new(Vec::new());
    pool.parallel_for(64, 1, |_, range| {
        for _ in range {
            let blocks: Vec<Box<[u8]>> = (0..1024).map(|i| vec![i as u8; 4096].into()).collect();
            kept.lock().unwrap().push(blocks);
        }
    });
    let loaded = private_mib();
    // The workers sleep now. The main thread frees the data: it goes back to the heaps of
    // the workers that allocated it, which do not look while they sleep.
    drop(kept);
    release_thread_memory();
    spin_wait(50);
    let freed = private_mib();
    pool.release_memory();
    spin_wait(200);
    let released = private_mib();
    println!(
        "private MiB: before {before:.0}, loaded {loaded:.0}, freed on the main thread {freed:.0}, after release_memory {released:.0}"
    );
    assert!(loaded - before > 200.0, "the data: {before} -> {loaded}");
    assert!(
        freed - before > 100.0,
        "kept by the sleeping workers' heaps: {before} -> {freed}"
    );
    assert!(
        released - before < 32.0,
        "given back: {before} -> {released} (before release_memory {freed})"
    );
}
