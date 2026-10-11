//! FSR 3.1 frame generation's compute work (ADR 0091): the frame half-way between the
//! previous frame and this one, from this frame's color without UI, its depth and its
//! motion vectors. The FidelityFX SDK's frame interpolation and optical flow (v1.1.4, MIT),
//! built by `engine_ffx`; not the SDK's swapchain: the engine draws the UI on the generated
//! frame and presents it itself.
//!
//! ```text
//! let mut interpolation = FrameInterpolation::new(&device, frames_in_flight);
//! // every frame, after the tone mapper wrote `source` (no UI in it):
//! interpolation.add_pass(&mut fg, InterpolationInput { source, depth, motion, camera, .. },
//!     generated, frame.number)?;
//! // `generated` now holds the frame between the previous `source` and this one.
//! ```
//!
//! The SDK records its passes (the inputs' prepare pass, the optical flow, 11
//! interpolation passes) into one graph pass, `frame interpolation`, and keeps the previous
//! source itself. Its contexts are created at the first [`FrameInterpolation::add_pass`],
//! and again when the display size or the format changes or the render size grows.

use crate::{FsrCamera, FsrError, transition};
use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use engine_ffx::framegen as ffi;
use engine_render::forward::MotionOutput;
use engine_render::graph::{Access, FrameGraph, ShaderStages, Texture};
use engine_rhi::Device;
use engine_rhi::ash::vk;

/// Dispatches the SDK refused.
static DISPATCH_FAILURES: AtomicU32 = AtomicU32::new(0);

/// How the source's values are encoded (the SDK decodes them to compare colors).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transfer {
    /// sRGB-encoded values in 0..1 (an SDR swapchain's).
    Srgb,
    /// HDR10: PQ-encoded BT.2020, with the display's peak in nits.
    Pq {
        /// The brightest value the content reaches, in nits.
        max_nits: f32,
    },
    /// Linear scRGB (1.0 = 80 nits), with the content's range in nits.
    ScRgb {
        /// The darkest value, in nits.
        min_nits: f32,
        /// The brightest value, in nits.
        max_nits: f32,
    },
}

impl Transfer {
    fn ffx(self) -> (u32, f32, f32) {
        match self {
            Self::Srgb => (ffi::TRANSFER_SRGB, 0.0, 1.0),
            Self::Pq { max_nits } => (ffi::TRANSFER_PQ, 0.0, max_nits),
            Self::ScRgb { min_nits, max_nits } => (ffi::TRANSFER_SCRGB, min_nits, max_nits),
        }
    }

    fn is_hdr(self) -> bool {
        self != Self::Srgb
    }
}

/// The camera's position and axes in world space: the SDK checks for them (FSR 3.1.4 and
/// later warn without them).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CameraBasis {
    /// Position.
    pub position: [f32; 3],
    /// Up, normalized.
    pub up: [f32; 3],
    /// Right, normalized.
    pub right: [f32; 3],
    /// Forward (the view direction), normalized.
    pub forward: [f32; 3],
}

/// What [`FrameInterpolation::add_pass`] interpolates from: this frame.
#[derive(Debug, Clone, Copy)]
pub struct InterpolationInput {
    /// This frame's color without UI, at the display size.
    pub source: Texture,
    /// The format the SDK reads `source` as: for sRGB data a UNORM format, so that it reads
    /// the encoded values (the image must allow that view: `MUTABLE_FORMAT`).
    pub source_format: vk::Format,
    /// How `source` is encoded.
    pub transfer: Transfer,
    /// Depth at the render size (0 at the near plane, 1 at the far plane).
    pub depth: Texture,
    /// Motion vectors and jitter at the render size (`ForwardPasses::motion`); its `reset`
    /// starts the interpolation over (nothing is interpolated across it).
    pub motion: MotionOutput,
    /// The camera's planes and field of view.
    pub camera: FsrCamera,
    /// The camera's position and axes.
    pub basis: CameraBasis,
    /// Time since the last frame, in milliseconds.
    pub frame_time_ms: f32,
}

/// The images the generated frame is written to.
#[derive(Debug, Clone, Copy)]
pub struct InterpolationOutput {
    /// The generated frame, at the display size: a storage image with an alpha channel
    /// (the SDK keeps intermediate data in it), also sampled (the SDK reads it back).
    pub texture: Texture,
    /// The format the SDK writes it as (UNORM for sRGB data, as [`InterpolationInput::source_format`]).
    pub format: vk::Format,
}

