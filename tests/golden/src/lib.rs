//! Golden-image tests (CLAUDE.md §15): fixed scenes rendered on each reference GPU and
//! compared with images committed per GPU vendor, through FLIP, a perceptual difference
//! (ADR 0062), and a gate on the GPU time of every pass. The shots are in
//! `tests/golden.rs`; this library holds the metric ([`flip`]), the golden images
//! ([`golden`]) and the test bed that renders the shots ([`scene`]); `bin/perf_gate.rs`
//! is the gate, and [`registers`] its register check.

// Test support: a failed render, read or write ends the test with a message (the
// functions document it under `# Panics`).
#![allow(clippy::expect_used)]

mod decal_set;
pub mod flip;
pub mod golden;
pub mod registers;
pub mod scene;
