//! engine_net's tests in one binary (ADR 0077): one link for all of them instead of one
//! per file. Each module was a test file of its own.

#[path = "../common/mod.rs"]
mod common;

mod coop;
mod replication;