struct Context {
    raw: NonNull<ffi::Context>,
    max_render: (u32, u32),
    display: (u32, u32),
    format: vk::Format,
    hdr: bool,
}

/// FSR 3.1's frame interpolation (see the module documentation). One per view: the SDK's
/// contexts keep the previous source and the optical flow's history.
pub struct FrameInterpolation {
    device: engine_ffx::Device,
    context: Option<Context>,
    /// Contexts replaced by a resize, destroyed once their frames are done.
    retired: Vec<(u64, NonNull<ffi::Context>)>,
    frames_in_flight: u64,
    /// The SDK's frame id, one more every frame.
    frame_id: u64,
    reset: bool,
    debug_flags: u32,
}

// SAFETY: the SDK's contexts are not tied to the thread that created them;
// `FrameInterpolation` is used from one thread at a time (`&mut self`).
unsafe impl Send for FrameInterpolation {}

impl fmt::Debug for FrameInterpolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameInterpolation")
            .field(
                "context",
                &self.context.as_ref().map(|c| (c.max_render, c.display)),
            )
            .field("frame_id", &self.frame_id)
            .finish_non_exhaustive()
    }
}

impl FrameInterpolation {
    /// Prepares the interpolation for `device`; the SDK's contexts are created at the first
    /// [`Self::add_pass`], at its sizes. `frames_in_flight` bounds how long a replaced
    /// context is kept.
    pub fn new(device: &Device, frames_in_flight: usize) -> Self {
        Self {
            device: engine_ffx::Device::new(device),
            context: None,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            frame_id: 0,
            reset: false,
            debug_flags: 0,
        }
    }

    /// Starts over at the next frame: it generates nothing between the last frame and it
    /// (a camera cut, frame generation just turned on).
    pub fn reset(&mut self) {
        self.reset = true;
    }

    /// The SDK's debug drawing (`engine_ffx::framegen::DISPATCH_DRAW_DEBUG_*`), 0 for none.
    pub fn set_debug_flags(&mut self, flags: u32) {
        self.debug_flags = flags;
    }

    /// Dispatches the SDK refused, since the process started.
    pub fn dispatch_failures() -> u32 {
        DISPATCH_FAILURES.load(Ordering::Relaxed)
    }

    /// Bytes of GPU memory the SDK's contexts hold (0 before the first [`Self::add_pass`]).
    pub fn gpu_memory(&self) -> u64 {
        self.context.as_ref().map_or(0, |c| {
            // SAFETY: a live context.
            unsafe { ffi::engine_framegen_gpu_memory(c.raw.as_ptr()) }
        })
    }

