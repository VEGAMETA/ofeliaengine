//! AMD FidelityFX Super Resolution 3.1 upscaling (CLAUDE.md §6, ADR 0060).
//!
//! [`Fsr`] runs the FSR 3.1 upscaler of the FidelityFX SDK (v1.1.4, MIT), built from the
//! vendored source by `engine_ffx` into the engine's binary: no DLL. It takes
//! the same inputs as the engine's own TAAU (`engine_upscale::Taau`, ADR 0043): the forward
//! renderer's jittered HDR color, its depth and its [`MotionOutput`], plus the camera's
//! planes and field of view and the frame time, and returns an HDR image at the output
//! size, ready for tonemapping.
//!
//! ```text
//! let mut fsr = Fsr::new(&device, frames_in_flight);
//! let render = QualityMode::Quality.render_size(output);
//! temporal.set_jitter_phases(Fsr::jitter_phases(render, output));
//! // every frame:
//! let passes = forward.add_passes(.., &ForwardView { temporal: Some(&temporal), .. })?;
//! let upscaled = fsr.add_pass(&mut fg, FsrInput { color: hdr, depth,
//!     motion: passes.motion.unwrap(), camera, frame_time_ms }, output, frame.number)?;
//! tonemap.add_pass(&mut fg, &bindless, upscaled, backbuffer, exposure);
//! ```
//!
//! The SDK records its own passes, barriers and descriptor updates into the frame's
//! command buffer inside one graph pass (`fsr`), and keeps its history and intermediate
//! images itself. Its context is created at the first [`Fsr::add_pass`] and again when the
//! output size changes or the render size grows; creating one compiles its 11 compute
//! pipelines.

use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use engine_ffx::fsr as ffi;
use engine_render::forward::MotionOutput;
use engine_render::graph::{Access, FrameGraph, ShaderStages, Texture, TextureDesc};
use engine_rhi::Device;
use engine_rhi::ash::vk;

/// Format of the upscaled image (HDR).
pub const OUTPUT_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// FSR's quality modes: how much smaller than the output the scene is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QualityMode {
    /// Render at the output size: antialiasing only (1.0×).
    NativeAa,
    /// 1.5× per axis (2.25× fewer pixels).
    #[default]
    Quality,
    /// 1.7× per axis.
    Balanced,
    /// 2.0× per axis.
    Performance,
    /// 3.0× per axis.
    UltraPerformance,
}

impl QualityMode {
    /// All modes, from the largest render size to the smallest.
    pub const ALL: [Self; 5] = [
        Self::NativeAa,
        Self::Quality,
        Self::Balanced,
        Self::Performance,
        Self::UltraPerformance,
    ];

    /// Output size over render size, per axis (`ffxFsr3UpscalerGetUpscaleRatioFromQualityMode`).
    pub fn ratio(self) -> f32 {
        match self {
            Self::NativeAa => 1.0,
            Self::Quality => 1.5,
            Self::Balanced => 1.7,
            Self::Performance => 2.0,
            Self::UltraPerformance => 3.0,
        }
    }

    /// The render size for an output size, rounded down as the SDK does
    /// (`ffxFsr3UpscalerGetRenderResolutionFromQualityMode`).
    pub fn render_size(self, output: (u32, u32)) -> (u32, u32) {
        let ratio = self.ratio();
        (
            ((output.0 as f32 / ratio) as u32).max(1),
            ((output.1 as f32 / ratio) as u32).max(1),
        )
    }

    /// The SDK's `FfxFsr3UpscalerQualityMode`.
    fn ffx(self) -> u32 {
        match self {
            Self::NativeAa => 0,
            Self::Quality => 1,
            Self::Balanced => 2,
            Self::Performance => 3,
            Self::UltraPerformance => 4,
        }
    }
}

/// The camera parameters FSR needs besides the images: it linearizes depth with them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FsrCamera {
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance (`f32::INFINITY` is not supported: the engine's projection has
    /// a finite far plane).
    pub far: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Meters per world unit (1 in the engine).
    pub meters_per_unit: f32,
}

