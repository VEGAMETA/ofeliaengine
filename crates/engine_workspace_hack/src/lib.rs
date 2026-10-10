//! No code: this crate exists for its dependencies' features (see its `Cargo.toml` and
//! ADR 0077). Every engine crate depends on it through `engine_core`, so building any set of
//! packages resolves the shared dependencies listed there exactly as the whole workspace
//! does, and reuses the workspace's compiled crates.
#![no_std]
