//! Engine foundation: math (SIMD), memory (arenas, pools), containers, job system,
//! time, hashing.
//!
//! This crate has no dependencies on the OS layer (`engine_platform`) or on any
//! graphics code. Everything here must be usable from any engine thread, including
//! the render and audio threads.

pub mod arena;
mod atomic;
pub mod half;
pub mod hash;
pub mod jobs;
pub mod math;
pub mod spsc;
pub mod sync;
