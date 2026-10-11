//! The forward renderer's pipelines for multisampled targets (MSAA, ADR 0075), one set
//! per sample count, made when a view first renders with it ([`ForwardRenderer::set_samples`])
//! and extended as features are turned on. Single-sampled views keep the renderer's own
//! pipelines: nothing here touches them.
//!
//! Multisampled views always draw the depth prepass, so the opaque shading only needs its
//! after-the-prepass variants (depth tested for equality, per sample): each sample keeps
//! the coverage the prepass gave it. Masked materials turn their alpha into coverage
//! there (`prepassCoverage`, alpha to coverage) instead of the alpha test. The prepass
//! writes the motion vectors too, when the view has them (`prepassCoverageMotion` for
//! masked materials), so no shading here writes them.
//!
//! [`ForwardRenderer::set_samples`]: super::ForwardRenderer::set_samples

use super::{
    BACKGROUND_MOTION, BACKGROUND_VERTEX, DEPTH_VERTEX, DEPTH_VERTEX_MASKED,
    DEPTH_VERTEX_MASKED_MOTION, DEPTH_VERTEX_MOTION, FRAGMENT, FRAGMENT_AO, FRAGMENT_AO_CONTACT,
    FRAGMENT_AO_CONTACT_SHADOWED, FRAGMENT_AO_SHADOWED, FRAGMENT_BLENDED,
    FRAGMENT_BLENDED_ATMOSPHERE, FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE,
    FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE_SHADOWED, FRAGMENT_BLENDED_ATMOSPHERE_SHADOWED,
    FRAGMENT_BLENDED_REACTIVE, FRAGMENT_BLENDED_REACTIVE_SHADOWED, FRAGMENT_BLENDED_SHADOWED,
    FRAGMENT_CONTACT, FRAGMENT_CONTACT_SHADOWED, FRAGMENT_SHADOWED, FRAGMENT_SUBSURFACE,
    FRAGMENT_SUBSURFACE_SHADOWED, MOTION_FORMAT, PREPASS_MOTION, REACTIVE_FORMAT,
    REFLECT_FRAGMENTS, ReflectShadingTable, SKY_FRAGMENT, VARIANTS, VERTEX, shading_desc,
};
use crate::atmosphere::{COMPOSITE_FRAGMENT_MS, COMPOSITE_VERTEX};
use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture};
use crate::reflections::{NORMAL_ROUGHNESS_FORMAT, WEIGHT_FORMAT, composite_pipelines};
use crate::subsurface::{ALBEDO_FORMAT, IRRADIANCE_FORMAT};
use core::ffi::CStr;
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, Blend, Device, GraphicsPipelineDesc, LazyPipeline, Pipeline, PipelineDesc, RhiError,
    ShaderStageDesc, spirv_words,
};

const PREPASS_COVERAGE: &[u8] = include_bytes!("../../shaders/forward.prepassCoverage.spv");
const PREPASS_COVERAGE_MOTION: &[u8] =
    include_bytes!("../../shaders/forward.prepassCoverageMotion.spv");
const SCATTER_FRAGMENT_MS: &[u8] = include_bytes!("../../shaders/forward.scatterFragmentMs.spv");
const RESOLVE_VERTEX: &[u8] = include_bytes!("../../shaders/msaa.resolveVertex.spv");
/// The nearest sample's reflection inputs, by sample count (2, 4, 8).
const RESOLVE_REFLECT: [(&[u8], &CStr); 3] = [
    (
        include_bytes!("../../shaders/msaa.resolveReflect2.spv"),
        c"resolveReflect2",
    ),
    (
        include_bytes!("../../shaders/msaa.resolveReflect4.spv"),
        c"resolveReflect4",
    ),
    (
        include_bytes!("../../shaders/msaa.resolveReflect8.spv"),
        c"resolveReflect8",
    ),
];
/// The nearest subsurface sample's light and albedo, by sample count.
const RESOLVE_SUBSURFACE: [(&[u8], &CStr); 3] = [
    (
        include_bytes!("../../shaders/msaa.resolveSubsurface2.spv"),
        c"resolveSubsurface2",
    ),
    (
        include_bytes!("../../shaders/msaa.resolveSubsurface4.spv"),
        c"resolveSubsurface4",
    ),
    (
        include_bytes!("../../shaders/msaa.resolveSubsurface8.spv"),
        c"resolveSubsurface8",
    ),
];

