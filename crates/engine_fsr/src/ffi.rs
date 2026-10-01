//! The C interface of `csrc/shim.cpp`. The structures mirror the C++ ones field by field;
//! `layout_matches_the_shim` checks sizes and offsets against the compiled shim.

use engine_rhi::ash::vk;

/// `EngineFsrCreateDesc`.
#[repr(C)]
pub(crate) struct CreateDesc {
    pub(crate) instance: vk::Instance,
    pub(crate) get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    pub(crate) physical_device: vk::PhysicalDevice,
    pub(crate) device: vk::Device,
    pub(crate) get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
    pub(crate) max_render_width: u32,
    pub(crate) max_render_height: u32,
    pub(crate) upscale_width: u32,
    pub(crate) upscale_height: u32,
    pub(crate) flags: u32,
    pub(crate) fp16: u32,
    pub(crate) modules: *const *const u8,
    pub(crate) module_sizes: *const u32,
    pub(crate) module_count: u32,
    pub(crate) message: Option<unsafe extern "C" fn(kind: u32, text: *const u16)>,
}

/// `EngineFsrImage`: a null `image` means none.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Image {
    pub(crate) image: vk::Image,
    pub(crate) format: vk::Format,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// `EngineFsrDispatchDesc`.
#[repr(C)]
pub(crate) struct DispatchDesc {
    pub(crate) command_buffer: vk::CommandBuffer,
    pub(crate) color: Image,
    pub(crate) depth: Image,
    pub(crate) motion_vectors: Image,
    pub(crate) reactive: Image,
    pub(crate) output: Image,
    pub(crate) jitter: [f32; 2],
    pub(crate) motion_vector_scale: [f32; 2],
    pub(crate) render_width: u32,
    pub(crate) render_height: u32,
    pub(crate) sharpen: u32,
    pub(crate) sharpness: f32,
    pub(crate) frame_time_ms: f32,
    pub(crate) pre_exposure: f32,
    pub(crate) reset: u32,
    pub(crate) camera_near: f32,
    pub(crate) camera_far: f32,
    pub(crate) camera_fov_y: f32,
    pub(crate) meters_per_unit: f32,
    /// Null image: FSR's automatic exposure.
    pub(crate) exposure: Image,
}

/// `EngineFsr`: opaque.
#[repr(C)]
pub(crate) struct Context {
    _private: [u8; 0],
}

/// `FFX_FSR3UPSCALER_ENABLE_*` (`ffx_fsr3upscaler.h`).
pub(crate) const ENABLE_HIGH_DYNAMIC_RANGE: u32 = 1 << 0;
pub(crate) const ENABLE_AUTO_EXPOSURE: u32 = 1 << 5;
pub(crate) const ENABLE_DYNAMIC_RESOLUTION: u32 = 1 << 6;
pub(crate) const ENABLE_DEBUG_CHECKING: u32 = 1 << 8;

unsafe extern "C" {
    pub(crate) fn engine_fsr_create(desc: *const CreateDesc, out: *mut *mut Context) -> i32;
    pub(crate) fn engine_fsr_dispatch(fsr: *mut Context, desc: *const DispatchDesc) -> i32;
    pub(crate) fn engine_fsr_destroy(fsr: *mut Context);
    pub(crate) fn engine_fsr_gpu_memory(fsr: *mut Context) -> u64;
    pub(crate) fn engine_fsr_jitter_phase_count(render_width: i32, display_width: i32) -> i32;
    pub(crate) fn engine_fsr_jitter_offset(
        x: *mut f32,
        y: *mut f32,
        index: i32,
        phases: i32,
    ) -> i32;
    pub(crate) fn engine_fsr_upscale_ratio(quality_mode: u32) -> f32;
    #[cfg(test)]
    pub(crate) fn engine_fsr_layout(out: *mut u32, count: u32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn layout_matches_the_shim() {
        let mut shim = [0u32; 10];
        // SAFETY: writes at most `shim.len()` values.
        unsafe { engine_fsr_layout(shim.as_mut_ptr(), shim.len() as u32) };
        let ours = [
            size_of::<CreateDesc>(),
            offset_of!(CreateDesc, max_render_width),
            offset_of!(CreateDesc, modules),
            offset_of!(CreateDesc, message),
            size_of::<Image>(),
            size_of::<DispatchDesc>(),
            offset_of!(DispatchDesc, output),
            offset_of!(DispatchDesc, jitter),
            offset_of!(DispatchDesc, meters_per_unit),
            offset_of!(DispatchDesc, exposure),
        ]
        .map(|v| v as u32);
        assert_eq!(ours, shim);
    }
}
