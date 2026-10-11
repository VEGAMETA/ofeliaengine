//! The forward renderer's pipeline sets, one per feature of the shading: plain, with ambient
//! occlusion, subsurface scattering, local shadows, contact shadows and reflections, each
//! made (or described, for creation on first use, ADR 0079) when the feature is turned on.
//! A view without a feature never binds its pipelines (the register rule, CLAUDE.md §5).

use super::*;

/// The shading pipelines, compiled with or without local lights. Like every shading
/// variant, made when a frame first draws with them or by the device's warm-up before
/// that (ADR 0079).
#[derive(Debug)]
pub(super) struct Shading {
    /// Forward shading: opaque, masked.
    pub(super) main: [LazyPipeline; 2],
    /// Forward shading after the depth prepass: depth tested for equality, not written;
    /// no alpha test (the prepass did it).
    pub(super) shade: LazyPipeline,
    /// Forward shading that also writes motion vectors, for views without the depth
    /// prepass (after it the prepass writes them): opaque, masked.
    pub(super) main_motion: [LazyPipeline; 2],
    /// Transparent surfaces: premultiplied blending, depth tested but not written; without
    /// and with the reactive mask.
    pub(super) blended: [LazyPipeline; 2],
}

/// A pipeline a frame draws with, made now if this is its first use (ADR 0079). `None`,
/// the view not drawn, when the driver could not make it (`engine_rhi` logs why).
pub(super) fn ready(pipeline: &LazyPipeline) -> Option<&Pipeline> {
    pipeline.get_or_create().ok()
}

/// The driver's statistics of a shading pipeline, made now if no frame has drawn with it.
pub(super) fn lazy_statistics(
    pipeline: &LazyPipeline,
    device: &Device,
) -> Vec<engine_rhi::PipelineStatistic> {
    pipeline
        .get_or_create()
        .map(|p| p.statistics(device))
        .unwrap_or_default()
}

/// Ambient occlusion (ADR 0058): its passes, and the shading that reads it after the depth
/// prepass (`fragmentMainAo`), per shading variant. The other entry points have no code
/// for it (see `shade` in `forward.slang`).
#[derive(Debug)]
pub(super) struct AoPasses {
    pub(super) passes: AmbientOcclusion,
    pub(super) shading: [LazyPipeline; 4],
}

/// The specialization constants of each shading variant: `LOCAL_LIGHTS`, `ENVIRONMENT`
/// (`forward.slang`).
pub(super) const VARIANTS: [[u32; 2]; 4] = [[0, 0], [1, 0], [0, 1], [1, 1]];

/// A shading pipeline of the forward pass.
pub(super) fn shading_desc<'a>(
    vertex: &'a [u32],
    fragment: &'a [u32],
    entry: &'a core::ffi::CStr,
    color_formats: &'a [vk::Format],
    depth_format: vk::Format,
    specialization: &'a [u32],
) -> GraphicsPipelineDesc<'a> {
    GraphicsPipelineDesc {
        specialization,
        color_formats,
        depth_format,
        // Cull mode and front face come with each batch's pipeline state.
        dynamic_culling: true,
        ..GraphicsPipelineDesc::new(
            ShaderStageDesc {
                spirv: vertex,
                entry: c"vertexMain",
            },
            Some(ShaderStageDesc {
                spirv: fragment,
                entry,
            }),
        )
    }
}

impl AoPasses {
    pub(super) fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT_AO).ok_or_else(invalid)?;
        let formats = [color_format];
        // After the depth prepass: tested for equality, not written.
        let shade = |fragment, entry, formats, variant: usize| GraphicsPipelineDesc {
            depth_write: false,
            depth_compare: vk::CompareOp::EQUAL,
            ..shading_desc(
                &vertex,
                fragment,
                entry,
                formats,
                depth_format,
                &VARIANTS[variant],
            )
        };
        let descs: Vec<PipelineDesc<'_>> = (0..VARIANTS.len())
            .map(|v| PipelineDesc::Graphics(shade(&fragment, c"fragmentMainAo", &formats[..], v)))
            .collect();
        let mut pipelines =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &descs)
                .into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "ambient occlusion pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let shading = [next()?, next()?, next()?, next()?];
        Ok(Self {
            passes: AmbientOcclusion::new(device, bindless)?,
            shading,
        })
    }
}

