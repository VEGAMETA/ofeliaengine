//! Rendering on top of `engine_rhi` (CLAUDE.md §6).
//!
//! - [`graph`]: the render graph. Passes declare what they read and write; the graph
//!   culls unused passes, aliases transient memory, derives barriers and records the
//!   frame (ADR 0011).
//! - [`mesh`]: the geometry pool: every mesh's vertices and indices in shared buffers,
//!   compact vertex attributes (ADR 0039).
//! - [`material`]: glTF metallic-roughness materials in one GPU table, textures by
//!   bindless index (ADR 0040).
//! - [`texture_pool`]: material textures with full mip chains, uploaded within a budget of
//!   bytes per frame (ADR 0040).
//! - [`forward`]: forward PBR with a directional light and a shadow map (ADR 0014), drawn
//!   GPU-driven: GPU frustum culling and indirect instanced draws (ADR 0034).
//! - [`contact`]: contact shadows of the sun from the depth buffer (ADR 0071).
//! - [`local_shadows`]: shadows of point and spot lights in a cached atlas (ADR 0071).
//! - [`skinning`]: skinned meshes skinned by a compute pass into the geometry pool, and
//!   their motion vectors (ADR 0068).
//! - [`atmosphere`]: a physically based sky and atmosphere, aerial perspective and
//!   exponential height fog (ADR 0069).
//! - [`reflections`]: local reflections: reflection probes and screen-space reflections
//!   (ADR 0073).
//! - [`subsurface`]: screen-space subsurface scattering: diffusion profiles, transmission
//!   and the scatter pass (ADR 0059).
//! - [`gpu_scene`]: the persistent GPU scene: instances in stable slots in device
//!   buffers, only changes uploaded each frame (ADR 0064).
//! - [`cull`]: frustum and bounding-box tests shared by CPU code and the GPU culling pass.
//! - [`upload`]: per-frame GPU data (camera, lights, draw lists) in a ring per frame slot.
//! - [`post`]: post-processing (tonemapping from the HDR target).
//! - [`png`]: a small PNG encoder for screenshots and golden-image tests.
//! - [`texture`]: textures that live across frames, uploaded through the ring.
//! - [`viewport`]: viewports: render targets with their own size, update rate and
//!   placement; nesting, split screen, pixel-perfect scaling (ADR 0016).

pub mod ao;
pub mod atmosphere;
pub mod compute;
pub mod contact;
pub mod cull;
pub mod environment;
pub mod forward;
pub mod gpu_scene;
pub mod graph;
pub mod hiz;
pub mod lights;
pub mod local_shadows;
pub mod material;
pub mod mesh;
pub mod png;
pub mod post;
pub mod postfx;
pub mod reflections;
pub mod shadow;
pub mod skinning;
pub mod subsurface;
pub mod texture;
pub mod texture_pool;
pub mod upload;
pub mod viewport;