/// `Push` in `msaa.slang`.
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ResolvePush {
    color: u32,
    motion: u32,
    depth: u32,
    _pad: u32,
}

/// A single-sampled full-screen pipeline writing `formats` from one of `entries` (by the
/// sample count).
fn nearest_resolve(
    device: &Device,
    bindless: &Bindless,
    samples: vk::SampleCountFlags,
    entries: [(&[u8], &CStr); 3],
    formats: &[vk::Format],
) -> Result<Pipeline, RhiError> {
    let k = match samples {
        vk::SampleCountFlags::TYPE_2 => 0,
        vk::SampleCountFlags::TYPE_4 => 1,
        _ => 2,
    };
    let vertex = words(RESOLVE_VERTEX)?;
    let fragment = words(entries[k].0)?;
    let desc = PipelineDesc::Graphics(GraphicsPipelineDesc {
        color_formats: formats,
        cull_mode: vk::CullModeFlags::NONE,
        depth_test: false,
        depth_write: false,
        ..GraphicsPipelineDesc::new(
            ShaderStageDesc {
                spirv: &vertex,
                entry: c"resolveVertex",
            },
            Some(ShaderStageDesc {
                spirv: &fragment,
                entry: entries[k].1,
            }),
        )
    });
    Pipeline::create_many(device, bindless.pipeline_layout(), &[desc])?
        .pop()
        .ok_or_else(short)
}

/// The farthest sample's depth, written by the reflection inputs' resolve.
pub(super) const FAR_DEPTH_FORMAT: vk::Format = vk::Format::R32_SFLOAT;

/// Declares a resolve of two multisampled images into `outputs` that keeps one sample per
/// pixel (the nearest by `depth`, or the subsurface one's own depth in alpha).
pub(super) fn add_nearest_resolve<'g>(
    fg: &mut FrameGraph<'g>,
    bindless: &'g Bindless,
    name: &'static str,
    pipeline: &'g Pipeline,
    inputs: [Texture; 2],
    depth: Option<Texture>,
    outputs: &[Texture],
) {
    let mut pass = fg
        .add_pass(name)
        .read(inputs[0], Access::Sampled(ShaderStages::FRAGMENT))
        .read(inputs[1], Access::Sampled(ShaderStages::FRAGMENT));
    for &output in outputs {
        pass = pass.color(output, LoadOp::DontCare, StoreOp::Store);
    }
    if let Some(depth) = depth {
        pass = pass.read(depth, Access::Sampled(ShaderStages::FRAGMENT));
    }
    pass.execute(move |ctx| {
        let (Some(color), Some(motion)) =
            (ctx.sampled_index(inputs[0]), ctx.sampled_index(inputs[1]))
        else {
            return;
        };
        let depth = depth.and_then(|d| ctx.sampled_index(d)).unwrap_or(0);
        let push = ResolvePush {
            color,
            motion,
            depth,
            _pad: 0,
        };
        let area = ctx.render_area();
        // SAFETY: recording inside the pass's rendering scope; the pass reads what it
        // declared.
        unsafe {
            pipeline.bind(ctx.cmd);
            bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
            bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
            super::set_viewport(ctx.device, ctx.cmd, area);
            ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
        }
    });
}

/// The opaque and transparent shading of one variant.
#[derive(Debug)]
pub(super) struct MsaaShading {
    /// After the prepass.
    pub(super) shade: LazyPipeline,
    /// Transparent surfaces: without and with the reactive mask.
    pub(super) blended: [LazyPipeline; 2],
}

/// With local lights' shadows (ADR 0071), per environment bit.
#[derive(Debug)]
pub(super) struct MsaaShadowed {
    pub(super) shading: [MsaaShading; 2],
    /// Transparent surfaces under an atmosphere: without and with the reactive mask.
    pub(super) blended_atmosphere: [LazyPipeline; 2],
    pub(super) ao: Option<[LazyPipeline; 2]>,
    pub(super) subsurface: Option<[LazyPipeline; 2]>,
}

/// The sun's contact shadows' shading (ADR 0071), as `ContactState`'s.
#[derive(Debug)]
pub(super) struct MsaaContact {
    pub(super) shading: [LazyPipeline; 4],
    pub(super) ao: Option<[LazyPipeline; 4]>,
    pub(super) shadowed: Option<[LazyPipeline; 2]>,
    pub(super) shadowed_ao: Option<[LazyPipeline; 2]>,
}