/// What [`Fsr::add_pass`] upscales.
#[derive(Debug, Clone, Copy)]
pub struct FsrInput {
    /// HDR color, jittered, at the render size.
    pub color: Texture,
    /// Depth, the size of `color` (0 at the near plane, 1 at the far plane).
    pub depth: Texture,
    /// Motion vectors, jitter, reset and the reactive mask (`ForwardPasses::motion`).
    pub motion: MotionOutput,
    /// The camera the frame was drawn with.
    pub camera: FsrCamera,
    /// Time since the last frame, in milliseconds.
    pub frame_time_ms: f32,
    /// The exposure the tone mapper applies, a 1 × 1 `R32_SFLOAT` texture
    /// (`engine_upscale::UpscaleInput::exposure`), or `None` for FSR's own automatic
    /// exposure. Switching between the two creates a new context.
    pub exposure: Option<Texture>,
}

/// An error from the FidelityFX SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsrError {
    /// An SDK call returned an `FfxErrorCode`.
    Ffx {
        /// What was being done.
        call: &'static str,
        /// The error code (`ffx_error.h`).
        code: i32,
    },
}

impl fmt::Display for FsrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffx { call, code } => {
                write!(f, "{call} failed: FfxErrorCode {code:#x}")
            }
        }
    }
}

impl std::error::Error for FsrError {}

/// Warnings the SDK reported through its message callback (with debug checking).
static WARNINGS: AtomicU32 = AtomicU32::new(0);
/// Dispatches the SDK refused.
static DISPATCH_FAILURES: AtomicU32 = AtomicU32::new(0);

/// The SDK's message callback: counts warnings and prints them in debug builds.
unsafe extern "C" fn message(kind: u32, text: *const u16) {
    // FFX_MESSAGE_TYPE_ERROR = 0, FFX_MESSAGE_TYPE_WARNING = 1.
    WARNINGS.fetch_add(1, Ordering::Relaxed);
    if cfg!(debug_assertions) && !text.is_null() {
        let mut len = 0;
        // SAFETY: the SDK passes a NUL-terminated UTF-16 string.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` elements were just read.
        let text = String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(text, len) });
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "FSR {}: {text}",
                if kind == 0 { "error" } else { "warning" }
            );
        }
    }
}

struct Context {
    raw: NonNull<ffi::Context>,
    max_render: (u32, u32),
    output: (u32, u32),
    /// Created with FSR's automatic exposure (no exposure texture).
    auto_exposure: bool,
}

/// FSR 3.1 upscaling (see the crate documentation). One per view: the SDK's context keeps
/// the view's history.
pub struct Fsr {
    device: engine_ffx::Device,
    context: Option<Context>,
    /// Contexts replaced by a resize, destroyed once their frames are done.
    retired: Vec<(u64, NonNull<ffi::Context>)>,
    frames_in_flight: u64,
    sharpness: Option<f32>,
    reset: bool,
    debug_checking: bool,
}

// SAFETY: the SDK context is not tied to the thread that created it; `Fsr` is used from one
// thread at a time (`&mut self`).
unsafe impl Send for Fsr {}

impl fmt::Debug for Fsr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fsr")
            .field(
                "context",
                &self.context.as_ref().map(|c| (c.max_render, c.output)),
            )
            .field("sharpness", &self.sharpness)
            .finish_non_exhaustive()
    }
}

impl Fsr {
    /// Prepares FSR for `device`; the SDK's context is created at the first
    /// [`Self::add_pass`], at its sizes. `frames_in_flight` bounds how long a replaced
    /// context is kept.
    pub fn new(device: &Device, frames_in_flight: usize) -> Self {
        Self {
            device: engine_ffx::Device::new(device),
            context: None,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            sharpness: None,
            reset: false,
            debug_checking: false,
        }
    }

    /// Jitter phases for upscaling `render` to `output`: ⌊8 · s²⌋ for a scale `s` per axis,
    /// as `ffxFsr3UpscalerGetJitterPhaseCount` computes it (its documentation says ⌈⌉, its
    /// code truncates; the upscaler uses the same count internally). The sequence is the
    /// Halton(2, 3) one the forward renderer's `Temporal` produces. Pass it to
    /// `Temporal::set_jitter_phases`.
    pub fn jitter_phases(render: (u32, u32), output: (u32, u32)) -> u32 {
        let scale = output.0 as f32 / render.0.max(1) as f32;
        ((8.0 * scale * scale) as u32).max(1)
    }

