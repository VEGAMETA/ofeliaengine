//! Atomics used by the lock-free queues (`jobs`, `spsc`) and the foreign jobs' free list.
//!
//! Under `--cfg loom` they are replaced by loom's instrumented versions so the queues can
//! be model-checked (`RUSTFLAGS="--cfg loom" cargo test -p engine_core --release loom`).

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{
    AtomicIsize, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{
    AtomicIsize, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

/// `UnsafeCell` whose accesses loom can track under `--cfg loom`.
#[cfg(loom)]
pub(crate) use loom::cell::UnsafeCell;

/// `UnsafeCell` with loom's access API (`with_mut`), so the same code checks under loom.
#[cfg(not(loom))]
#[derive(Debug)]
pub(crate) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

#[cfg(not(loom))]
impl<T> UnsafeCell<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(core::cell::UnsafeCell::new(value))
    }

    #[inline(always)]
    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}