/// Which of the renderer's features have pipelines (their multisampled ones follow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Features {
    pub(super) ao: bool,
    pub(super) subsurface: bool,
    pub(super) shadowed: bool,
    pub(super) contact: bool,
    pub(super) reflect: bool,
}

/// One sample count's pipelines.
#[derive(Debug)]
pub(super) struct MsaaPipelines {
    pub(super) samples: vk::SampleCountFlags,
    /// Depth prepass: opaque (depth only), masked (alpha to coverage).
    pub(super) prepass: [Pipeline; 2],
    /// The same, also writing motion vectors (views with a temporal state).
    pub(super) prepass_motion: [Pipeline; 2],
    /// By shading variant (`VARIANTS`).
    pub(super) shading: [MsaaShading; 4],
    /// Camera motion where no geometry was drawn.
    pub(super) background: Pipeline,
    /// The environment's sky where no geometry was drawn.
    pub(super) sky: Pipeline,
    /// The atmosphere's full-screen pass, per sample.
    pub(super) composite: LazyPipeline,
    /// Transparent surfaces under an atmosphere, by local lights: without and with the
    /// reactive mask.
    pub(super) blended_atmosphere: [[LazyPipeline; 2]; 2],
    pub(super) ao: Option<[LazyPipeline; 4]>,
    /// Subsurface materials' shading per variant, the scatter pass, and the resolve of
    /// their light and albedo (the nearest sample's).
    pub(super) subsurface: Option<([LazyPipeline; 4], LazyPipeline, Pipeline)>,
    pub(super) shadowed: Option<MsaaShadowed>,
    pub(super) contact: Option<MsaaContact>,
    /// Reflections' shading, composite, and the resolve of their inputs (the nearest
    /// sample's).
    pub(super) reflect: Option<Box<(ReflectShadingTable, [Pipeline; 4], Pipeline)>>,
}

fn invalid() -> RhiError {
    RhiError::Vulkan {
        call: "embedded SPIR-V",
        result: vk::Result::ERROR_INVALID_SHADER_NV,
    }
}

fn words(bytes: &[u8]) -> Result<Vec<u32>, RhiError> {
    spirv_words(bytes).ok_or_else(invalid)
}

fn short() -> RhiError {
    RhiError::Vulkan {
        call: "multisampled forward pipelines (fewer than described)",
        result: vk::Result::ERROR_UNKNOWN,
    }
}

/// Pipeline descriptions with their SPIR-V kept alive, created together.
struct Batch<'a> {
    vertex: &'a [u32],
    depth: vk::Format,
    samples: vk::SampleCountFlags,
}

impl<'a> Batch<'a> {
    /// Opaque shading after the prepass: depth tested for equality, not written.
    fn equal(
        &self,
        fragment: &'a [u32],
        entry: &'a CStr,
        formats: &'a [vk::Format],
        variant: usize,
    ) -> PipelineDesc<'a> {
        PipelineDesc::Graphics(GraphicsPipelineDesc {
            depth_write: false,
            depth_compare: vk::CompareOp::EQUAL,
            samples: self.samples,
            ..shading_desc(
                self.vertex,
                fragment,
                entry,
                formats,
                self.depth,
                &VARIANTS[variant],
            )
        })
    }

    /// Transparent surfaces: premultiplied, depth tested, not written.
    fn blended(
        &self,
        fragment: &'a [u32],
        entry: &'a CStr,
        formats: &'a [vk::Format],
        variant: usize,
    ) -> PipelineDesc<'a> {
        PipelineDesc::Graphics(GraphicsPipelineDesc {
            depth_write: false,
            depth_compare: vk::CompareOp::LESS,
            blend: Blend::Premultiplied,
            samples: self.samples,
            ..shading_desc(
                self.vertex,
                fragment,
                entry,
                formats,
                self.depth,
                &VARIANTS[variant],
            )
        })
    }
}

/// After the prepass, for each of `variants`.
fn shades<'a>(
    batch: &Batch<'a>,
    descs: &mut Vec<PipelineDesc<'a>>,
    fragment: (&'a [u32], &'a CStr),
    formats: &'a [vk::Format],
    variants: &[usize],
) {
    for &v in variants {
        descs.push(batch.equal(fragment.0, fragment.1, formats, v));
    }
}

