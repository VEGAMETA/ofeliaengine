//! The C interface of `csrc/brixelizer.cpp`, Brixelizer and Brixelizer GI (ADR 0090). The
//! structures mirror the C++ ones field by field; `layout_matches_the_interface` checks
//! sizes and offsets against the compiled code.
//!
//! - A [`Context`] is the scene's sparse distance field: cascades of 64³ voxels around a
//!   center, each voxel a brick of 8³ distances in a 512³ atlas, built on the GPU from the
//!   instances' triangles, one cascade per update. It owns the SDK's backend.
//! - A [`GiContext`] is one view's GI over a [`Context`]'s distance field: diffuse and
//!   specular light at the view's size.

use crate::{Device, Image};
use engine_rhi::ash::vk;

/// Cascade slots of the SDK (static, dynamic and merged ones together).
pub const MAX_CASCADES: u32 = 24;
/// Voxels along a cascade's edge.
pub const CASCADE_RESOLUTION: u32 = 64;
/// Instances a context holds.
pub const MAX_INSTANCES: u32 = 1 << 16;

/// `FfxBrixelizerGIInternalResolution`: the GI's internal size against the view's.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalResolution {
    /// The view's size.
    Native = 0,
    /// 75 % per axis.
    Percent75 = 1,
    /// 50 % per axis.
    Percent50 = 2,
    /// 25 % per axis.
    Percent25 = 3,
}

/// `EngineBrixelizerCreateDesc`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CreateDesc {
    /// The device the context is created on.
    pub device: Device,
    /// Where the cascades start centered.
    pub center: [f32; 3],
    /// Cascades (at most 8 with dynamic ones, 24 without).
    pub cascade_count: u32,
    /// Cascade 0's voxel (brick) edge, world units.
    pub voxel_size: f32,
    /// Each cascade's voxel size over the previous one's.
    pub cascade_ratio: f32,
    /// Static and dynamic cascades, merged into the traced ones (else static only).
    pub dynamic: u32,
    /// Triangle references one update may store.
    pub max_references: u32,
    /// Bytes of triangle swap space one update may use.
    pub triangle_swap_size: u32,
    /// Bricks one update may build.
    pub max_bricks_per_bake: u32,
    /// Reads the build's statistics back from the GPU ([`engine_brixelizer_stats`]).
    pub readback: u32,
    /// GI contexts the context's backend can hold.
    pub max_gi_contexts: u32,
}

/// `EngineBrixelizerInstance`: a mesh placed in the scene.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Instance {
    /// World-space bounds.
    pub aabb_min: [f32; 3],
    /// World-space bounds.
    pub aabb_max: [f32; 3],
    /// Object to world, row-major 3 × 4.
    pub transform: [f32; 12],
    /// A registered buffer of 32-bit indices ([`engine_brixelizer_register_buffer`]).
    pub index_buffer: u32,
    /// Byte offset of the first index.
    pub index_offset: u32,
    /// Triangles.
    pub triangle_count: u32,
    /// A registered buffer of positions, three floats each.
    pub vertex_buffer: u32,
    /// Byte offset of the first vertex (indices count from it).
    pub vertex_offset: u32,
    /// Bytes from one position to the next.
    pub vertex_stride: u32,
    /// Vertices.
    pub vertex_count: u32,
    /// Rebuilt every frame in the dynamic cascades (added again every frame).
    pub dynamic: u32,
}

/// `EngineBrixelizerUpdateDesc`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UpdateDesc {
    /// Where the update is recorded.
    pub command_buffer: vk::CommandBuffer,
    /// The cascades' center (the camera).
    pub center: [f32; 3],
    /// The frame's number (it picks the cascade updated).
    pub frame_index: u32,
    /// A debug view of the distance field traced from a camera into `debug_output`
    /// (`GENERAL`): `FfxBrixelizerTraceDebugModes` + 1 (1 distance, 2 UVW, 3 iterations, 4
    /// normals, 5 bricks, 6 cascades), or 0 for none.
    pub debug_mode: u32,
    /// The camera's world from view, as the GI's matrices (row-vector convention).
    pub inverse_view: [f32; 16],
    /// Its view from clip.
    pub inverse_projection: [f32; 16],
    /// Padding.
    pub pad: u32,
    /// The debug view's image.
    pub debug_output: Image,
}

/// `EngineBrixelizerStats`: an update's numbers, read back some frames late.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
#[allow(missing_docs)]
pub struct Stats {
    pub cascade_index: u32,
    pub static_triangles: u32,
    pub static_references: u32,
    pub static_bricks: u32,
    pub dynamic_triangles: u32,
    pub dynamic_references: u32,
    pub dynamic_bricks: u32,
    pub brick_allocations_attempted: u32,
    pub brick_allocations_succeeded: u32,
    pub bricks_cleared: u32,
    pub bricks_merged: u32,
    pub free_bricks: u32,
    /// Bytes of scratch the last update needed.
    pub scratch_needed: u64,
    /// Bytes of scratch allocated.
    pub scratch_size: u64,
}

/// `EngineFfxCube`: a cube texture, sampled at its first level.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Cube {
    /// The image (six layers).
    pub image: vk::Image,
    /// Its format.
    pub format: vk::Format,
    /// A face's size.
    pub size: u32,
    /// Padding.
    pub pad: u32,
}

/// `EngineBrixelizerGiCreateDesc`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GiCreateDesc {
    /// The view's size (the outputs').
    pub width: u32,
    /// The view's size.
    pub height: u32,
    /// The GI's internal size.
    pub internal_resolution: InternalResolution,
    /// Specular GI traced (else its output stays black).
    pub specular: u32,
}

