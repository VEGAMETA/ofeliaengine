//! Atomics used by the lock-free queues.
//!
//! Under `--cfg loom` they are replaced by loom's instrumented versions so the queues can
//! be model-checked (`RUSTFLAGS="--cfg loom" cargo test -p engine_core --release loom`).

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicIsize, AtomicPtr, AtomicUsize, Ordering, fence};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicIsize, AtomicPtr, AtomicUsize, Ordering, fence};