    /// The texture LOD bias AMD recommends for FSR, log2(render width / output width) − 1
    /// (the SDK's documentation, "Mipmap biasing"): one level sharper than the output size.
    /// Pass it to `MaterialTable::set_texture_lod_bias`; `engine_upscale::texture_lod_bias` is the
    /// same without the extra level.
    pub fn texture_lod_bias(render: (u32, u32), output: (u32, u32)) -> f32 {
        (render.0.max(1) as f32 / output.0.max(1) as f32).log2() - 1.0
    }

    /// Sharpening after upscaling (RCAS), 0 (none) to 1 (most), or `None` for no
    /// sharpening pass (the default). Takes effect with the next [`Self::add_pass`].
    pub fn set_sharpness(&mut self, sharpness: Option<f32>) {
        self.sharpness = sharpness.map(|s| s.clamp(0.0, 1.0));
    }

    /// The current sharpness.
    pub fn sharpness(&self) -> Option<f32> {
        self.sharpness
    }

    /// Makes the SDK check its inputs and report problems through its message callback
    /// (counted by [`Self::warnings`]); takes effect when the next context is created.
    /// For tests and debugging.
    pub fn set_debug_checking(&mut self, enabled: bool) {
        self.debug_checking = enabled;
    }

    /// Warnings and errors the SDK reported so far, for every context of the process.
    pub fn warnings() -> u32 {
        WARNINGS.load(Ordering::Relaxed)
    }

    /// Dispatches the SDK refused so far (invalid inputs), for every context of the
    /// process. The frame then has no upscaled image.
    pub fn dispatch_failures() -> u32 {
        DISPATCH_FAILURES.load(Ordering::Relaxed)
    }

    /// Forgets the history: the next frame starts accumulating anew (after a camera cut,
    /// with `Temporal::reset`, which does the same through [`MotionOutput::reset`]).
    pub fn reset(&mut self) {
        self.reset = true;
    }

    /// Bytes of GPU memory the SDK's context holds (history, intermediate images, 0 before
    /// the first [`Self::add_pass`]).
    pub fn gpu_memory(&self) -> u64 {
        self.context.as_ref().map_or(0, |c| {
            // SAFETY: a live context.
            unsafe { ffi::engine_fsr_gpu_memory(c.raw.as_ptr()) }
        })
    }