/// The pipelines of subsurface materials (ADR 0059), created with the first profiles.
#[derive(Debug)]
pub(super) struct SubsurfacePipelines {
    /// Per shading variant, their shading after the prepass (also writing their diffuse
    /// light and albedo).
    pub(super) shading: [LazyPipeline; 4],
    /// The scatter pass: the subsurface batches again, adding where their depth is.
    pub(super) scatter: LazyPipeline,
}

impl SubsurfacePipelines {
    pub(super) fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let depth_vertex = spirv_words(DEPTH_VERTEX).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT_SUBSURFACE).ok_or_else(invalid)?;
        let scatter = spirv_words(SCATTER_FRAGMENT).ok_or_else(invalid)?;
        let formats = [color_format, IRRADIANCE_FORMAT, ALBEDO_FORMAT];
        // After the prepass, as `ForwardRenderer::shading`'s `shade`.
        let shade = |fragment, entry, color_formats, specialization| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                specialization,
                color_formats,
                depth_format,
                dynamic_culling: true,
                depth_write: false,
                depth_compare: vk::CompareOp::EQUAL,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"vertexMain",
                    },
                    Some(ShaderStageDesc {
                        spirv: fragment,
                        entry,
                    }),
                )
            })
        };
        let mut descs = Vec::with_capacity(VARIANTS.len() + 1);
        for variant in &VARIANTS {
            descs.push(shade(
                &fragment,
                c"fragmentSubsurface",
                &formats[..],
                variant,
            ));
        }
        let color = [color_format];
        descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            color_formats: &color,
            depth_format,
            dynamic_culling: true,
            depth_write: false,
            depth_compare: vk::CompareOp::EQUAL,
            blend: Blend::Additive,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &depth_vertex,
                    entry: c"depthVertex",
                },
                Some(ShaderStageDesc {
                    spirv: &scatter,
                    entry: c"scatterFragment",
                }),
            )
        }));
        let mut pipelines =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &descs)
                .into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "subsurface pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let shading = [next()?, next()?, next()?, next()?];
        Ok(Self {
            shading,
            scatter: next()?,
        })
    }
}

/// Shading that looks up local lights' shadows (ADR 0071), per environment bit (the
/// local-light variants of [`VARIANTS`]: `[1, 0]`, `[1, 1]`), created with the feature.
#[derive(Debug)]
pub(super) struct ShadowedPipelines {
    pub(super) shading: [Shading; 2],
    /// Transparent surfaces under an atmosphere or fog: without and with the reactive mask.
    pub(super) blended_atmosphere: [LazyPipeline; 2],
    /// With ambient occlusion, per environment bit.
    pub(super) ao: Option<[LazyPipeline; 2]>,
    /// Subsurface materials, per environment bit.
    pub(super) subsurface: Option<[LazyPipeline; 2]>,
    /// The shadow views: opaque (depth only), masked.
    pub(super) depth: [Pipeline; 2],
    pub(super) cull: ComputeShader<Push>,
}

