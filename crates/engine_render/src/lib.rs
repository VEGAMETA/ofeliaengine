//! Rendering on top of `engine_rhi` (CLAUDE.md §6).
//!
//! - [`graph`]: the render graph. Passes declare what they read and write; the graph
//!   culls unused passes, aliases transient memory, derives barriers and records the
//!   frame (ADR 0011).
//! - [`mesh`]: meshes (vertex pulling) and a minimal unlit-ish mesh renderer.
//! - [`forward`]: forward PBR with a directional light and a shadow map (ADR 0014).
//! - [`upload`]: per-frame GPU data (camera, lights, draw lists) in a ring per frame slot.
//! - [`post`]: post-processing (tonemapping from the HDR target).
//! - [`texture`]: textures that live across frames, uploaded through the ring.

pub mod forward;
pub mod graph;
pub mod mesh;
pub mod post;
pub mod texture;
pub mod upload;
