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

/// Right-handed view matrix: the camera at `eye` looking at `target`, `up` roughly up.
pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
    glam::camera::rh::view::look_at_mat4(eye, target, up)
}

/// Right-handed orthographic projection for Vulkan (depth 0..1, clip Y down): shadow maps,
/// 2D.
pub fn vulkan_orthographic(
    left: f32,
    right: f32,
    bottom: f32,
    top: f32,
    near: f32,
    far: f32,
) -> Mat4 {
    glam::camera::rh::proj::vulkan::orthographic(left, right, bottom, top, near, far)
}

/// Right-handed perspective projection for Vulkan: view space +Y up and −Z forward, clip
/// depth 0..1 (near..far), clip Y down (Vulkan's convention), so images come out upright.
pub fn vulkan_perspective(fov_y_radians: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    glam::camera::rh::proj::vulkan::perspective(fov_y_radians, aspect, near, far)
}
