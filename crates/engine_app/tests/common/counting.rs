//! A global allocator that counts, and the steady-state measurement of the player's frames
//! (CLAUDE.md §5), for the zero-allocation tests: each declares
//! `#[global_allocator] static GLOBAL: counting::CountingAllocator` and runs alone in its
//! file, so nothing else allocates concurrently. A measurement that allocates prints the
//! stacks of its first allocations, so a failure seen once (under a loaded machine) names
//! what allocated.

#![allow(dead_code, unreachable_pub)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// While set, the threads that allocate are noted (the first few).
static NOTING: AtomicBool = AtomicBool::new(false);
static THREADS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
/// The stacks of the first allocations while noting, with their threads.
static TRACES: Mutex<Vec<(u32, Backtrace)>> = Mutex::new(Vec::new());
/// How many stacks are kept, and how many were asked for.
const TRACED: usize = 4;
static TRACE_ASKED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set while this thread captures a stack: what the capture allocates is not counted.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
}

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

/// Heap allocations so far, every thread's.
pub fn allocations() -> usize {
    ALLOCATIONS.load(Ordering::Relaxed)
}

fn count() {
    if CAPTURING.try_with(Cell::get).unwrap_or(false) {
        return;
    }
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
        // The stack, resolved only if the measurement fails.
        if TRACE_ASKED.fetch_add(1, Ordering::Relaxed) < TRACED
            && CAPTURING.try_with(|c| c.set(true)).is_ok()
        {
            let trace = Backtrace::force_capture();
            if let Ok(mut traces) = TRACES.lock() {
                traces.push((id, trace));
            }
            CAPTURING.with(|c| c.set(false));
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
    assert_steady_state_once(player, game, |_| true);
}

/// [`assert_steady_state`], the warm-up also lasting until `settled` says the scene has
/// settled (a GI volume converged: its convergence captures the probes again, once).
pub fn assert_steady_state_once(
    player: &mut engine_app::Player,
    game: &mut crate::common::Cubes,
    settled: impl Fn(&engine_app::Player) -> bool,
) {
    assert_steady_state_each(player, game, settled, |_| {});
}

/// [`assert_steady_state_once`], `each` run on the player before every frame (a pose moved,
/// as the editor moves it).
#[allow(dead_code)]
pub fn assert_steady_state_each(
    player: &mut engine_app::Player,
    game: &mut crate::common::Cubes,
    settled: impl Fn(&engine_app::Player) -> bool,
    mut each: impl FnMut(&mut engine_app::Player),
) {
    // Warm-up, 120 frames and at least 1.5 s: pipelines, the view's state, the graph's
    // buffers and heaps, the window thread's first display checks (once a second). And
    // half a second past the last slow frame, 20 s at most: with a cold driver cache the
    // first frames compile pipelines for seconds, and what settles after them (2026-10-15:
    // 6 allocations after 120 frames that took the whole 1.5 s) is not the steady state.
    let started = Instant::now();
    let mut last_slow = started;
    let mut warm = 0;
    while (warm < 120
        || started.elapsed() < Duration::from_millis(1500)
        || last_slow.elapsed() < Duration::from_millis(500)
        || !settled(player))
        && started.elapsed() < Duration::from_secs(20)
    {
        let frame = Instant::now();
        each(player);
        player.frame(game).unwrap();
        if frame.elapsed() > Duration::from_millis(50) {
            last_slow = Instant::now();
        }
        warm += 1;
    }
    // SAFETY: a plain query.
    let frame_thread = unsafe { GetCurrentThreadId() };
    if let Ok(mut traces) = TRACES.lock() {
        traces.clear();
        traces.reserve(TRACED);
    }
    TRACE_ASKED.store(0, Ordering::Relaxed);
    NOTING.store(true, Ordering::Relaxed);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let measured = Instant::now();
    let mut rendered = 0;
    while rendered < 240 || measured.elapsed() < Duration::from_millis(1200) {
        each(player);
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
    if allocations > 0
        && let Ok(traces) = TRACES.lock()
    {
        for (k, (thread, trace)) in traces.iter().enumerate() {
            println!(
                "allocation {} of the measurement, thread {thread}:\n{trace}",
                k + 1
            );
        }
    }
    assert!(rendered >= 230, "{rendered} frames rendered");
    assert_eq!(allocations, 0, "allocations per frame in steady state");
}
