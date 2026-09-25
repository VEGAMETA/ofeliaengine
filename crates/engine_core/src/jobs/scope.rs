//! Structured spawning of borrowed jobs.

use super::job::{JobHeader, JobRef, Panic, execute};
use super::latch::{CountLatch, Latch};
use super::registry::Registry;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use std::panic::{self, AssertUnwindSafe};

/// Completion state shared between a scope and its jobs.
pub(crate) struct ScopeCore {
    pub(crate) latch: CountLatch,
    panic: AtomicPtr<Panic>,
}

impl ScopeCore {
    pub(crate) fn new(registry: &Registry, owner: usize) -> Self {
        Self {
            latch: CountLatch::new(0, registry, owner),
            panic: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Keeps the first panic; later ones are dropped. Allocates only on this cold path.
    pub(crate) fn record_panic(&self, payload: Panic) {
        let boxed = Box::into_raw(Box::new(payload));
        if self
            .panic
            .compare_exchange(ptr::null_mut(), boxed, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: `boxed` was not published, so this thread still owns it.
            drop(unsafe { Box::from_raw(boxed) });
        }
    }

    pub(crate) fn take_panic(&self) -> Option<Panic> {
        let p = self.panic.swap(ptr::null_mut(), Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            // SAFETY: non-null pointers in `panic` come from `Box::into_raw` above and are
            // taken out exactly once by the swap.
            Some(*unsafe { Box::from_raw(p) })
        }
    }
}

impl Drop for ScopeCore {
    fn drop(&mut self) {
        drop(self.take_panic());
    }
}

/// A spawned job, stored in the spawning thread's arena.
#[repr(C)]
struct ScopeJob<F> {
    header: JobHeader,
    core: *const ScopeCore,
    func: ManuallyDrop<F>,
}

impl<F: FnOnce()> ScopeJob<F> {
    unsafe fn execute_raw(job: JobRef) {
        let this = job.cast::<Self>().as_ptr();
        // SAFETY: `job` points to a `ScopeJob<F>` written by `Scope::spawn`, executed
        // exactly once; its scope waits for the latch before the arena memory is reused.
        unsafe {
            let core = (*this).core;
            let func = ManuallyDrop::take(&mut (*this).func);
            if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(func)) {
                (*core).record_panic(payload);
            }
            CountLatch::set(&raw const (*core).latch);
        }
    }
}

/// A scope for spawning jobs that borrow from the enclosing stack frame.
///
/// Created by [`ThreadPool::scope`](super::ThreadPool::scope). Every job spawned in the
/// scope finishes before `scope` returns, so jobs may borrow anything that outlives the
/// scope. The scope waits by running jobs itself, never by blocking idle.
///
/// Spawning is only possible from the thread that owns the scope: `&Scope` is not `Send`,
/// so jobs cannot capture it. For recursive parallelism open a nested scope (or use
/// `join`) inside the job.
///
/// Job records are placed in a per-thread arena and the arena is rewound when the scope
/// ends, so spawning allocates nothing. If the arena is full, the job runs inline at the
/// `spawn` call.
pub struct Scope<'scope, 'env: 'scope> {
    registry: *const Registry,
    index: usize,
    pub(crate) core: ScopeCore,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

impl<'scope, 'env> Scope<'scope, 'env> {
    pub(crate) fn new(registry: &Registry, index: usize) -> Self {
        Self {
            registry,
            index,
            core: ScopeCore::new(registry, index),
            _scope: PhantomData,
            _env: PhantomData,
        }
    }

    /// Spawns a job into the scope.
    ///
    /// # Examples
    ///
    /// ```
    /// use engine_core::jobs::ThreadPool;
    ///
    /// let pool = ThreadPool::builder().workers(2).build().unwrap();
    /// let mut halves = [0u64; 2];
    /// let data: Vec<u64> = (1..=100).collect();
    /// let (lo, hi) = halves.split_at_mut(1);
    /// pool.scope(|s| {
    ///     s.spawn(|| lo[0] = data[..50].iter().sum());
    ///     s.spawn(|| hi[0] = data[50..].iter().sum());
    /// });
    /// assert_eq!(halves[0] + halves[1], 5050);
    /// ```
    pub fn spawn<F>(&'scope self, f: F)
    where
        F: FnOnce() + Send + 'scope,
    {
        // SAFETY: the registry outlives the scope (the pool is borrowed for the duration
        // of `ThreadPool::scope`).
        let registry = unsafe { &*self.registry };
        let slot = &registry.slots[self.index];
        let Some(memory) = slot.owner.arena.alloc(Layout::new::<ScopeJob<F>>()) else {
            self.run_inline(f);
            return;
        };
        let job = memory.cast::<ScopeJob<F>>();
        // SAFETY: `memory` is fresh, properly aligned arena memory for a `ScopeJob<F>`.
        unsafe {
            job.as_ptr().write(ScopeJob {
                header: JobHeader::new(ScopeJob::<F>::execute_raw),
                core: &raw const self.core,
                func: ManuallyDrop::new(f),
            });
        }
        let job_ref: JobRef = job.cast::<JobHeader>();
        self.core.latch.increment();
        // SAFETY: `Scope` is neither `Send` nor `Sync`, so this is the owner thread of
        // slot `self.index`.
        match unsafe { slot.deque.push(job_ref) } {
            Ok(()) => registry.notify_work(),
            // Deque full: run it now. SAFETY: the job was never published.
            Err(job_ref) => unsafe { execute(job_ref) },
        }
    }

    fn run_inline<F: FnOnce()>(&self, f: F) {
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(f)) {
            self.core.record_panic(payload);
        }
    }
}