    /// Declares the interpolation of the frame between the previous source and
    /// `input.source` into `output`. After a reset (or the first frame) the SDK writes the
    /// source itself there. `frame_number` bounds how long a replaced context is kept.
    pub fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        input: InterpolationInput,
        output: InterpolationOutput,
        frame_number: u64,
    ) -> Result<(), FsrError> {
        engine_profiler::zone!("fsr::FrameInterpolation::add_pass");
        self.collect(frame_number);
        let source_desc = fg.texture_desc(input.source);
        let depth_desc = fg.texture_desc(input.depth);
        let velocity_desc = fg.texture_desc(input.motion.velocity);
        let output_desc = fg.texture_desc(output.texture);
        let display = (source_desc.width, source_desc.height);
        let render = (depth_desc.width, depth_desc.height);
        let hdr = input.transfer.is_hdr();
        let mut reset = core::mem::take(&mut self.reset) || input.motion.reset;
        if self.context.as_ref().is_none_or(|c| {
            c.display != display
                || c.max_render.0 < render.0
                || c.max_render.1 < render.1
                || c.format != input.source_format
                || c.hdr != hdr
        }) {
            let context = self.create_context(render, display, input.source_format, hdr)?;
            if let Some(old) = self.context.replace(context) {
                self.retired.push((frame_number, old.raw));
            }
            reset = true;
        }
        let Some(context) = self.context.as_ref() else {
            unreachable!("created above");
        };
        let raw = context.raw;
        self.frame_id += 1;
        let frame_id = self.frame_id;
        let flags = self.debug_flags;
        let (transfer, min_luminance, max_luminance) = input.transfer.ffx();
        let velocity = input.motion.velocity;
        fg.add_pass("frame interpolation")
            .read(input.source, Access::Sampled(ShaderStages::COMPUTE))
            .read(input.depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(velocity, Access::Sampled(ShaderStages::COMPUTE))
            .write(output.texture, Access::StorageWrite(ShaderStages::COMPUTE))
            .execute(move |ctx| {
                let image = |texture: Texture, format: vk::Format, width: u32, height: u32| {
                    engine_ffx::Image {
                        image: ctx.image(texture),
                        format,
                        width,
                        height,
                    }
                };
                let source = image(
                    input.source,
                    input.source_format,
                    source_desc.width,
                    source_desc.height,
                );
                let depth = image(
                    input.depth,
                    depth_desc.format,
                    depth_desc.width,
                    depth_desc.height,
                );
                let motion = image(
                    velocity,
                    velocity_desc.format,
                    velocity_desc.width,
                    velocity_desc.height,
                );
                let generated = image(
                    output.texture,
                    output.format,
                    output_desc.width,
                    output_desc.height,
                );
                let inputs = [
                    (source.image, vk::ImageAspectFlags::COLOR),
                    (depth.image, vk::ImageAspectFlags::DEPTH),
                    (motion.image, vk::ImageAspectFlags::COLOR),
                ];
                // The SDK expects SHADER_READ_ONLY_OPTIMAL for compute reads, as FSR's
                // upscaler does (`Fsr::add_pass`).
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
                    source,
                    depth,
                    motion_vectors: motion,
                    output: generated,
                    jitter: input.motion.jitter,
                    // As for the upscaler: the engine's vectors are the texture-coordinate
                    // delta current − previous, the SDK's point to the previous position.
                    motion_vector_scale: [-(render.0 as f32), -(render.1 as f32)],
                    render_width: render.0,
                    render_height: render.1,
                    frame_time_ms: input.frame_time_ms,
                    camera_near: input.camera.near,
                    camera_far: input.camera.far,
                    camera_fov_y: input.camera.fov_y,
                    meters_per_unit: input.camera.meters_per_unit,
                    camera_position: input.basis.position,
                    camera_up: input.basis.up,
                    camera_right: input.basis.right,
                    camera_forward: input.basis.forward,
                    frame_id,
                    reset: u32::from(reset),
                    transfer_function: transfer,
                    min_luminance,
                    max_luminance,
                    flags,
                };
                // SAFETY: a live context (retired ones are kept until their frames are
                // done); the images are alive for the frame and in the layouts the shim
                // expects (inputs just transitioned, the output GENERAL for storage).
                if unsafe { ffi::engine_framegen_dispatch(raw.as_ptr(), &desc) } != 0 {
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
        Ok(())
    }

    fn create_context(
        &self,
        render: (u32, u32),
        display: (u32, u32),
        format: vk::Format,
        hdr: bool,
    ) -> Result<Context, FsrError> {
        engine_profiler::zone!("fsr::FrameInterpolation::create_context");
        let mut flags = 0;
        if hdr {
            flags |= ffi::ENABLE_HDR_COLOR_INPUT;
        }
        engine_ffx::register_modules();
        let desc = ffi::CreateDesc {
            device: self.device,
            max_render_width: render.0,
            max_render_height: render.1,
            display_width: display.0,
            display_height: display.1,
            format,
            flags,
        };
        let mut raw = core::ptr::null_mut();
        // SAFETY: the handles are the engine's live instance and device; the modules were
        // handed over above.
        let code = unsafe { ffi::engine_framegen_create(&desc, &mut raw) };
        match NonNull::new(raw) {
            Some(raw) if code == 0 => Ok(Context {
                raw,
                max_render: render,
                display,
                format,
                hdr,
            }),
            _ => Err(FsrError::Ffx {
                call: "ffxFrameInterpolationContextCreate",
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
            unsafe { ffi::engine_framegen_destroy(raw.as_ptr()) };
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
            unsafe { ffi::engine_framegen_destroy(raw.as_ptr()) };
        }
    }
}