impl ShadowedPipelines {
    pub(super) fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
        settings: &LocalShadowSettings,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
        let vertex = words(VERTEX)?;
        let fragment = words(FRAGMENT_SHADOWED)?;
        let fragment_masked = words(FRAGMENT_MASKED_SHADOWED)?;
        let fragment_motion = words(FRAGMENT_MOTION_SHADOWED)?;
        let fragment_masked_motion = words(FRAGMENT_MASKED_MOTION_SHADOWED)?;
        let fragment_blended = words(FRAGMENT_BLENDED_SHADOWED)?;
        let fragment_blended_reactive = words(FRAGMENT_BLENDED_REACTIVE_SHADOWED)?;
        let fragment_blended_atmosphere = words(FRAGMENT_BLENDED_ATMOSPHERE_SHADOWED)?;
        let fragment_blended_atmosphere_reactive =
            words(FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE_SHADOWED)?;
        let shadow_vertex = words(LOCAL_SHADOW_VERTEX)?;
        let shadow_vertex_masked = words(LOCAL_SHADOW_VERTEX_MASKED)?;
        let shadow_fragment_masked = words(SHADOW_FRAGMENT_MASKED)?;
        let formats = [color_format];
        let motion_formats = [color_format, MOTION_FORMAT];
        let reactive_formats = [color_format, REACTIVE_FORMAT];
        // As `ForwardRenderer::new`'s, for the local-light variants only.
        let main = |fragment, entry, formats, equal: bool, variant: usize| GraphicsPipelineDesc {
            depth_write: !equal,
            depth_compare: if equal {
                vk::CompareOp::EQUAL
            } else {
                vk::CompareOp::LESS
            },
            ..shading_desc(
                &vertex,
                fragment,
                entry,
                formats,
                depth_format,
                &VARIANTS[variant],
            )
        };
        let blended = |fragment, entry, formats, variant: usize| GraphicsPipelineDesc {
            depth_write: false,
            depth_compare: vk::CompareOp::LESS,
            blend: Blend::Premultiplied,
            ..shading_desc(
                &vertex,
                fragment,
                entry,
                formats,
                depth_format,
                &VARIANTS[variant],
            )
        };
        let mut descs = Vec::with_capacity(20);
        for lit in [1, 3] {
            descs.extend([
                main(&fragment, c"fragmentMainShadowed", &formats[..], false, lit),
                main(
                    &fragment_masked,
                    c"fragmentMaskedShadowed",
                    &formats[..],
                    false,
                    lit,
                ),
                main(&fragment, c"fragmentMainShadowed", &formats[..], true, lit),
                main(
                    &fragment_motion,
                    c"fragmentMainMotionShadowed",
                    &motion_formats[..],
                    false,
                    lit,
                ),
                main(
                    &fragment_masked_motion,
                    c"fragmentMaskedMotionShadowed",
                    &motion_formats[..],
                    false,
                    lit,
                ),
                blended(
                    &fragment_blended,
                    c"fragmentBlendedShadowed",
                    &formats[..],
                    lit,
                ),
                blended(
                    &fragment_blended_reactive,
                    c"fragmentBlendedReactiveShadowed",
                    &reactive_formats[..],
                    lit,
                ),
            ]);
        }
        descs.push(blended(
            &fragment_blended_atmosphere,
            c"fragmentBlendedAtmosphereShadowed",
            &formats[..],
            3,
        ));
        descs.push(blended(
            &fragment_blended_atmosphere_reactive,
            c"fragmentBlendedAtmosphereReactiveShadowed",
            &reactive_formats[..],
            3,
        ));
        let masked_depth = ShaderStageDesc {
            spirv: &shadow_fragment_masked,
            entry: c"shadowFragmentMasked",
        };
        let shadow = |spirv, entry, fragment| GraphicsPipelineDesc {
            depth_format: settings.format,
            dynamic_culling: true,
            depth_bias: Some(settings.depth_bias),
            ..GraphicsPipelineDesc::new(ShaderStageDesc { spirv, entry }, fragment)
        };
        // The shadow views' and their culling now; the shading when first drawn with.
        let cull = ComputeSource::<Push>::new(CULL_SHADOW_VIEWS, c"cullShadowViews", &[])?;
        let now = [
            PipelineDesc::Graphics(shadow(&shadow_vertex, c"localShadowVertex", None)),
            PipelineDesc::Graphics(shadow(
                &shadow_vertex_masked,
                c"localShadowVertexMasked",
                Some(masked_depth),
            )),
            cull.desc(),
        ];
        let mut pipelines =
            Pipeline::create_many(device, bindless.pipeline_layout(), &now)?.into_iter();
        let fewer = || RhiError::Vulkan {
            call: "local shadow pipelines (fewer than described)",
            result: vk::Result::ERROR_UNKNOWN,
        };
        let depth = [
            pipelines.next().ok_or_else(fewer)?,
            pipelines.next().ok_or_else(fewer)?,
        ];
        let cull = cull.finish(pipelines.next().ok_or_else(fewer)?);
        let lazy: Vec<PipelineDesc<'_>> =
            descs.iter().copied().map(PipelineDesc::Graphics).collect();
        let mut described =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &lazy)
                .into_iter();
        let mut next = || described.next().ok_or_else(fewer);
        let mut shading = || -> Result<Shading, RhiError> {
            Ok(Shading {
                main: [next()?, next()?],
                shade: next()?,
                main_motion: [next()?, next()?],
                blended: [next()?, next()?],
            })
        };
        let shading = [shading()?, shading()?];
        let blended_atmosphere = [next()?, next()?];
        Ok(Self {
            shading,
            blended_atmosphere,
            ao: None,
            subsurface: None,
            depth,
            cull,
        })
    }

    /// The shading with ambient occlusion, or of subsurface materials, when the renderer
    /// has those features on and they are not made yet.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ensure_extras(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
        ao: bool,
        subsurface: bool,
    ) -> Result<(), RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
        let make_ao = ao && self.ao.is_none();
        let make_subsurface = subsurface && self.subsurface.is_none();
        if !make_ao && !make_subsurface {
            return Ok(());
        }
        let vertex = words(VERTEX)?;
        let fragment_ao = words(FRAGMENT_AO_SHADOWED)?;
        let fragment_subsurface = words(FRAGMENT_SUBSURFACE_SHADOWED)?;
        let formats = [color_format];
        let subsurface_formats = [color_format, IRRADIANCE_FORMAT, ALBEDO_FORMAT];
        // After the depth prepass: tested for equality, not written.
        let shade = |fragment, entry, formats, variant: usize| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                depth_write: false,
                depth_compare: vk::CompareOp::EQUAL,
                ..shading_desc(
                    &vertex,
                    fragment,
                    entry,
                    formats,
                    depth_format,
                    &VARIANTS[variant],
                )
            })
        };
        let mut descs = Vec::with_capacity(4);
        for lit in [1, 3] {
            if make_ao {
                descs.push(shade(
                    &fragment_ao,
                    c"fragmentMainAoShadowed",
                    &formats[..],
                    lit,
                ));
            }
            if make_subsurface {
                descs.push(shade(
                    &fragment_subsurface,
                    c"fragmentSubsurfaceShadowed",
                    &subsurface_formats[..],
                    lit,
                ));
            }
        }
        let mut pipelines =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &descs)
                .into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "local shadow pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let mut ao = [None, None];
        let mut subsurface = [None, None];
        for k in 0..2 {
            if make_ao {
                ao[k] = Some(next()?);
            }
            if make_subsurface {
                subsurface[k] = Some(next()?);
            }
        }
        if let [Some(a), Some(b)] = ao {
            self.ao = Some([a, b]);
        }
        if let [Some(a), Some(b)] = subsurface {
            self.subsurface = Some([a, b]);
        }
        Ok(())
    }
}

