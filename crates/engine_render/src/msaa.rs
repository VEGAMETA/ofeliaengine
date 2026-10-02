//! The resolve of a multisampled scene (MSAA, ADR 0075; `msaa.slang`): one full-screen
//! pass from the samples to one value per pixel. Color by a luminance-weighted average
//! (the samples' tone-mapped average: bright samples do not take over edge pixels),
//! motion vectors from the nearest sample, and for SMAA S2x the plain average with half
//! the two samples' difference. The depth is resolved by the forward renderer itself
//! (`ForwardPasses::resolved_depth`).

use crate::forward::MOTION_FORMAT;
use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc};
use crate::post::HDR_FORMAT;
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, Device, GraphicsPipelineDesc, Pipeline, PipelineDesc, RhiError, ShaderStageDesc,
    spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/msaa.resolveVertex.spv");
/// By sample count (2, 4, 8): color; color and motion vectors.
const RESOLVES: [[(&[u8], &core::ffi::CStr); 2]; 3] = [
    [
        (include_bytes!("../shaders/msaa.resolve2.spv"), c"resolve2"),
        (
            include_bytes!("../shaders/msaa.resolveMotion2.spv"),
            c"resolveMotion2",
        ),
    ],
    [
        (include_bytes!("../shaders/msaa.resolve4.spv"), c"resolve4"),
        (
            include_bytes!("../shaders/msaa.resolveMotion4.spv"),
            c"resolveMotion4",
        ),
    ],
    [
        (include_bytes!("../shaders/msaa.resolve8.spv"), c"resolve8"),
        (
            include_bytes!("../shaders/msaa.resolveMotion8.spv"),
            c"resolveMotion8",
        ),
    ],
];
/// SMAA S2x (two samples): color; color and motion vectors.
const SPLITS: [(&[u8], &core::ffi::CStr); 2] = [
    (
        include_bytes!("../shaders/msaa.resolveSplit.spv"),
        c"resolveSplit",
    ),
    (
        include_bytes!("../shaders/msaa.resolveSplitMotion.spv"),
        c"resolveSplitMotion",
    ),
];

/// `Push` in `msaa.slang` (16 bytes).
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    color: u32,
    motion: u32,
    depth: u32,
    _pad: u32,
}

/// What [`MsaaResolve::add_pass`] resolves.
#[derive(Debug, Clone, Copy)]
pub struct MsaaInput {
    /// The multisampled HDR color.
    pub color: Texture,
    /// The multisampled depth (for the motion vectors' nearest sample).
    pub depth: Texture,
    /// The multisampled motion vectors, when the view has them.
    pub motion: Option<Texture>,
    /// SMAA S2x (two samples): also half the samples' difference, and a plain average.
    pub split: bool,
}

/// What [`MsaaResolve::add_pass`] produced: single-sampled images.
#[derive(Debug, Clone, Copy)]
pub struct MsaaResolved {
    /// HDR color.
    pub color: Texture,
    /// Motion vectors, when the input had them.
    pub motion: Option<Texture>,
    /// With `split`: half of sample 0 minus sample 1.
    pub difference: Option<Texture>,
}

fn full_screen<'a>(
    vertex: &'a [u32],
    fragment: &'a [u32],
    entry: &'a core::ffi::CStr,
    formats: &'a [vk::Format],
) -> GraphicsPipelineDesc<'a> {
    GraphicsPipelineDesc {
        color_formats: formats,
        cull_mode: vk::CullModeFlags::NONE,
        depth_test: false,
        depth_write: false,
        ..GraphicsPipelineDesc::new(
            ShaderStageDesc {
                spirv: vertex,
                entry: c"resolveVertex",
            },
            Some(ShaderStageDesc {
                spirv: fragment,
                entry,
            }),
        )
    }
}

fn slot(samples: vk::SampleCountFlags) -> Option<usize> {
    match samples {
        vk::SampleCountFlags::TYPE_2 => Some(0),
        vk::SampleCountFlags::TYPE_4 => Some(1),
        vk::SampleCountFlags::TYPE_8 => Some(2),
        _ => None,
    }
}

/// The resolve's pipelines, made per sample count as views ask for them.
#[derive(Debug, Default)]
pub struct MsaaResolve {
    /// By sample count: without and with motion vectors.
    resolves: [Option<[Pipeline; 2]>; 3],
    /// S2x's: without and with motion vectors.
    splits: Option<[Pipeline; 2]>,
}