    /// Declares the upscaling pass into an image of `output_size` and returns it: HDR,
    /// ready for tonemapping. `frame_number` bounds how long a replaced context is kept.
    pub fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        input: FsrInput,
        output_size: (u32, u32),
        frame_number: u64,
    ) -> Result<Texture, FsrError> {
        engine_profiler::zone!("fsr::Fsr::add_pass");
        self.collect(frame_number);
        let output_size = (output_size.0.max(1), output_size.1.max(1));
        let color_desc = fg.texture_desc(input.color);
        let depth_desc = fg.texture_desc(input.depth);
        let velocity_desc = fg.texture_desc(input.motion.velocity);
        let render = (color_desc.width, color_desc.height);
        let mut reset = core::mem::take(&mut self.reset) || input.motion.reset;
        let auto_exposure = input.exposure.is_none();
        if self.context.as_ref().is_none_or(|c| {
            c.output != output_size
                || c.max_render.0 < render.0
                || c.max_render.1 < render.1
                || c.auto_exposure != auto_exposure
        }) {
            let context = self.create_context(render, output_size, auto_exposure)?;
            if let Some(old) = self.context.replace(context) {
                self.retired.push((frame_number, old.raw));
            }
            reset = true;
        }
        let Some(context) = self.context.as_ref() else {
            unreachable!("created above");
        };
        let raw = context.raw;
        let output = fg.create_texture(
            "fsr output",
            TextureDesc::new_2d(OUTPUT_FORMAT, output_size.0, output_size.1),
        );
        let sharpness = self.sharpness;
        let reactive = input.motion.reactive;
        let reactive_desc = reactive.map(|r| fg.texture_desc(r));
        let exposure = input.exposure;
        let exposure_desc = exposure.map(|e| fg.texture_desc(e));
        let velocity = input.motion.velocity;
        let mut pass = fg.add_pass("fsr");
        if let Some(mask) = reactive {
            pass = pass.read(mask, Access::Sampled(ShaderStages::COMPUTE));
        }
        if let Some(exposure) = exposure {
            pass = pass.read(exposure, Access::Sampled(ShaderStages::COMPUTE));
        }
        pass.read(input.color, Access::Sampled(ShaderStages::COMPUTE))
            .read(input.depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(velocity, Access::Sampled(ShaderStages::COMPUTE))
            .write(output, Access::StorageWrite(ShaderStages::COMPUTE))
            .execute(move |ctx| {
                let image = |texture: Texture, desc: TextureDesc| engine_ffx::Image {
                    image: ctx.image(texture),
                    format: desc.format,
                    width: desc.width,
                    height: desc.height,
                };
                let color = image(input.color, color_desc);
                let depth = image(input.depth, depth_desc);
                let motion = image(velocity, velocity_desc);
                let reactive = match (reactive, reactive_desc) {
                    (Some(mask), Some(desc)) => image(mask, desc),
                    _ => engine_ffx::Image::default(),
                };
                let exposure = match (exposure, exposure_desc) {
                    (Some(texture), Some(desc)) => image(texture, desc),
                    _ => engine_ffx::Image::default(),
                };
                let inputs = [
                    (color.image, vk::ImageAspectFlags::COLOR),
                    (depth.image, vk::ImageAspectFlags::DEPTH),
                    (motion.image, vk::ImageAspectFlags::COLOR),
                    (reactive.image, vk::ImageAspectFlags::COLOR),
                    (exposure.image, vk::ImageAspectFlags::COLOR),
                ];
                // The graph leaves sampled inputs in READ_ONLY_OPTIMAL; the SDK expects
                // SHADER_READ_ONLY_OPTIMAL for compute reads and returns them in it. The
                // layout change is a write: it waits for every earlier access (the graph
                // adds no barrier between two reads in the same layout, such as a read-only
                // depth attachment and this pass).
                // SAFETY: recording outside a rendering scope; the images are the pass's
                // declared inputs, synchronized by the graph for compute sampling.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::ALL_COMMANDS,
                            vk::AccessFlags2::MEMORY_WRITE,
                        ),
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );
                }
                let desc = ffi::DispatchDesc {
                    command_buffer: ctx.cmd,
                    color,
                    depth,
                    motion_vectors: motion,
                    reactive,
                    output: image(
                        output,
                        TextureDesc::new_2d(OUTPUT_FORMAT, output_size.0, output_size.1),
                    ),
                    jitter: input.motion.jitter,
                    // The engine's vectors are the texture-coordinate delta current −
                    // previous; the SDK adds its vectors (times this scale, over the render
                    // size) to a position to find the previous one.
                    motion_vector_scale: [-(render.0 as f32), -(render.1 as f32)],
                    render_width: render.0,
                    render_height: render.1,
                    sharpen: u32::from(sharpness.is_some()),
                    sharpness: sharpness.unwrap_or(0.0),
                    frame_time_ms: input.frame_time_ms,
                    pre_exposure: 1.0,
                    reset: u32::from(reset),
                    camera_near: input.camera.near,
                    camera_far: input.camera.far,
                    camera_fov_y: input.camera.fov_y,
                    meters_per_unit: input.camera.meters_per_unit,
                    exposure,
                };
                // SAFETY: a live context (retired ones are kept until their frames are
                // done); the images are alive for the frame and in the layouts the shim
                // expects (inputs just transitioned, the output GENERAL for storage).
                if unsafe { ffi::engine_fsr_dispatch(raw.as_ptr(), &desc) } != 0 {
                    DISPATCH_FAILURES.fetch_add(1, Ordering::Relaxed);
                }
                // Back to the graph's layout after the SDK's compute reads.
                // SAFETY: as above.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::NONE,
                        ),
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                    );
                }
            });
        Ok(output)
    }

    fn create_context(
        &self,
        render: (u32, u32),
        output: (u32, u32),
        auto_exposure: bool,
    ) -> Result<Context, FsrError> {
        engine_profiler::zone!("fsr::create_context");
        let mut flags = ffi::ENABLE_HIGH_DYNAMIC_RANGE | ffi::ENABLE_DYNAMIC_RESOLUTION;
        if auto_exposure {
            flags |= ffi::ENABLE_AUTO_EXPOSURE;
        }
        if self.debug_checking {
            flags |= ffi::ENABLE_DEBUG_CHECKING;
        }
        engine_ffx::register_modules();
        let desc = ffi::CreateDesc {
            device: self.device,
            max_render_width: render.0,
            max_render_height: render.1,
            upscale_width: output.0,
            upscale_height: output.1,
            flags,
            message: Some(message),
        };
        let mut raw = core::ptr::null_mut();
        // SAFETY: the handles are the engine's live instance and device; the modules were
        // handed over above.
        let code = unsafe { ffi::engine_fsr_create(&desc, &mut raw) };
        match NonNull::new(raw) {
            Some(raw) if code == 0 => Ok(Context {
                raw,
                max_render: render,
                output,
                auto_exposure,
            }),
            _ => Err(FsrError::Ffx {
                call: "ffxFsr3UpscalerContextCreate",
                code,
            }),
        }
    }

    fn collect(&mut self, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, raw) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it have finished.
            unsafe { ffi::engine_fsr_destroy(raw.as_ptr()) };
        }
    }

    /// Destroys the SDK's contexts.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self) {
        for (_, raw) in self
            .retired
            .drain(..)
            .chain(self.context.take().map(|c| (0, c.raw)))
        {
            // SAFETY: guaranteed by the caller.
            unsafe { ffi::engine_fsr_destroy(raw.as_ptr()) };
        }
    }
}