/// The sun's contact shadows (ADR 0071): their passes and the shading after the depth
/// prepass that reads their mask, made when they are first turned on; the sets for ambient
/// occlusion and local lights' shadows when those are on too.
#[derive(Debug)]
pub(super) struct ContactState {
    pub(super) passes: ContactShadows,
    /// Per shading variant.
    pub(super) shading: [LazyPipeline; 4],
    pub(super) ao: Option<[LazyPipeline; 4]>,
    /// With local lights' shadows, per environment bit (the local-light variants).
    pub(super) shadowed: Option<[LazyPipeline; 2]>,
    pub(super) shadowed_ao: Option<[LazyPipeline; 2]>,
}

/// Shading pipelines after the depth prepass (depth tested for equality, not written) of
/// `variants`.
pub(super) fn prepass_shading(
    device: &Device,
    bindless: &Bindless,
    color_format: vk::Format,
    depth_format: vk::Format,
    fragment: (&[u8], &core::ffi::CStr),
    variants: &[usize],
) -> Result<Vec<LazyPipeline>, RhiError> {
    let invalid = || RhiError::Vulkan {
        call: "embedded SPIR-V",
        result: vk::Result::ERROR_INVALID_SHADER_NV,
    };
    let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
    let words = spirv_words(fragment.0).ok_or_else(invalid)?;
    let formats = [color_format];
    let descs: Vec<PipelineDesc<'_>> = variants
        .iter()
        .map(|&variant| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                depth_write: false,
                depth_compare: vk::CompareOp::EQUAL,
                ..shading_desc(
                    &vertex,
                    &words,
                    fragment.1,
                    &formats,
                    depth_format,
                    &VARIANTS[variant],
                )
            })
        })
        .collect();
    Ok(LazyPipeline::describe_many(
        device,
        bindless.shared_pipeline_layout(),
        &descs,
    ))
}

/// The first four (or two) of `pipelines` as an array.
pub(super) fn array_of<T, const N: usize>(pipelines: Vec<T>) -> Result<[T; N], RhiError> {
    pipelines.try_into().map_err(|_| RhiError::Vulkan {
        call: "contact shadow pipelines (fewer than described)",
        result: vk::Result::ERROR_UNKNOWN,
    })
}

