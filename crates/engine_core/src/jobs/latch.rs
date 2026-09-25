//! Completion signals for jobs.
//!
//! A latch is set by the thread that finishes a job and observed by the thread that waits
//! for it. The waiter may free the latch as soon as it observes it set, so `set` takes a
//! raw pointer and reads everything it needs *before* the final atomic operation.

use super::registry::Registry;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, Thread};

pub(crate) trait Latch {
    /// Marks the latch set and wakes its waiter if needed.
    ///
    /// # Safety
    ///
    /// `this` must point to a live latch. The latch may be freed by its owner as soon as
    /// the set becomes visible, so implementations must not touch `*this` afterwards.
    unsafe fn set(this: *const Self);
}

/// Counts outstanding jobs; the owning participant waits for zero while helping.
pub(crate) struct CountLatch {
    counter: AtomicUsize,
    registry: *const Registry,
    owner: usize,
}

impl CountLatch {
    pub(crate) fn new(initial: usize, registry: &Registry, owner: usize) -> Self {
        Self {
            counter: AtomicUsize::new(initial),
            registry,
            owner,
        }
    }

    #[inline]
    pub(crate) fn increment(&self) {
        self.counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Undoes an [`increment`](Self::increment) whose job was never published.
    #[inline]
    pub(crate) fn decrement_unpublished(&self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }

    /// Whether every counted job has finished. `SeqCst` pairs with the waiter's
    /// `SLEEPING` store (see `Registry::sleep`).
    #[inline]
    pub(crate) fn probe(&self) -> bool {
        self.counter.load(Ordering::SeqCst) == 0
    }
}

impl Latch for CountLatch {
    #[inline]
    unsafe fn set(this: *const Self) {
        // SAFETY: `this` is live until the decrement below becomes visible; the fields
        // needed afterwards are copied first.
        unsafe {
            let registry = (*this).registry;
            let owner = (*this).owner;
            if (*this).counter.fetch_sub(1, Ordering::SeqCst) == 1 {
                // The registry outlives every job of the pool.
                (*registry).wake_participant(owner);
            }
        }
    }
}

// SAFETY: the registry pointer is only used to call `&self` methods on a `Sync` type that
// outlives all jobs.
unsafe impl Send for CountLatch {}
// SAFETY: see above; the counter is atomic.
unsafe impl Sync for CountLatch {}

/// Latch for a thread that is not a participant of the pool and blocks in `park`.
pub(crate) struct ThreadLatch {
    set: AtomicBool,
    thread: Thread,
}

impl ThreadLatch {
    pub(crate) fn for_current_thread() -> Self {
        Self {
            set: AtomicBool::new(false),
            thread: thread::current(),
        }
    }

    pub(crate) fn wait(&self) {
        // `park` may return spuriously; an `unpark` issued before `park` is not lost.
        while !self.set.load(Ordering::Acquire) {
            thread::park();
        }
    }
}

impl Latch for ThreadLatch {
    unsafe fn set(this: *const Self) {
        // SAFETY: `this` is live until the store below; the thread handle is cloned
        // first (a reference-count increment, no allocation).
        unsafe {
            let thread = (*this).thread.clone();
            (*this).set.store(true, Ordering::Release);
            thread.unpark();
        }
    }
}
