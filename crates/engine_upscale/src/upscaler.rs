//! One interface over the engine's upscalers (CLAUDE.md §6, ADR 0063): the engine's own
//! TAAU ([`crate::Taau`]), FSR 3.1 (`engine_fsr`) and DLSS (`engine_dlss`) take the same
//! inputs and are chosen per view.
//!
//! ```text
//! let mut upscaler: Box<dyn Upscaler> = match choice {
//!     UpscalerKind::Taau => Box::new(Taau::new(&device, &bindless, frames_in_flight)?),
//!     UpscalerKind::Fsr => Box::new(Fsr::new(&device, frames_in_flight)),
//!     UpscalerKind::Dlss => Box::new(Dlss::new(&streamline, frames_in_flight)?),
//! };
//! let render = upscaler.render_size(Quality::Quality, output);
//! temporal.set_jitter_phases(upscaler.jitter_phases(render, output));
//! materials.set_texture_lod_bias(&bindless, upscaler.texture_lod_bias(render, output))?;
//! // every frame:
//! let upscaled = upscaler.add_pass(&mut fg, UpscaleContext { device: &device,
//!     allocator: &mut allocator, bindless: &bindless }, input, output, frame.number)?;
//! ```

use core::fmt;
use engine_render::forward::MotionOutput;
use engine_render::graph::{FrameGraph, Texture};
use engine_rhi::{Allocator, Bindless, Device, RhiError};

/// Which upscaler a view uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpscalerKind {
    /// The engine's own temporal antialiasing and upsampling: always available.
    Taau,
    /// AMD FidelityFX Super Resolution 3.1, built into the engine: any Vulkan 1.3 GPU.
    Fsr,
    /// NVIDIA DLSS Super Resolution through Streamline: NVIDIA RTX GPUs, with the DLLs
    /// the user supplies.
    Dlss,
}

impl UpscalerKind {
    /// A short name for menus and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Taau => "TAAU",
            Self::Fsr => "FSR 3.1",
            Self::Dlss => "DLSS",
        }
    }
}

/// How much smaller than the output a view renders. Each upscaler maps it to its own
/// ratio ([`Upscaler::render_size`]): FSR's and the engine's are 1, 1.5, 1.7, 2 and 3 per
/// axis; DLSS reports its own (1, 1.5, about 1.72, 2, 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Quality {
    /// Render at the output size: antialiasing only (FSR's Native AA, NVIDIA's DLAA).
    Native,
    /// About 1.5× smaller per axis.
    #[default]
    Quality,
    /// About 1.7× smaller per axis.
    Balanced,
    /// 2× smaller per axis.
    Performance,
    /// 3× smaller per axis.
    UltraPerformance,
}

impl Quality {
    /// All levels, from the largest render size to the smallest.
    pub const ALL: [Self; 5] = [
        Self::Native,
        Self::Quality,
        Self::Balanced,
        Self::Performance,
        Self::UltraPerformance,
    ];

    /// Output size over render size per axis, as FSR and the engine's TAAU use it.
    pub fn ratio(self) -> f32 {
        match self {
            Self::Native => 1.0,
            Self::Quality => 1.5,
            Self::Balanced => 1.7,
            Self::Performance => 2.0,
            Self::UltraPerformance => 3.0,
        }
    }

    /// The render size for `output` at [`Self::ratio`], rounded down (FSR's rule).
    pub fn render_size(self, output: (u32, u32)) -> (u32, u32) {
        let ratio = self.ratio();
        (
            ((output.0 as f32 / ratio) as u32).max(1),
            ((output.1 as f32 / ratio) as u32).max(1),
        )
    }
}

/// The camera a frame was drawn with, for upscalers that linearize depth (FSR).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UpscaleCamera {
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance (finite: the engine's projection has a far plane).
    pub far: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
}

/// What an upscaler upscales: the forward renderer's outputs for one view. Every upscaler
/// gets all of it and uses what it needs.
#[derive(Debug, Clone, Copy)]
pub struct UpscaleInput {
    /// HDR color, jittered, at the render size.
    pub color: Texture,
    /// Depth, the size of `color` (0 at the near plane, 1 at the far plane).
    pub depth: Texture,
    /// Motion vectors, jitter, reset and the reactive mask (`ForwardPasses::motion`).
    pub motion: MotionOutput,
    /// The camera the frame was drawn with.
    pub camera: UpscaleCamera,
    /// Time since the last frame, in milliseconds.
    pub frame_time_ms: f32,
}

