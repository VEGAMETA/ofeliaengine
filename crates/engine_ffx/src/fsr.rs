//! The C interface of `csrc/fsr.cpp`, FSR 3.1 (ADR 0060). The structures mirror the C++
//! ones field by field; `layout_matches_the_interface` checks sizes and offsets against
//! the compiled code.

use crate::{Device, Image};
use engine_rhi::ash::vk;

/// `EngineFsrCreateDesc`.
#[repr(C)]
pub struct CreateDesc {
    /// The device the context is created on.
    pub device: Device,
    /// The largest render size the context upscales from.
    pub max_render_width: u32,
    /// The largest render size the context upscales from.
    pub max_render_height: u32,
    /// The output size.
    pub upscale_width: u32,
    /// The output size.
    pub upscale_height: u32,
    /// `FFX_FSR3UPSCALER_ENABLE_*` flags (below).
    pub flags: u32,
    /// Receives the SDK's messages (`FfxMsgType`, UTF-16 text), or `None`.
    pub message: Option<unsafe extern "C" fn(kind: u32, text: *const u16)>,
}

/// `EngineFsrDispatchDesc`.
#[repr(C)]
#[allow(missing_docs)]
pub struct DispatchDesc {
    pub command_buffer: vk::CommandBuffer,
    pub color: Image,
    pub depth: Image,
    pub motion_vectors: Image,
    pub reactive: Image,
    pub output: Image,
    pub jitter: [f32; 2],
    pub motion_vector_scale: [f32; 2],
    pub render_width: u32,
    pub render_height: u32,
    pub sharpen: u32,
    pub sharpness: f32,
    pub frame_time_ms: f32,
    pub pre_exposure: f32,
    pub reset: u32,
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov_y: f32,
    pub meters_per_unit: f32,
    /// Null image: FSR's automatic exposure.
    pub exposure: Image,
}

/// `EngineFsr`: opaque.
#[repr(C)]
pub struct Context {
    _private: [u8; 0],
}

/// `FFX_FSR3UPSCALER_ENABLE_HIGH_DYNAMIC_RANGE` (`ffx_fsr3upscaler.h`).
pub const ENABLE_HIGH_DYNAMIC_RANGE: u32 = 1 << 0;
/// `FFX_FSR3UPSCALER_ENABLE_AUTO_EXPOSURE`.
pub const ENABLE_AUTO_EXPOSURE: u32 = 1 << 5;
/// `FFX_FSR3UPSCALER_ENABLE_DYNAMIC_RESOLUTION`.
pub const ENABLE_DYNAMIC_RESOLUTION: u32 = 1 << 6;
/// `FFX_FSR3UPSCALER_ENABLE_DEBUG_CHECKING`.
pub const ENABLE_DEBUG_CHECKING: u32 = 1 << 8;

#[allow(missing_docs)]
unsafe extern "C" {
    pub fn engine_fsr_create(desc: *const CreateDesc, out: *mut *mut Context) -> i32;
    pub fn engine_fsr_dispatch(fsr: *mut Context, desc: *const DispatchDesc) -> i32;
    pub fn engine_fsr_destroy(fsr: *mut Context);
    pub fn engine_fsr_gpu_memory(fsr: *mut Context) -> u64;
    pub fn engine_fsr_jitter_phase_count(render_width: i32, display_width: i32) -> i32;
    pub fn engine_fsr_jitter_offset(x: *mut f32, y: *mut f32, index: i32, phases: i32) -> i32;
    pub fn engine_fsr_upscale_ratio(quality_mode: u32) -> f32;
    #[cfg(test)]
    fn engine_fsr_layout(out: *mut u32, count: u32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn layout_matches_the_interface() {
        let mut ours_c = [0u32; 10];
        // SAFETY: writes at most `ours_c.len()` values.
        unsafe { engine_fsr_layout(ours_c.as_mut_ptr(), ours_c.len() as u32) };
        let ours = [
            size_of::<CreateDesc>(),
            offset_of!(CreateDesc, max_render_width),
            offset_of!(CreateDesc, flags),
            offset_of!(CreateDesc, message),
            size_of::<Image>(),
            size_of::<DispatchDesc>(),
            offset_of!(DispatchDesc, output),
            offset_of!(DispatchDesc, jitter),
            offset_of!(DispatchDesc, meters_per_unit),
            offset_of!(DispatchDesc, exposure),
        ]
        .map(|v| v as u32);
        assert_eq!(ours, ours_c);
    }
}