/// Layout transitions of the pass's sampled inputs (null images are skipped), after the
/// accesses in `after` and before compute shader reads.
///
/// # Safety
///
/// Recording outside a rendering scope; the images are in `from`.
unsafe fn transition(
    ctx: &engine_render::graph::PassContext<'_>,
    images: &[(vk::Image, vk::ImageAspectFlags); 5],
    after: (vk::PipelineStageFlags2, vk::AccessFlags2),
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let mut barriers = [vk::ImageMemoryBarrier2::default(); 5];
    let mut count = 0;
    for &(image, aspect) in images {
        if image == vk::Image::null() {
            continue;
        }
        barriers[count] = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(after.0)
            .src_access_mask(after.1)
            .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(from)
            .new_layout(to)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        count += 1;
    }
    // SAFETY: guaranteed by the caller.
    unsafe {
        ctx.device.cmd_pipeline_barrier2(
            ctx.cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&barriers[..count]),
        );
    }
}

impl From<engine_upscale::Quality> for QualityMode {
    fn from(quality: engine_upscale::Quality) -> Self {
        use engine_upscale::Quality;
        match quality {
            Quality::Native => Self::NativeAa,
            Quality::Quality => Self::Quality,
            Quality::Balanced => Self::Balanced,
            Quality::Performance => Self::Performance,
            Quality::UltraPerformance => Self::UltraPerformance,
        }
    }
}

impl From<FsrError> for engine_upscale::UpscaleError {
    fn from(e: FsrError) -> Self {
        match e {
            FsrError::Ffx { call, code } => Self::Library {
                upscaler: engine_upscale::UpscalerKind::Fsr,
                call,
                code,
            },
        }
    }
}

/// FSR behind the engine's upscaler interface (ADR 0063). The texture LOD bias is the
/// standard log2(render / output), which measured better than AMD's extra level (ADR 0060).
impl engine_upscale::Upscaler for Fsr {
    fn kind(&self) -> engine_upscale::UpscalerKind {
        engine_upscale::UpscalerKind::Fsr
    }

    fn render_size(&self, quality: engine_upscale::Quality, output: (u32, u32)) -> (u32, u32) {
        QualityMode::from(quality).render_size(output)
    }

    fn jitter_phases(&self, render: (u32, u32), output: (u32, u32)) -> u32 {
        Self::jitter_phases(render, output)
    }

    fn reset(&mut self) {
        Self::reset(self);
    }

    fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        _ctx: engine_upscale::UpscaleContext<'_, 'g>,
        input: engine_upscale::UpscaleInput,
        output_size: (u32, u32),
        frame_number: u64,
    ) -> Result<Texture, engine_upscale::UpscaleError> {
        Ok(Self::add_pass(
            self,
            fg,
            FsrInput {
                color: input.color,
                depth: input.depth,
                motion: input.motion,
                camera: FsrCamera {
                    near: input.camera.near,
                    far: input.camera.far,
                    fov_y: input.camera.fov_y,
                    meters_per_unit: 1.0,
                },
                frame_time_ms: input.frame_time_ms,
                exposure: input.exposure,
            },
            output_size,
            frame_number,
        )?)
    }

    unsafe fn destroy(
        &mut self,
        _allocator: &mut engine_rhi::Allocator,
        _bindless: &engine_rhi::Bindless,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::destroy(self) };
    }
}

/// The SDK's own numbers, to check this crate's against (tests).
#[doc(hidden)]
pub mod sdk {
    /// `ffxFsr3UpscalerGetJitterPhaseCount`.
    pub fn jitter_phase_count(render_width: u32, output_width: u32) -> u32 {
        // SAFETY: a pure function.
        unsafe {
            super::ffi::engine_fsr_jitter_phase_count(render_width as i32, output_width as i32)
        }
        .max(0) as u32
    }

    /// `ffxFsr3UpscalerGetJitterOffset` for the `index`-th frame, in pixels.
    pub fn jitter_offset(index: u32, phases: u32) -> [f32; 2] {
        let (mut x, mut y) = (0.0, 0.0);
        // SAFETY: writes the two floats.
        unsafe {
            super::ffi::engine_fsr_jitter_offset(&mut x, &mut y, index as i32, phases as i32)
        };
        [x, y]
    }

    /// `ffxFsr3UpscalerGetUpscaleRatioFromQualityMode`.
    pub fn upscale_ratio(mode: super::QualityMode) -> f32 {
        // SAFETY: a pure function.
        unsafe { super::ffi::engine_fsr_upscale_ratio(mode.ffx()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_rhi::{AdapterPreference, Instance, InstanceDesc};

    /// Contexts are created on several devices at once, as tests do (each renders on its
    /// own thread with its own instance and device): every context must get its own
    /// device's functions and `VkDeviceContext`, and the module tables must outlive every
    /// `Fsr`. The validation layer catches a context built on another thread's device.
    #[test]
    fn contexts_are_created_on_several_devices_at_once() {
        let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
        let threads: Vec<_> = (0..4)
            .map(|thread| {
                std::thread::spawn(move || {
                    let gpu = || -> Result<(Instance, Device), engine_rhi::RhiError> {
                        let instance = Instance::new(&InstanceDesc {
                            app_name: String::from("engine_fsr contexts"),
                            validation: true,
                            debug_utils: true,
                            surface: false,
                            headless_surface: false,
                            sync_validation: false,
                        })?;
                        let preference = AdapterPreference::from_env()
                            .unwrap_or(AdapterPreference::HighPerformance);
                        let adapter = instance.select_adapter(&preference)?;
                        let device = Device::new(&instance, &adapter)?;
                        Ok((instance, device))
                    };
                    let (instance, device) = match gpu() {
                        Ok(pair) => pair,
                        Err(e) if !required => return Some(e.to_string()),
                        Err(e) => panic!("GPU required but unavailable: {e}"),
                    };
                    for round in 0..6u32 {
                        // A new `Fsr` every round: another thread may be creating a
                        // context while this one's `Fsr` is dropped.
                        let fsr = Fsr::new(&device, 1);
                        let render = (64 + 8 * thread + round, 64);
                        let context = fsr
                            .create_context(render, (128, 128), round % 2 == 0)
                            .unwrap();
                        // SAFETY: the context was never used on the GPU.
                        unsafe { ffi::engine_fsr_destroy(context.raw.as_ptr()) };
                    }
                    drop(device);
                    let report = instance.validation_report();
                    assert_eq!(
                        report.errors, 0,
                        "validation errors: {:#?}",
                        report.first_errors
                    );
                    None
                })
            })
            .collect();
        for thread in threads {
            if let Some(skipped) = thread.join().unwrap() {
                println!("skipping GPU test: {skipped}");
            }
        }
    }
}
