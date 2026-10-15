//! A global allocator that counts, and the steady-state measurement of the player's frames
//! (CLAUDE.md §5), for the zero-allocation tests: each declares
//! `#[global_allocator] static GLOBAL: counting::CountingAllocator` and runs alone in its
//! file, so nothing else allocates concurrently.

#![allow(dead_code, unreachable_pub)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// While set, the threads that allocate are noted (the first few).
static NOTING: AtomicBool = AtomicBool::new(false);
static THREADS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(access: u32, inherit: i32, id: u32) -> *mut core::ffi::c_void;
    fn GetThreadDescription(thread: *mut core::ffi::c_void, text: *mut *mut u16) -> i32;
    fn LocalFree(memory: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
}

/// A thread's description (its name), when Windows has one.
fn thread_name(id: u32) -> String {
    const THREAD_QUERY_LIMITED_INFORMATION: u32 = 0x0800;
    // SAFETY: plain queries; the text is freed with `LocalFree` as documented and the
    // handle closed.
    unsafe {
        let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id);
        if thread.is_null() {
            return String::from("(gone)");
        }
        let mut text = core::ptr::null_mut();
        let mut name = String::from("(unnamed)");
        if GetThreadDescription(thread, &mut text) >= 0 && !text.is_null() {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            name = String::from_utf16_lossy(core::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
        }
        CloseHandle(thread);
        name
    }
}

fn count() {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    if NOTING.load(Ordering::Relaxed) {
        // SAFETY: a plain query.
        let id = unsafe { GetCurrentThreadId() };
        for slot in &THREADS {
            match slot.compare_exchange(0, id, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(existing) if existing == id => break,
                Err(_) => {}
            }
        }
    }
}

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Runs the player's frames to a steady state, then counts the heap allocations of at
/// least 240 more (and 1.2 s: the window thread's display check, once a second, falls
/// inside) and asserts there are none, naming the threads that allocated.
pub fn assert_steady_state(player: &mut engine_app::Player, game: &mut crate::common::Cubes) {
    // Warm-up, 120 frames and at least 1.5 s: pipelines, the view's state, the graph's
    // buffers and heaps, the window thread's first display checks (once a second).
    let started = Instant::now();
    let mut warm = 0;
    while warm < 120 || started.elapsed() < Duration::from_millis(1500) {
        player.frame(game).unwrap();
        warm += 1;
    }
    // SAFETY: a plain query.
    let frame_thread = unsafe { GetCurrentThreadId() };
    NOTING.store(true, Ordering::Relaxed);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let measured = Instant::now();
    let mut rendered = 0;
    while rendered < 240 || measured.elapsed() < Duration::from_millis(1200) {
        if player.frame(game).unwrap().is_some() {
            rendered += 1;
        }
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    NOTING.store(false, Ordering::Relaxed);
    let threads: Vec<(u32, String)> = THREADS
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .filter(|&t| t != 0)
        .map(|t| (t, thread_name(t)))
        .collect();
    println!(
        "{allocations} allocations in {rendered} frames (after {warm} warm-up frames); allocating threads {threads:?}, the frame thread {frame_thread}"
    );
    assert!(rendered >= 230, "{rendered} frames rendered");
    assert_eq!(allocations, 0, "allocations per frame in steady state");
}
