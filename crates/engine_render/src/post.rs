//! Post-processing passes. For now: tonemapping from an HDR target to the display format
//! (Khronos PBR Neutral with exposure), as one fullscreen-triangle pass in the graph.

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
    _pad: u32,
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
        let pipeline = &self.pipeline;
        fg.add_pass("tonemap")
            .read(hdr, Access::Sampled(ShaderStages::FRAGMENT))
            .color(output, LoadOp::DontCare, StoreOp::Store)
            .execute(move |ctx| {
                let Some(source) = ctx.sampled_index(hdr) else {
                    return;
                };
                let push = Push {
                    source,
                    sampler: Bindless::sampler(SamplerKind::LinearClamp),
                    exposure,
                    _pad: 0,
                };
                // SAFETY: `Push` is plain `repr(C)` data without padding.
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&raw const push).cast::<u8>(),
                        core::mem::size_of::<Push>(),
                    )
                };
                let area = ctx.render_area();
                // SAFETY: recording inside the pass's rendering scope.
                unsafe {
                    pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    bindless.push_constants(ctx.cmd, bytes);
                    ctx.device.cmd_set_viewport(
                        ctx.cmd,
                        0,
                        &[vk::Viewport {
                            x: 0.0,
                            y: 0.0,
                            width: area.extent.width as f32,
                            height: area.extent.height as f32,
                            min_depth: 0.0,
                            max_depth: 1.0,
                        }],
                    );
                    ctx.device.cmd_set_scissor(ctx.cmd, 0, &[area]);
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                }
            });
    }
}