impl ContactState {
    pub(super) fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let shading = prepass_shading(
            device,
            bindless,
            color_format,
            depth_format,
            (FRAGMENT_CONTACT, c"fragmentMainContact"),
            &[0, 1, 2, 3],
        )?;
        Ok(Self {
            passes: ContactShadows::new(device, bindless)?,
            shading: array_of(shading)?,
            ao: None,
            shadowed: None,
            shadowed_ao: None,
        })
    }

    /// The sets for ambient occlusion and local lights' shadows, when those are on and the
    /// sets are not made yet.
    pub(super) fn ensure_extras(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        formats: (vk::Format, vk::Format),
        ao: bool,
        shadows: bool,
    ) -> Result<(), RhiError> {
        let (color, depth) = formats;
        if ao && self.ao.is_none() {
            self.ao = Some(array_of(prepass_shading(
                device,
                bindless,
                color,
                depth,
                (FRAGMENT_AO_CONTACT, c"fragmentMainAoContact"),
                &[0, 1, 2, 3],
            )?)?);
        }
        if shadows && self.shadowed.is_none() {
            self.shadowed = Some(array_of(prepass_shading(
                device,
                bindless,
                color,
                depth,
                (FRAGMENT_CONTACT_SHADOWED, c"fragmentMainContactShadowed"),
                &[1, 3],
            )?)?);
        }
        if ao && shadows && self.shadowed_ao.is_none() {
            self.shadowed_ao = Some(array_of(prepass_shading(
                device,
                bindless,
                color,
                depth,
                (
                    FRAGMENT_AO_CONTACT_SHADOWED,
                    c"fragmentMainAoContactShadowed",
                ),
                &[1, 3],
            )?)?);
        }
        Ok(())
    }
}

/// The local-light variant of a shading variant index (bit 1: environment).
pub(super) fn shadowed_index(variant: usize) -> usize {
    variant >> 1
}

/// Reflections (ADR 0073): their passes, and the shading of views with them after the
/// depth prepass, which also writes the normal, roughness and specular weight. By
/// `[lights][ambient occlusion][contact shadows][local shadows]` (local shadows only with
/// local lights).
#[derive(Debug)]
pub(super) struct ReflectionShading {
    pub(super) passes: ReflectionPasses,
    pub(super) shading: ReflectShadingTable,
}

/// [`ReflectionShading::shading`]'s table.
pub(super) type ReflectShadingTable = [[[[Option<LazyPipeline>; 2]; 2]; 2]; 2];

impl ReflectionShading {
    pub(super) fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let fragments = REFLECT_FRAGMENTS
            .iter()
            .map(|&(bytes, entry)| Ok((spirv_words(bytes).ok_or_else(invalid)?, entry)))
            .collect::<Result<Vec<_>, RhiError>>()?;
        let formats = [color_format, NORMAL_ROUGHNESS_FORMAT, WEIGHT_FORMAT];
        // The environment variants (2, 3): without and with local lights; after the
        // prepass, tested for equality.
        let mut descs = Vec::with_capacity(12);
        let mut slots = Vec::with_capacity(12);
        for (lights, variant) in [(0, 2), (1, 3)] {
            for (k, (fragment, entry)) in fragments.iter().enumerate() {
                let shadowed = k & 1 != 0;
                if shadowed && lights == 0 {
                    continue;
                }
                descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
                    depth_write: false,
                    depth_compare: vk::CompareOp::EQUAL,
                    ..shading_desc(
                        &vertex,
                        fragment,
                        entry,
                        &formats,
                        depth_format,
                        &VARIANTS[variant],
                    )
                }));
                slots.push((lights, k));
            }
        }
        let pipelines =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &descs);
        let mut shading: ReflectShadingTable = Default::default();
        for ((lights, k), pipeline) in slots.into_iter().zip(pipelines) {
            shading[lights][k >> 2][(k >> 1) & 1][k & 1] = Some(pipeline);
        }
        Ok(Self {
            passes: ReflectionPasses::new(device, bindless, color_format)?,
            shading,
        })
    }

    /// The shading for a view: with local lights (`lights`), ambient occlusion, the sun's
    /// contact shadows, local lights' shadows.
    pub(super) fn pipeline(
        &self,
        lights: bool,
        ao: bool,
        contact: bool,
        shadowed: bool,
    ) -> Option<&LazyPipeline> {
        self.shading[usize::from(lights)][usize::from(ao)][usize::from(contact)]
            [usize::from(shadowed && lights)]
        .as_ref()
    }
}

/// The shading features [`DecalShading::ensure`] makes decal variants for.
#[derive(Debug, Clone, Copy)]
pub(super) struct DecalFeatures {
    pub(super) ao: bool,
    pub(super) contact: bool,
    pub(super) shadows: bool,
    pub(super) reflections: bool,
}

