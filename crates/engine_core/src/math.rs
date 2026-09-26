//! Math types (ADR 0012): [`glam`]'s vectors, matrices and quaternions, re-exported so
//! that every crate uses the same types and version.
//!
//! Conventions:
//! - right-handed world space, +Y up;
//! - column vectors (`matrix * vector`), column-major storage (matches Slang/SPIR-V
//!   `column_major` and Vulkan push constants without transposing);
//! - clip space: Vulkan's (depth 0..1, +Y down in NDC) is handled in the projection
//!   helpers of the renderer, not here.
//!
//! Wide SIMD types for physics (SoA, AVX2 by runtime dispatch) will live here too when
//! the physics work starts (CLAUDE.md §8).

pub use glam::{
    Affine3A, EulerRot, IVec2, IVec3, IVec4, Mat3, Mat3A, Mat4, Quat, UVec2, UVec3, UVec4, Vec2,
    Vec3, Vec3A, Vec4,
};
