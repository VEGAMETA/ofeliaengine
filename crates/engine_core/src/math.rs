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
    Affine2, Affine3A, EulerRot, IVec2, IVec3, IVec4, Mat2, Mat3, Mat3A, Mat4, Quat, UVec2, UVec3,
    UVec4, Vec2, Vec3, Vec3A, Vec4,
};
/// Double precision, for geometry processing at cook time (convex hulls and fitting,
/// ADR 0101), where f32's rounding decides which points lie on a plane.
pub use glam::{DMat3, DQuat, DVec2, DVec3};

/// Right-handed view matrix: the camera at `eye` looking at `target`, `up` roughly up.
pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
    glam::camera::rh::view::look_at_mat4(eye, target, up)
}

/// Right-handed orthographic projection for Vulkan (depth 0..1, clip Y down): shadow maps,
/// 2D. `top` maps to the top of the target, also when `top < bottom` (y-down pixel
/// space).
///
/// Built as the Y-up (DirectX) projection followed by a Y flip: glam 0.33.10's
/// `vulkan::orthographic` flips the Y scale but not the Y offset, which is wrong whenever
/// `bottom + top != 0`.
pub fn vulkan_orthographic(
    left: f32,
    right: f32,
    bottom: f32,
    top: f32,
    near: f32,
    far: f32,
) -> Mat4 {
    Mat4::from_scale(Vec3::new(1.0, -1.0, 1.0))
        * glam::camera::rh::proj::directx::orthographic(left, right, bottom, top, near, far)
}

/// Right-handed perspective projection for Vulkan: view space +Y up and −Z forward, clip
/// depth 0..1 (near..far), clip Y down (Vulkan's convention), so images come out upright.
pub fn vulkan_perspective(fov_y_radians: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    glam::camera::rh::proj::vulkan::perspective(fov_y_radians, aspect, near, far)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vulkan_orthographic_maps_bounds_to_clip_edges() {
        // Y up, symmetric and not.
        for (bottom, top) in [(-5.0, 5.0), (0.0, 100.0), (20.0, 30.0), (100.0, 0.0)] {
            let m = vulkan_orthographic(-1.0, 3.0, bottom, top, -1.0, 1.0);
            let clip = |x: f32, y: f32| m * Vec4::new(x, y, 0.0, 1.0);
            // Left/right → −1/+1; top → −1 (top of a Vulkan target), bottom → +1.
            assert!((clip(-1.0, top).x + 1.0).abs() < 1e-6);
            assert!((clip(3.0, top).x - 1.0).abs() < 1e-6);
            assert!((clip(0.0, top).y + 1.0).abs() < 1e-6, "{bottom} {top}");
            assert!((clip(0.0, bottom).y - 1.0).abs() < 1e-6, "{bottom} {top}");
            // Depth: z = −near → 0, z = −far → 1 (right-handed, looking down −z).
            assert!((m * Vec4::new(0.0, 0.0, 1.0, 1.0)).z.abs() < 1e-6);
            assert!(((m * Vec4::new(0.0, 0.0, -1.0, 1.0)).z - 1.0).abs() < 1e-6);
        }
    }
}
