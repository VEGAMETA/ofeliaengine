//! Job system: a work-stealing thread pool for fork-join parallelism.
//!
//! Design goals (CLAUDE.md §5, ADR 0002):
//!
//! - **Zero allocations in steady state.** Deques and the injector have fixed capacity;
//!   job records live on the waiting thread's stack (`join`, `parallel_for`) or in a
//!   per-thread arena rewound at scope end (`Scope::spawn`). Every overflow degrades to
//!   running the job inline.
//! - **No locks.** Queues are lock-free; idle threads park on `WaitOnAddress` (futex on
//!   Linux) via `std::thread::park`, with Go-style throttling of wake-ups.
//! - **Deterministic data parallelism.** `parallel_for` splits work into fixed chunks
//!   that do not depend on the thread count or on scheduling.
//! - **Waiting helps.** A thread waiting for its jobs runs other jobs; there are no
//!   fibers (see the ADR for why).
//! - **Foreign jobs** ([`ForeignJobs`]): a C library's task callbacks (Jolt's) hand jobs
//!   to the pool from any thread, in a fixed set of records.
//!
//! ```
//! use engine_core::jobs::ThreadPool;
//!
//! let pool = ThreadPool::builder().workers(3).build().unwrap();
//! pool.register_current_thread().unwrap(); // the main thread helps too
//!
//! let mut squares = vec![0u32; 1000];
//! pool.scope(|s| {
//!     for (i, chunk) in squares.chunks_mut(250).enumerate() {
//!         s.spawn(move || {
//!             for (j, v) in chunk.iter_mut().enumerate() {
//!                 let x = (i * 250 + j) as u32;
//!                 *v = x * x;
//!             }
//!         });
//!     }
//! });
//! assert_eq!(squares[999], 999 * 999);
//! ```

mod arena;
mod deque;
mod foreign;
mod injector;
mod job;
mod latch;
mod pool;
mod registry;
mod scope;

pub use foreign::{ForeignFn, ForeignJobs};
pub use pool::{BuildError, RegisterError, ThreadPool, ThreadPoolBuilder};
pub use scope::Scope;

#[cfg(test)]
#[cfg(not(loom))]
mod tests;
