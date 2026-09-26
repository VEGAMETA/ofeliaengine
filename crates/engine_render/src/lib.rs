//! Rendering on top of `engine_rhi` (CLAUDE.md §6).
//!
//! - [`graph`]: the render graph. Passes declare what they read and write; the graph
//!   culls unused passes, aliases transient memory, derives barriers and records the
//!   frame (ADR 0011).
//! - [`mesh`]: forward mesh rendering with vertex pulling, the start of the 3D renderer.

pub mod graph;
pub mod mesh;