/// `EngineBrixelizerGiDispatchDesc`. Matrices in the row-vector convention, row by row: a
/// column-vector matrix (`glam`) stored column by column.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(missing_docs)]
pub struct GiDispatchDesc {
    pub command_buffer: vk::CommandBuffer,
    pub view: [f32; 16],
    pub projection: [f32; 16],
    pub prev_view: [f32; 16],
    pub prev_projection: [f32; 16],
    pub camera_position: [f32; 3],
    /// The traced cascades, most detailed first (0 .. cascade count − 1).
    pub start_cascade: u32,
    pub end_cascade: u32,
    pub ray_pushoff: f32,
    pub sdf_solve_eps: f32,
    pub specular_ray_pushoff: f32,
    pub specular_sdf_solve_eps: f32,
    pub t_min: f32,
    pub t_max: f32,
    pub environment_intensity: f32,
    pub motion_vector_scale: [f32; 2],
    pub normals_unpack_mul: f32,
    pub normals_unpack_add: f32,
    pub roughness_channel: u32,
    pub roughness_perceptual: u32,
    pub roughness_threshold: f32,
    pub pad: u32,
    /// Inputs, in `SHADER_READ_ONLY_OPTIMAL`.
    pub depth: Image,
    pub history_depth: Image,
    pub normal: Image,
    pub history_normal: Image,
    pub roughness: Image,
    pub motion_vectors: Image,
    pub prev_lit: Image,
    pub noise: Image,
    pub environment: Cube,
    /// Outputs, in `GENERAL`.
    pub diffuse: Image,
    pub specular: Image,
}

/// `EngineBrixelizerGiDebugDesc`: the SDK's view of the GI's caches at each pixel's
/// surface.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(missing_docs)]
pub struct GiDebugDesc {
    pub command_buffer: vk::CommandBuffer,
    pub view: [f32; 16],
    pub projection: [f32; 16],
    /// 0: the radiance cache; 1: the irradiance cache.
    pub mode: u32,
    pub normals_unpack_mul: f32,
    pub normals_unpack_add: f32,
    pub pad: u32,
    /// Inputs, in `SHADER_READ_ONLY_OPTIMAL`.
    pub depth: Image,
    pub normal: Image,
    /// Output, in `GENERAL`.
    pub output: Image,
}

/// `EngineBrixelizer`: opaque.
#[repr(C)]
pub struct Context {
    _private: [u8; 0],
}

/// `EngineBrixelizerGi`: opaque.
#[repr(C)]
pub struct GiContext {
    _private: [u8; 0],
}

#[allow(missing_docs)]
unsafe extern "C" {
    pub fn engine_brixelizer_create(desc: *const CreateDesc, out: *mut *mut Context) -> i32;
    pub fn engine_brixelizer_destroy(context: *mut Context);
    pub fn engine_brixelizer_register_buffer(
        context: *mut Context,
        buffer: vk::Buffer,
        size: u32,
        out_index: *mut u32,
    ) -> i32;
    pub fn engine_brixelizer_unregister_buffer(context: *mut Context, index: u32) -> i32;
    pub fn engine_brixelizer_create_instances(
        context: *mut Context,
        instances: *const Instance,
        count: u32,
        out_ids: *mut u32,
    ) -> i32;
    pub fn engine_brixelizer_delete_instances(
        context: *mut Context,
        ids: *const u32,
        count: u32,
    ) -> i32;
    pub fn engine_brixelizer_update(context: *mut Context, desc: *const UpdateDesc) -> i32;
    pub fn engine_brixelizer_stats(context: *mut Context, out: *mut Stats);
    pub fn engine_brixelizer_gpu_memory(context: *mut Context) -> u64;
    pub fn engine_brixelizer_gi_create(
        context: *mut Context,
        desc: *const GiCreateDesc,
        out: *mut *mut GiContext,
    ) -> i32;
    pub fn engine_brixelizer_gi_destroy(gi: *mut GiContext);
    pub fn engine_brixelizer_gi_dispatch(gi: *mut GiContext, desc: *const GiDispatchDesc) -> i32;
    pub fn engine_brixelizer_gi_gpu_memory(gi: *mut GiContext) -> u64;
    pub fn engine_brixelizer_gi_debug(gi: *mut GiContext, desc: *const GiDebugDesc) -> i32;
    #[cfg(test)]
    fn engine_brixelizer_layout(out: *mut u32, count: u32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn layout_matches_the_interface() {
        let mut theirs = [0u32; 20];
        // SAFETY: writes at most `theirs.len()` values.
        unsafe { engine_brixelizer_layout(theirs.as_mut_ptr(), theirs.len() as u32) };
        let ours = [
            size_of::<CreateDesc>(),
            offset_of!(CreateDesc, center),
            offset_of!(CreateDesc, max_gi_contexts),
            size_of::<Instance>(),
            offset_of!(Instance, transform),
            offset_of!(Instance, dynamic),
            size_of::<UpdateDesc>(),
            offset_of!(UpdateDesc, debug_output),
            size_of::<Stats>(),
            offset_of!(Stats, scratch_needed),
            size_of::<Cube>(),
            size_of::<GiCreateDesc>(),
            size_of::<GiDispatchDesc>(),
            offset_of!(GiDispatchDesc, camera_position),
            offset_of!(GiDispatchDesc, roughness_threshold),
            offset_of!(GiDispatchDesc, depth),
            offset_of!(GiDispatchDesc, environment),
            offset_of!(GiDispatchDesc, specular),
            size_of::<GiDebugDesc>(),
            offset_of!(GiDebugDesc, depth),
        ]
        .map(|v| v as u32);
        assert_eq!(ours, theirs);
    }
}
