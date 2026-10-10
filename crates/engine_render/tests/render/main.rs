//! engine_render's tests in one binary (ADR 0077): one link for all of them
//! instead of one per file. Each module was a test file of its own.

mod ao;
mod async_compute;
mod atmosphere;
mod breadcrumbs;
mod culling;
mod environment;
mod feedback;
mod forward;
mod geometry;
mod gpu_scene;
mod graph;
mod hiz;
mod lights;
mod local_shadows;
mod lods;
mod materials;
mod mesh_streaming;
mod motion;
mod occlusion;
mod post;
mod reflections;
mod shaders;
mod shadows;
mod skinning;
mod streaming;
mod subsurface;
mod tonemap;
mod transparency;