/// Decals (ADR 0082): their passes, and the shading after the depth prepass that blends
/// the decal buffer in, made when they are first turned on, for each combination of the
/// other features in use (ambient occlusion, contact shadows, local shadows, reflections).
#[derive(Debug)]
pub(super) struct DecalShading {
    pub(super) passes: DecalPasses,
    /// `[variant][ambient occlusion][contact shadows][local shadows]` (local shadows only
    /// in the local-light variants).
    main: [[[[Option<LazyPipeline>; 2]; 2]; 2]; 4],
    /// `[lights][ambient occlusion][contact shadows][local shadows]`, as
    /// [`ReflectionShading::shading`].
    reflect: ReflectShadingTable,
}

impl DecalShading {
    pub(super) fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        Ok(Self {
            passes: DecalPasses::new(device, bindless)?,
            main: Default::default(),
            reflect: Default::default(),
        })
    }

    /// Describes the variants of `features` not described yet (made on first use or by
    /// the device's warm-up, ADR 0079).
    pub(super) fn ensure(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        formats: (vk::Format, vk::Format),
        features: DecalFeatures,
    ) -> Result<(), RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let (color, depth) = formats;
        // Slots by k = (ambient occlusion × 2 + contact shadows) × 2 + local shadows, as
        // the fragment tables.
        let wanted = |k: usize| {
            (k & 4 == 0 || features.ao)
                && (k & 2 == 0 || features.contact)
                && (k & 1 == 0 || features.shadows)
        };
        let mut main = Vec::new();
        let mut reflect = Vec::new();
        for k in (0..8).filter(|&k| wanted(k)) {
            let (a, c, l) = (k >> 2, (k >> 1) & 1, k & 1);
            for variant in 0..4 {
                if (l == 0 || variant & 1 != 0) && self.main[variant][a][c][l].is_none() {
                    main.push((k, variant));
                }
            }
            for lights in 0..2 {
                if features.reflections
                    && (l == 0 || lights != 0)
                    && self.reflect[lights][a][c][l].is_none()
                {
                    reflect.push((k, lights));
                }
            }
        }
        if main.is_empty() && reflect.is_empty() {
            return Ok(());
        }
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let words = |table: [(&[u8], &'static core::ffi::CStr); 8]| {
            table
                .into_iter()
                .map(|(bytes, entry)| Ok((spirv_words(bytes).ok_or_else(invalid)?, entry)))
                .collect::<Result<Vec<_>, RhiError>>()
        };
        let main_fragments = words(DECAL_FRAGMENTS)?;
        let reflect_fragments = words(REFLECT_DECAL_FRAGMENTS)?;
        let main_formats = [color];
        let reflect_formats = [color, NORMAL_ROUGHNESS_FORMAT, WEIGHT_FORMAT];
        let mut descs = Vec::with_capacity(main.len() + reflect.len());
        for &(k, variant) in &main {
            let (fragment, entry) = &main_fragments[k];
            descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
                depth_write: false,
                depth_compare: vk::CompareOp::EQUAL,
                ..shading_desc(
                    &vertex,
                    fragment,
                    entry,
                    &main_formats,
                    depth,
                    &VARIANTS[variant],
                )
            }));
        }
        for &(k, lights) in &reflect {
            let (fragment, entry) = &reflect_fragments[k];
            descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
                depth_write: false,
                depth_compare: vk::CompareOp::EQUAL,
                ..shading_desc(
                    &vertex,
                    fragment,
                    entry,
                    &reflect_formats,
                    depth,
                    &VARIANTS[2 + lights],
                )
            }));
        }
        let mut pipelines =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &descs)
                .into_iter();
        for (k, variant) in main {
            self.main[variant][k >> 2][(k >> 1) & 1][k & 1] = pipelines.next();
        }
        for (k, lights) in reflect {
            self.reflect[lights][k >> 2][(k >> 1) & 1][k & 1] = pipelines.next();
        }
        Ok(())
    }

    /// The shading of a view with decals: of shading `variant` (bit 0 local lights, bit 1
    /// an environment), writing reflections' inputs (`reflect`, environment variants
    /// only), with ambient occlusion, the sun's contact shadows, local lights' shadows.
    pub(super) fn pipeline(
        &self,
        variant: usize,
        reflect: bool,
        ao: bool,
        contact: bool,
        shadowed: bool,
    ) -> Option<&LazyPipeline> {
        let lights = variant & 1;
        let (a, c, l) = (
            usize::from(ao),
            usize::from(contact),
            usize::from(shadowed && lights != 0),
        );
        if reflect {
            self.reflect[lights][a][c][l].as_ref()
        } else {
            self.main[variant & 3][a][c][l].as_ref()
        }
    }
}
