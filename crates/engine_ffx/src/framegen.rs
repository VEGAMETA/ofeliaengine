//! The C interface of `csrc/framegen.cpp`: FSR 3.1's frame interpolation and optical flow
//! (ADR 0091), without the SDK's swapchain. The structures mirror the C++ ones field by
//! field; `layout_matches_the_interface` checks sizes and offsets against the compiled code.

use crate::{Device, Image};
use engine_rhi::ash::vk;

/// `EngineFrameGenCreateDesc`.
#[repr(C)]
pub struct CreateDesc {
    /// The device the contexts are created on.
    pub device: Device,
    /// The largest render size (depth and motion vectors).
    pub max_render_width: u32,
    /// The largest render size (depth and motion vectors).
    pub max_render_height: u32,
    /// The display size (the interpolation sources and the generated frame).
    pub display_width: u32,
    /// The display size.
    pub display_height: u32,
    /// The format the sources are described with and the output's: a UNORM format for
    /// sRGB-encoded data (the SDK decodes it itself, [`DispatchDesc::transfer_function`]).
    pub format: vk::Format,
    /// `FFX_FRAMEINTERPOLATION_ENABLE_*` flags (below).
    pub flags: u32,
}

/// `EngineFrameGenDispatchDesc`.
#[repr(C)]
#[allow(missing_docs)]
pub struct DispatchDesc {
    pub command_buffer: vk::CommandBuffer,
    /// This frame's color without UI, at the display size.
    pub source: Image,
    /// Depth at the render size.
    pub depth: Image,
    /// Motion vectors at the render size.
    pub motion_vectors: Image,
    /// The generated frame, at the display size: a storage image with an alpha channel.
    pub output: Image,
    pub jitter: [f32; 2],
    pub motion_vector_scale: [f32; 2],
    pub render_width: u32,
    pub render_height: u32,
    pub frame_time_ms: f32,
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov_y: f32,
    pub meters_per_unit: f32,
    pub camera_position: [f32; 3],
    pub camera_up: [f32; 3],
    pub camera_right: [f32; 3],
    pub camera_forward: [f32; 3],
    /// Grows by exactly one per frame; any other step resets the interpolation.
    pub frame_id: u64,
    pub reset: u32,
    /// [`TRANSFER_SRGB`], [`TRANSFER_PQ`] or [`TRANSFER_SCRGB`].
    pub transfer_function: u32,
    pub min_luminance: f32,
    pub max_luminance: f32,
    /// `FFX_FRAMEINTERPOLATION_DISPATCH_*` (the SDK's debug views; below).
    pub flags: u32,
}

/// `EngineFrameGen`: opaque.
#[repr(C)]
pub struct Context {
    _private: [u8; 0],
}

/// `FFX_FRAMEINTERPOLATION_ENABLE_DEPTH_INVERTED` (`ffx_frameinterpolation.h`).
pub const ENABLE_DEPTH_INVERTED: u32 = 1 << 0;
/// `FFX_FRAMEINTERPOLATION_ENABLE_DEPTH_INFINITE`.
pub const ENABLE_DEPTH_INFINITE: u32 = 1 << 1;
/// `FFX_FRAMEINTERPOLATION_ENABLE_HDR_COLOR_INPUT`.
pub const ENABLE_HDR_COLOR_INPUT: u32 = 1 << 3;
/// `FFX_FRAMEINTERPOLATION_ENABLE_DEBUG_CHECKING`.
pub const ENABLE_DEBUG_CHECKING: u32 = 1 << 7;

/// `FFX_FRAMEINTERPOLATION_DISPATCH_DRAW_DEBUG_TEAR_LINES`: bars of changing color at the
/// generated frame's sides.
pub const DISPATCH_DRAW_DEBUG_TEAR_LINES: u32 = 1 << 0;
/// `FFX_FRAMEINTERPOLATION_DISPATCH_DRAW_DEBUG_RESET_INDICATORS`.
pub const DISPATCH_DRAW_DEBUG_RESET_INDICATORS: u32 = 1 << 1;
/// `FFX_FRAMEINTERPOLATION_DISPATCH_DRAW_DEBUG_VIEW`: the internal images instead of the
/// frame.
pub const DISPATCH_DRAW_DEBUG_VIEW: u32 = 1 << 2;

/// `FFX_BACKBUFFER_TRANSFER_FUNCTION_SRGB`: sRGB-encoded values.
pub const TRANSFER_SRGB: u32 = 0;
/// `FFX_BACKBUFFER_TRANSFER_FUNCTION_PQ`: HDR10.
pub const TRANSFER_PQ: u32 = 1;
/// `FFX_BACKBUFFER_TRANSFER_FUNCTION_SCRGB`: linear scRGB.
pub const TRANSFER_SCRGB: u32 = 2;

#[allow(missing_docs)]
unsafe extern "C" {
    pub fn engine_framegen_create(desc: *const CreateDesc, out: *mut *mut Context) -> i32;
    pub fn engine_framegen_dispatch(fg: *mut Context, desc: *const DispatchDesc) -> i32;
    pub fn engine_framegen_destroy(fg: *mut Context);
    pub fn engine_framegen_gpu_memory(fg: *mut Context) -> u64;
    #[cfg(test)]
    fn engine_framegen_layout(out: *mut u32, count: u32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn layout_matches_the_interface() {
        let mut ours_c = [0u32; 11];
        // SAFETY: writes at most `ours_c.len()` values.
        unsafe { engine_framegen_layout(ours_c.as_mut_ptr(), ours_c.len() as u32) };
        let ours = [
            size_of::<CreateDesc>(),
            offset_of!(CreateDesc, max_render_width),
            offset_of!(CreateDesc, format),
            offset_of!(CreateDesc, flags),
            size_of::<DispatchDesc>(),
            offset_of!(DispatchDesc, output),
            offset_of!(DispatchDesc, jitter),
            offset_of!(DispatchDesc, meters_per_unit),
            offset_of!(DispatchDesc, camera_forward),
            offset_of!(DispatchDesc, frame_id),
            offset_of!(DispatchDesc, flags),
        ]
        .map(|v| v as u32);
        assert_eq!(ours, ours_c);
    }
}