fn take<T, const N: usize>(pipelines: &mut impl Iterator<Item = T>) -> Result<[T; N], RhiError> {
    let mut out = Vec::with_capacity(N);
    for _ in 0..N {
        out.push(pipelines.next().ok_or_else(short)?);
    }
    out.try_into().map_err(|_| short())
}

impl MsaaPipelines {
    /// The base set for `samples`: prepass, shading, sky, motion background, the
    /// atmosphere's pass and transparent surfaces.
    pub(super) fn new(
        device: &Device,
        bindless: &Bindless,
        color: vk::Format,
        depth: vk::Format,
        samples: vk::SampleCountFlags,
    ) -> Result<Self, RhiError> {
        let vertex = words(VERTEX)?;
        let fragment = words(FRAGMENT)?;
        let fragment_blended = words(FRAGMENT_BLENDED)?;
        let fragment_blended_reactive = words(FRAGMENT_BLENDED_REACTIVE)?;
        let fragment_blended_atmosphere = words(FRAGMENT_BLENDED_ATMOSPHERE)?;
        let fragment_blended_atmosphere_reactive = words(FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE)?;
        let depth_vertex = words(DEPTH_VERTEX)?;
        let depth_vertex_masked = words(DEPTH_VERTEX_MASKED)?;
        let coverage = words(PREPASS_COVERAGE)?;
        let depth_vertex_motion = words(DEPTH_VERTEX_MOTION)?;
        let prepass_motion = words(PREPASS_MOTION)?;
        let depth_vertex_masked_motion = words(DEPTH_VERTEX_MASKED_MOTION)?;
        let coverage_motion = words(PREPASS_COVERAGE_MOTION)?;
        let background_vertex = words(BACKGROUND_VERTEX)?;
        let background_motion = words(BACKGROUND_MOTION)?;
        let sky_fragment = words(SKY_FRAGMENT)?;
        let composite_vertex = words(COMPOSITE_VERTEX)?;
        let composite_fragment = words(COMPOSITE_FRAGMENT_MS)?;
        let formats = [color];
        let reactive_formats = [color, REACTIVE_FORMAT];
        let background_formats = [MOTION_FORMAT];
        let batch = Batch {
            vertex: &vertex,
            depth,
            samples,
        };
        // The shading variants are described (made on first use, ADR 0079), the rest made.
        let mut lazy_descs = Vec::with_capacity(3 * 4 + 5);
        let mut descs = Vec::with_capacity(6);
        for v in 0..VARIANTS.len() {
            lazy_descs.push(batch.equal(&fragment, c"fragmentMain", &formats, v));
            lazy_descs.push(batch.blended(&fragment_blended, c"fragmentBlended", &formats, v));
            lazy_descs.push(batch.blended(
                &fragment_blended_reactive,
                c"fragmentBlendedReactive",
                &reactive_formats,
                v,
            ));
        }
        // The prepass: depth only; masked materials' alpha as coverage (no color target:
        // the fragment's alpha at location 0 is all alpha to coverage reads).
        descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            depth_format: depth,
            dynamic_culling: true,
            samples,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &depth_vertex,
                    entry: c"depthVertex",
                },
                None,
            )
        }));
        descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            depth_format: depth,
            dynamic_culling: true,
            samples,
            alpha_to_coverage: true,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &depth_vertex_masked,
                    entry: c"depthVertexMasked",
                },
                Some(ShaderStageDesc {
                    spirv: &coverage,
                    entry: c"prepassCoverage",
                }),
            )
        }));
        // The same writing motion vectors (the masked one's alpha at location 0 still
        // drives the coverage; the motion target keeps red and green).
        descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            color_formats: &background_formats,
            depth_format: depth,
            dynamic_culling: true,
            samples,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &depth_vertex_motion,
                    entry: c"depthVertexMotion",
                },
                Some(ShaderStageDesc {
                    spirv: &prepass_motion,
                    entry: c"prepassMotion",
                }),
            )
        }));
        descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            color_formats: &background_formats,
            depth_format: depth,
            dynamic_culling: true,
            samples,
            alpha_to_coverage: true,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &depth_vertex_masked_motion,
                    entry: c"depthVertexMaskedMotion",
                },
                Some(ShaderStageDesc {
                    spirv: &coverage_motion,
                    entry: c"prepassCoverageMotion",
                }),
            )
        }));
        let full_screen = |color_formats, fragment| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats,
                depth_format: depth,
                cull_mode: vk::CullModeFlags::NONE,
                depth_write: false,
                depth_compare: vk::CompareOp::EQUAL,
                samples,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &background_vertex,
                        entry: c"backgroundVertex",
                    },
                    Some(fragment),
                )
            })
        };
        descs.push(full_screen(
            &background_formats,
            ShaderStageDesc {
                spirv: &background_motion,
                entry: c"backgroundMotion",
            },
        ));
        descs.push(full_screen(
            &formats,
            ShaderStageDesc {
                spirv: &sky_fragment,
                entry: c"skyFragment",
            },
        ));
        lazy_descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            color_formats: &formats,
            cull_mode: vk::CullModeFlags::NONE,
            depth_test: false,
            depth_write: false,
            blend: Blend::Premultiplied,
            samples,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &composite_vertex,
                    entry: c"compositeVertex",
                },
                Some(ShaderStageDesc {
                    spirv: &composite_fragment,
                    entry: c"compositeFragmentMs",
                }),
            )
        }));
        for lit in [2, 3] {
            lazy_descs.push(batch.blended(
                &fragment_blended_atmosphere,
                c"fragmentBlendedAtmosphere",
                &formats,
                lit,
            ));
            lazy_descs.push(batch.blended(
                &fragment_blended_atmosphere_reactive,
                c"fragmentBlendedAtmosphereReactive",
                &reactive_formats,
                lit,
            ));
        }
        let mut pipelines =
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
        let mut next = || pipelines.next().ok_or_else(short);
        let prepass = [next()?, next()?];
        let prepass_motion = [next()?, next()?];
        let background = next()?;
        let sky = next()?;
        let mut described =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &lazy_descs)
                .into_iter();
        let mut next = || described.next().ok_or_else(short);
        let mut shading = || -> Result<MsaaShading, RhiError> {
            Ok(MsaaShading {
                shade: next()?,
                blended: [next()?, next()?],
            })
        };
        let shading = [shading()?, shading()?, shading()?, shading()?];
        let composite = next()?;
        let blended_atmosphere = [[next()?, next()?], [next()?, next()?]];
        Ok(Self {
            samples,
            prepass,
            prepass_motion,
            shading,
            background,
            sky,
            composite,
            blended_atmosphere,
            ao: None,
            subsurface: None,
            shadowed: None,
            contact: None,
            reflect: None,
        })
    }

    /// Makes the pipelines of `features` not made yet.
    #[allow(clippy::too_many_lines)]
    pub(super) fn ensure(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        color: vk::Format,
        depth: vk::Format,
        features: Features,
    ) -> Result<(), RhiError> {
        let shadowed_ready = self.shadowed.as_ref().is_none_or(|s| {
            (!features.ao || s.ao.is_some()) && (!features.subsurface || s.subsurface.is_some())
        });
        let contact_ready = !features.contact
            || self.contact.as_ref().is_some_and(|c| {
                (!features.ao || c.ao.is_some())
                    && (!features.shadowed || c.shadowed.is_some())
                    && (!(features.ao && features.shadowed) || c.shadowed_ao.is_some())
            });
        if (!features.ao || self.ao.is_some())
            && (!features.subsurface || self.subsurface.is_some())
            && (!features.shadowed || self.shadowed.is_some())
            && shadowed_ready
            && contact_ready
            && (!features.reflect || self.reflect.is_some())
        {
            // Every frame: nothing to make (and nothing allocated).
            return Ok(());
        }
        let samples = self.samples;
        let vertex = words(VERTEX)?;
        let batch = Batch {
            vertex: &vertex,
            depth,
            samples,
        };
        let formats = [color];
        let reactive_formats = [color, REACTIVE_FORMAT];
        let subsurface_formats = [color, IRRADIANCE_FORMAT, ALBEDO_FORMAT];
        let layout = bindless.shared_pipeline_layout();

        if features.ao && self.ao.is_none() {
            let fragment = words(FRAGMENT_AO)?;
            let mut descs = Vec::with_capacity(4);
            shades(
                &batch,
                &mut descs,
                (&fragment, c"fragmentMainAo"),
                &formats,
                &[0, 1, 2, 3],
            );
            let mut p = LazyPipeline::describe_many(device, layout, &descs).into_iter();
            self.ao = Some(take(&mut p)?);
        }

        if features.subsurface && self.subsurface.is_none() {
            let fragment = words(FRAGMENT_SUBSURFACE)?;
            let scatter = words(SCATTER_FRAGMENT_MS)?;
            let depth_vertex = words(DEPTH_VERTEX)?;
            let mut descs = Vec::with_capacity(5);
            shades(
                &batch,
                &mut descs,
                (&fragment, c"fragmentSubsurface"),
                &subsurface_formats,
                &[0, 1, 2, 3],
            );
            descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats: &formats,
                depth_format: depth,
                dynamic_culling: true,
                depth_write: false,
                depth_compare: vk::CompareOp::EQUAL,
                blend: Blend::Additive,
                samples,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &depth_vertex,
                        entry: c"depthVertex",
                    },
                    Some(ShaderStageDesc {
                        spirv: &scatter,
                        entry: c"scatterFragmentMs",
                    }),
                )
            }));
            let mut p = LazyPipeline::describe_many(device, layout, &descs).into_iter();
            let shading = take(&mut p)?;
            let scatter = p.next().ok_or_else(short)?;
            let resolve = nearest_resolve(
                device,
                bindless,
                samples,
                RESOLVE_SUBSURFACE,
                &[IRRADIANCE_FORMAT, ALBEDO_FORMAT],
            )?;
            self.subsurface = Some((shading, scatter, resolve));
        }

        if features.shadowed && self.shadowed.is_none() {
            let still = words(FRAGMENT_SHADOWED)?;
            let blended = words(FRAGMENT_BLENDED_SHADOWED)?;
            let blended_reactive = words(FRAGMENT_BLENDED_REACTIVE_SHADOWED)?;
            let atmosphere = words(FRAGMENT_BLENDED_ATMOSPHERE_SHADOWED)?;
            let atmosphere_reactive = words(FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE_SHADOWED)?;
            let mut descs = Vec::with_capacity(8);
            for lit in [1, 3] {
                descs.push(batch.equal(&still, c"fragmentMainShadowed", &formats, lit));
                descs.push(batch.blended(&blended, c"fragmentBlendedShadowed", &formats, lit));
                descs.push(batch.blended(
                    &blended_reactive,
                    c"fragmentBlendedReactiveShadowed",
                    &reactive_formats,
                    lit,
                ));
            }
            descs.push(batch.blended(
                &atmosphere,
                c"fragmentBlendedAtmosphereShadowed",
                &formats,
                3,
            ));
            descs.push(batch.blended(
                &atmosphere_reactive,
                c"fragmentBlendedAtmosphereReactiveShadowed",
                &reactive_formats,
                3,
            ));
            let mut p = LazyPipeline::describe_many(device, layout, &descs).into_iter();
            let mut next = || p.next().ok_or_else(short);
            let mut shading = || -> Result<MsaaShading, RhiError> {
                Ok(MsaaShading {
                    shade: next()?,
                    blended: [next()?, next()?],
                })
            };
            let shading = [shading()?, shading()?];
            let blended_atmosphere = [next()?, next()?];
            self.shadowed = Some(MsaaShadowed {
                shading,
                blended_atmosphere,
                ao: None,
                subsurface: None,
            });
        }

        if let Some(shadowed) = self.shadowed.as_mut() {
            if features.ao && shadowed.ao.is_none() {
                let fragment = words(FRAGMENT_AO_SHADOWED)?;
                let mut descs = Vec::with_capacity(2);
                shades(
                    &batch,
                    &mut descs,
                    (&fragment, c"fragmentMainAoShadowed"),
                    &formats,
                    &[1, 3],
                );
                let mut p = LazyPipeline::describe_many(device, layout, &descs).into_iter();
                shadowed.ao = Some(take(&mut p)?);
            }
            if features.subsurface && shadowed.subsurface.is_none() {
                let fragment = words(FRAGMENT_SUBSURFACE_SHADOWED)?;
                let mut descs = Vec::with_capacity(2);
                shades(
                    &batch,
                    &mut descs,
                    (&fragment, c"fragmentSubsurfaceShadowed"),
                    &subsurface_formats,
                    &[1, 3],
                );
                let mut p = LazyPipeline::describe_many(device, layout, &descs).into_iter();
                shadowed.subsurface = Some(take(&mut p)?);
            }
        }

        if features.contact {
            let contact_set = |fragment: &[u8],
                               entry: &'static CStr,
                               variants: &[usize]|
             -> Result<Vec<LazyPipeline>, RhiError> {
                let fragment = words(fragment)?;
                let mut descs = Vec::with_capacity(variants.len());
                shades(&batch, &mut descs, (&fragment, entry), &formats, variants);
                Ok(LazyPipeline::describe_many(device, layout, &descs))
            };
            if self.contact.is_none() {
                let shading = contact_set(FRAGMENT_CONTACT, c"fragmentMainContact", &[0, 1, 2, 3])?;
                self.contact = Some(MsaaContact {
                    shading: shading.try_into().map_err(|_| short())?,
                    ao: None,
                    shadowed: None,
                    shadowed_ao: None,
                });
            }
            if let Some(contact) = self.contact.as_mut() {
                if features.ao && contact.ao.is_none() {
                    let set =
                        contact_set(FRAGMENT_AO_CONTACT, c"fragmentMainAoContact", &[0, 1, 2, 3])?;
                    contact.ao = Some(set.try_into().map_err(|_| short())?);
                }
                if features.shadowed && contact.shadowed.is_none() {
                    let set = contact_set(
                        FRAGMENT_CONTACT_SHADOWED,
                        c"fragmentMainContactShadowed",
                        &[1, 3],
                    )?;
                    contact.shadowed = Some(set.try_into().map_err(|_| short())?);
                }
                if features.ao && features.shadowed && contact.shadowed_ao.is_none() {
                    let set = contact_set(
                        FRAGMENT_AO_CONTACT_SHADOWED,
                        c"fragmentMainAoContactShadowed",
                        &[1, 3],
                    )?;
                    contact.shadowed_ao = Some(set.try_into().map_err(|_| short())?);
                }
            }
        }

        if features.reflect && self.reflect.is_none() {
            let fragments = REFLECT_FRAGMENTS
                .iter()
                .map(|&(bytes, entry)| Ok((words(bytes)?, entry)))
                .collect::<Result<Vec<_>, RhiError>>()?;
            let reflect_formats = [color, NORMAL_ROUGHNESS_FORMAT, WEIGHT_FORMAT];
            // As `ReflectionShading::new`: the environment variants, without and with
            // local lights.
            let mut descs = Vec::with_capacity(12);
            let mut slots = Vec::with_capacity(12);
            for (lights, variant) in [(0, 2), (1, 3)] {
                for (k, (fragment, entry)) in fragments.iter().enumerate() {
                    if k & 1 != 0 && lights == 0 {
                        continue;
                    }
                    descs.push(batch.equal(fragment, entry, &reflect_formats, variant));
                    slots.push((lights, k));
                }
            }
            let pipelines = LazyPipeline::describe_many(device, layout, &descs);
            let mut table: ReflectShadingTable = Default::default();
            for ((lights, k), pipeline) in slots.into_iter().zip(pipelines) {
                table[lights][k >> 2][(k >> 1) & 1][k & 1] = Some(pipeline);
            }
            let composite = composite_pipelines(device, bindless, color, samples)?;
            let resolve = nearest_resolve(
                device,
                bindless,
                samples,
                RESOLVE_REFLECT,
                &[
                    NORMAL_ROUGHNESS_FORMAT,
                    WEIGHT_FORMAT,
                    NORMAL_ROUGHNESS_FORMAT,
                    WEIGHT_FORMAT,
                    FAR_DEPTH_FORMAT,
                ],
            )?;
            self.reflect = Some(Box::new((table, composite, resolve)));
        }
        Ok(())
    }

    /// The reflection shading for a view, as `ReflectionShading::pipeline`.
    pub(super) fn reflect_pipeline(
        &self,
        lights: bool,
        ao: bool,
        contact: bool,
        shadowed: bool,
    ) -> Option<&LazyPipeline> {
        let (table, _, _) = self.reflect.as_deref()?;
        table[usize::from(lights)][usize::from(ao)][usize::from(contact)]
            [usize::from(shadowed && lights)]
        .as_ref()
    }
}
