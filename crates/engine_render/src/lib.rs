//! Rendering on top of `engine_rhi` (CLAUDE.md §6).
//!
//! - [`graph`]: the render graph. Passes declare what they read and write; the graph
//!   culls unused passes, aliases transient memory, derives barriers and records the
//!   frame (ADR 0011).
//! - [`mesh`]: meshes (vertex pulling) and a minimal unlit-ish mesh renderer.
//! - [`forward`]: forward PBR with a directional light and a shadow map (ADR 0014), drawn
//!   GPU-driven: GPU frustum culling and indirect instanced draws (ADR 0034).
//! - [`cull`]: frustum and bounding-box tests shared by CPU code and the GPU culling pass.
//! - [`upload`]: per-frame GPU data (camera, lights, draw lists) in a ring per frame slot.
//! - [`post`]: post-processing (tonemapping from the HDR target).
//! - [`texture`]: textures that live across frames, uploaded through the ring.
//! - [`viewport`]: viewports: render targets with their own size, update rate and
//!   placement; nesting, split screen, pixel-perfect scaling (ADR 0016).

pub mod cull;
pub mod forward;
pub mod graph;
pub mod mesh;
pub mod post;
pub mod texture;
pub mod upload;
pub mod viewport;
