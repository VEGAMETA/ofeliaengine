//! engine_physics_jolt's tests in one binary (ADR 0077): one link for all of them instead of one
//! per file. Each module was a test file of its own.

mod characters;
mod ecs_sync;
mod jolt;
mod queries;
mod rollback;