impl MsaaResolve {
    /// No pipelines yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the pipelines for `samples` (and S2x's with `split`).
    pub fn ensure(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        samples: vk::SampleCountFlags,
        split: bool,
    ) -> Result<(), RhiError> {
        let Some(k) = slot(samples) else {
            return Ok(());
        };
        if self.resolves[k].is_some() && (!split || self.splits.is_some()) {
            return Ok(());
        }
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let color = [HDR_FORMAT];
        let with_motion = [HDR_FORMAT, MOTION_FORMAT];
        let split_formats = [HDR_FORMAT, HDR_FORMAT];
        let split_motion = [HDR_FORMAT, HDR_FORMAT, MOTION_FORMAT];
        let layout = bindless.pipeline_layout();
        if self.resolves[k].is_none() {
            let [(a, ea), (b, eb)] = RESOLVES[k];
            let (a, b) = (
                spirv_words(a).ok_or_else(invalid)?,
                spirv_words(b).ok_or_else(invalid)?,
            );
            let descs = [
                PipelineDesc::Graphics(full_screen(&vertex, &a, ea, &color)),
                PipelineDesc::Graphics(full_screen(&vertex, &b, eb, &with_motion)),
            ];
            let mut p = Pipeline::create_many(device, layout, &descs)?.into_iter();
            if let (Some(a), Some(b)) = (p.next(), p.next()) {
                self.resolves[k] = Some([a, b]);
            }
        }
        if split && self.splits.is_none() {
            let [(a, ea), (b, eb)] = SPLITS;
            let (a, b) = (
                spirv_words(a).ok_or_else(invalid)?,
                spirv_words(b).ok_or_else(invalid)?,
            );
            let descs = [
                PipelineDesc::Graphics(full_screen(&vertex, &a, ea, &split_formats)),
                PipelineDesc::Graphics(full_screen(&vertex, &b, eb, &split_motion)),
            ];
            let mut p = Pipeline::create_many(device, layout, &descs)?.into_iter();
            if let (Some(a), Some(b)) = (p.next(), p.next()) {
                self.splits = Some([a, b]);
            }
        }
        Ok(())
    }

    /// Declares the resolve of `input`. `None` when its pipelines were not made
    /// ([`Self::ensure`]) or the input is single-sampled.
    pub fn add_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        input: MsaaInput,
    ) -> Option<MsaaResolved> {
        let desc = fg.texture_desc(input.color);
        let k = slot(desc.samples)?;
        let motion = usize::from(input.motion.is_some());
        let pipeline = if input.split {
            &self.splits.as_ref()?[motion]
        } else {
            &self.resolves[k].as_ref()?[motion]
        };
        let single = |format| TextureDesc::new_2d(format, desc.width, desc.height);
        let color = fg.create_texture("msaa color", single(HDR_FORMAT));
        let difference = input
            .split
            .then(|| fg.create_texture("msaa difference", single(HDR_FORMAT)));
        let resolved_motion = input
            .motion
            .map(|_| fg.create_texture("msaa motion", single(MOTION_FORMAT)));
        let (color_ms, depth_ms, motion_ms) = (input.color, input.depth, input.motion);
        let mut pass = fg
            .add_pass("msaa resolve")
            .read(color_ms, Access::Sampled(ShaderStages::FRAGMENT))
            .color(color, LoadOp::DontCare, StoreOp::Store);
        if let Some(difference) = difference {
            pass = pass.color(difference, LoadOp::DontCare, StoreOp::Store);
        }
        if let (Some(motion), Some(resolved)) = (motion_ms, resolved_motion) {
            pass = pass
                .read(motion, Access::Sampled(ShaderStages::FRAGMENT))
                .read(depth_ms, Access::Sampled(ShaderStages::FRAGMENT))
                .color(resolved, LoadOp::DontCare, StoreOp::Store);
        }
        pass.execute(move |ctx| {
            let Some(color) = ctx.sampled_index(color_ms) else {
                return;
            };
            let (motion, depth) = match motion_ms {
                Some(m) => match (ctx.sampled_index(m), ctx.sampled_index(depth_ms)) {
                    (Some(m), Some(d)) => (m, d),
                    _ => return,
                },
                None => (0, 0),
            };
            let push = Push {
                color,
                motion,
                depth,
                _pad: 0,
            };
            let area = ctx.render_area();
            // SAFETY: recording inside the pass's rendering scope; the pass reads the
            // samples it declared.
            unsafe {
                pipeline.bind(ctx.cmd);
                bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
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
        Some(MsaaResolved {
            color,
            motion: resolved_motion,
            difference,
        })
    }
}