/// The engine objects an upscaler may need while declaring its pass.
pub struct UpscaleContext<'a, 'g> {
    /// The device.
    pub device: &'a Device,
    /// Device memory (TAAU's history).
    pub allocator: &'a mut Allocator,
    /// The bindless set the frame executes with.
    pub bindless: &'g Bindless,
}

impl fmt::Debug for UpscaleContext<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpscaleContext").finish_non_exhaustive()
    }
}

/// Why an upscaler could not declare its pass.
#[derive(Debug)]
pub enum UpscaleError {
    /// A Vulkan or allocation error.
    Rhi(RhiError),
    /// The upscaler's library refused a call.
    Library {
        /// Which upscaler.
        upscaler: UpscalerKind,
        /// The call that failed.
        call: &'static str,
        /// The library's error code.
        code: i32,
    },
}

impl fmt::Display for UpscaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rhi(e) => write!(f, "{e}"),
            Self::Library {
                upscaler,
                call,
                code,
            } => write!(f, "{}: {call} failed with code {code}", upscaler.name()),
        }
    }
}

impl std::error::Error for UpscaleError {}

impl From<RhiError> for UpscaleError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

/// A temporal upscaler for one view: it keeps the view's history. See the module
/// documentation.
pub trait Upscaler {
    /// Which upscaler this is.
    fn kind(&self) -> UpscalerKind;

    /// The render size for `quality` at `output`.
    fn render_size(&self, quality: Quality, output: (u32, u32)) -> (u32, u32);

    /// Jitter phases for upscaling `render` to `output`; pass it to
    /// `Temporal::set_jitter_phases`.
    fn jitter_phases(&self, render: (u32, u32), output: (u32, u32)) -> u32;

    /// The texture LOD bias for upscaling `render` to `output`; pass it to
    /// `MaterialTable::set_texture_lod_bias`. By default [`crate::texture_lod_bias`],
    /// log2(render / output), which measured best for TAAU and FSR (ADR 0060).
    fn texture_lod_bias(&self, render: (u32, u32), output: (u32, u32)) -> f32 {
        crate::texture_lod_bias(render, output)
    }

    /// Forgets the history (camera cut); `MotionOutput::reset` does the same per frame.
    fn reset(&mut self);

    /// Declares the upscaling pass into an image of `output_size` and returns it: HDR,
    /// ready for tonemapping. `frame_number` bounds how long replaced resources are kept.
    fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        ctx: UpscaleContext<'_, 'g>,
        input: UpscaleInput,
        output_size: (u32, u32),
        frame_number: u64,
    ) -> Result<Texture, UpscaleError>;

    /// Frees the upscaler's GPU resources.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless);
}

impl Upscaler for crate::Taau {
    fn kind(&self) -> UpscalerKind {
        UpscalerKind::Taau
    }

    fn render_size(&self, quality: Quality, output: (u32, u32)) -> (u32, u32) {
        quality.render_size(output)
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
        ctx: UpscaleContext<'_, 'g>,
        input: UpscaleInput,
        output_size: (u32, u32),
        frame_number: u64,
    ) -> Result<Texture, UpscaleError> {
        Ok(Self::add_pass(
            self,
            fg,
            ctx.device,
            ctx.allocator,
            ctx.bindless,
            crate::TaauInput {
                color: input.color,
                depth: input.depth,
                motion: input.motion,
            },
            output_size,
            frame_number,
        )?)
    }

    unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::destroy(self, allocator, bindless) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_ratios_round_down() {
        assert_eq!(Quality::Native.render_size((1920, 1080)), (1920, 1080));
        assert_eq!(Quality::Quality.render_size((1920, 1080)), (1280, 720));
        assert_eq!(Quality::Balanced.render_size((1920, 1080)), (1129, 635));
        assert_eq!(Quality::Performance.render_size((1920, 1080)), (960, 540));
        assert_eq!(
            Quality::UltraPerformance.render_size((1920, 1080)),
            (640, 360)
        );
        assert_eq!(Quality::UltraPerformance.render_size((1, 1)), (1, 1));
    }

    #[test]
    fn upscaler_is_object_safe() {
        fn takes(_: Option<&mut dyn Upscaler>) {}
        takes(None);
        assert_eq!(UpscalerKind::Dlss.name(), "DLSS");
    }
}
