//! Post-processing passes. For now: tonemapping from an HDR target to the display format
//! (Khronos PBR Neutral with exposure), as one fullscreen-triangle pass in the graph, also
//! used to composite viewports into sub-rectangles of the output.

use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture};
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, Device, GraphicsPipelineDesc, Pipeline, RhiError, SamplerKind, ShaderStageDesc,
    spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/tonemap.vertexMain.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/tonemap.fragmentMain.spv");

/// The HDR format scenes render into before tonemapping.
pub const HDR_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// `Push` in `tonemap.slang` (16 bytes).
#[repr(C)]
struct Push {
    source: u32,
    sampler: u32,
    exposure: f32,
    flags: u32,
}

/// How [`Tonemap::add_composite_pass`] writes a source into (part of) an output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Composite {
    /// Target rectangle in output pixels, or `None` for the whole output.
    pub rect: Option<vk::Rect2D>,
    /// Sampler: linear for smooth scaling, nearest for pixel-exact integer scaling.
    pub sampler: SamplerKind,
    /// Linear multiplier before tone mapping.
    pub exposure: f32,
    /// Apply the tone mapper (HDR sources); `false` copies (LDR sources).
    pub tonemap: bool,
    /// What happens to the output first. `Load` keeps earlier composites.
    pub load: LoadOp,
}

impl Default for Composite {
    fn default() -> Self {
        Self {
            rect: None,
            sampler: SamplerKind::LinearClamp,
            exposure: 1.0,
            tonemap: true,
            load: LoadOp::DontCare,
        }
    }
}

/// Tonemapping into one output format.
#[derive(Debug)]
pub struct Tonemap {
    pipeline: Pipeline,
}

impl Tonemap {
    /// Creates the pipeline for writing `output_format` (for example the swapchain's).
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        output_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT).ok_or_else(invalid)?;
        let formats = [output_format];
        let pipeline = Pipeline::graphics(
            device,
            bindless.pipeline_layout(),
            &GraphicsPipelineDesc {
                color_formats: &formats,
                cull_mode: vk::CullModeFlags::NONE,
                depth_test: false,
                depth_write: false,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"vertexMain",
                    },
                    Some(ShaderStageDesc {
                        spirv: &fragment,
                        entry: c"fragmentMain",
                    }),
                )
            },
            vk::PipelineCache::null(),
        )?;
        Ok(Self { pipeline })
    }

    /// Declares a pass that tonemaps `hdr` (scaled by `exposure`) into `output`. The frame
    /// must execute with `bindless` so that `hdr` gets an index.
    pub fn add_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        hdr: Texture,
        output: Texture,
        exposure: f32,
    ) {
        self.declare(
            fg,
            bindless,
            "tonemap",
            hdr,
            output,
            Composite {
                exposure,
                ..Composite::default()
            },
        );
    }

    /// Declares a pass that writes `source` into `composite.rect` of `output`, tonemapped
    /// or copied, scaled with the chosen sampler. Several composites into one output
    /// build split screens and picture-in-picture (the first clears, the others load).
    pub fn add_composite_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        source: Texture,
        output: Texture,
        composite: Composite,
    ) {
        self.declare(fg, bindless, "composite", source, output, composite);
    }

    fn declare<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        name: &'static str,
        source: Texture,
        output: Texture,
        composite: Composite,
    ) {
        let pipeline = &self.pipeline;
        fg.add_pass(name)
            .read(source, Access::Sampled(ShaderStages::FRAGMENT))
            .color(output, composite.load, StoreOp::Store)
            .execute(move |ctx| {
                let Some(index) = ctx.sampled_index(source) else {
                    return;
                };
                let push = Push {
                    source: index,
                    sampler: Bindless::sampler(composite.sampler),
                    exposure: composite.exposure,
                    flags: u32::from(!composite.tonemap),
                };
                // SAFETY: `Push` is plain `repr(C)` data without padding.
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&raw const push).cast::<u8>(),
                        core::mem::size_of::<Push>(),
                    )
                };
                let area = ctx.render_area();
                let rect = composite.rect.unwrap_or(area);
                // SAFETY: recording inside the pass's rendering scope.
                unsafe {
                    pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    bindless.push_constants(ctx.cmd, bytes);
                    ctx.device.cmd_set_viewport(
                        ctx.cmd,
                        0,
                        &[vk::Viewport {
                            x: rect.offset.x as f32,
                            y: rect.offset.y as f32,
                            width: rect.extent.width as f32,
                            height: rect.extent.height as f32,
                            min_depth: 0.0,
                            max_depth: 1.0,
                        }],
                    );
                    ctx.device.cmd_set_scissor(ctx.cmd, 0, &[rect]);
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                }
            });
    }
}
