//! Atomics of the ring. Under `--cfg loom` they are loom's instrumented versions, so the
//! ring can be model-checked (`RUSTFLAGS="--cfg loom" cargo test -p engine_log --release --lib loom`).

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU64, Ordering, fence};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU64, Ordering, fence};
