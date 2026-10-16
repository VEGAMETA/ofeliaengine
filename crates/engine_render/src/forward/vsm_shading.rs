//! The shading of a view with the sun's virtual shadow pages that the visibility pass's
//! mask does not serve (ADR 0093 step 3d): transparent surfaces, which look the pages up
//! themselves, and subsurface materials, whose front reads the mask and whose
//! transmission reads the pages. Their `*Vsm` entry points are twins of the ones views
//! without pages draw with, so those carry no code for the pages (the register rule,
//! CLAUDE.md §5). And the pass that marks the transparent surfaces' pages.
//!
//! Described with the pages and made on first use (ADR 0079).

use super::*;

const BLENDED: [(&[u8], &core::ffi::CStr); 2] = [
    (
        include_bytes!("../../shaders/forward.fragmentBlendedVsm.spv"),
        c"fragmentBlendedVsm",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentBlendedReactiveVsm.spv"),
        c"fragmentBlendedReactiveVsm",
    ),
];
const BLENDED_SHADOWED: [(&[u8], &core::ffi::CStr); 2] = [
    (
        include_bytes!("../../shaders/forward.fragmentBlendedShadowedVsm.spv"),
        c"fragmentBlendedShadowedVsm",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentBlendedReactiveShadowedVsm.spv"),
        c"fragmentBlendedReactiveShadowedVsm",
    ),
];
const BLENDED_ATMOSPHERE: [(&[u8], &core::ffi::CStr); 2] = [
    (
        include_bytes!("../../shaders/forward.fragmentBlendedAtmosphereVsm.spv"),
        c"fragmentBlendedAtmosphereVsm",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentBlendedAtmosphereReactiveVsm.spv"),
        c"fragmentBlendedAtmosphereReactiveVsm",
    ),
];
const BLENDED_ATMOSPHERE_SHADOWED: [(&[u8], &core::ffi::CStr); 2] = [
    (
        include_bytes!("../../shaders/forward.fragmentBlendedAtmosphereShadowedVsm.spv"),
        c"fragmentBlendedAtmosphereShadowedVsm",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentBlendedAtmosphereReactiveShadowedVsm.spv"),
        c"fragmentBlendedAtmosphereReactiveShadowedVsm",
    ),
];
/// Subsurface materials: plain, with local lights' shadows, with probe GI, with both.
const SUBSURFACE: [(&[u8], &core::ffi::CStr); 4] = [
    (
        include_bytes!("../../shaders/forward.fragmentSubsurfaceVsm.spv"),
        c"fragmentSubsurfaceVsm",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentSubsurfaceShadowedVsm.spv"),
        c"fragmentSubsurfaceShadowedVsm",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentSubsurfaceGiVsm.spv"),
        c"fragmentSubsurfaceGiVsm",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentSubsurfaceShadowedGiVsm.spv"),
        c"fragmentSubsurfaceShadowedGiVsm",
    ),
];
const MARK: &[u8] = include_bytes!("../../shaders/forward.vsmMarkTransparent.spv");

/// An entry point pair's SPIR-V and names (without and with the reactive mask), and the
/// shading variants it is described for.
type BlendedSet<'a> = (
    &'a [Vec<u32>; 2],
    &'a [(&'a [u8], &'a core::ffi::CStr); 2],
    &'a [usize],
);

/// The `*Vsm` shading and the transparent surfaces' marking (see the module
/// documentation).
#[derive(Debug)]
pub(super) struct VsmShading {
    /// Transparent surfaces per shading variant: without and with the reactive mask.
    blended: [[LazyPipeline; 2]; 4],
    /// With local lights' shadows, per environment bit (the local-light variants).
    blended_shadowed: [[LazyPipeline; 2]; 2],
    /// Under an atmosphere or fog (the environment variants), per local-light bit.
    blended_atmosphere: [[LazyPipeline; 2]; 2],
    /// Under an atmosphere or fog, with local lights' shadows.
    blended_atmosphere_shadowed: [LazyPipeline; 2],
    /// Subsurface materials per shading variant: without and with probe GI.
    subsurface: [[LazyPipeline; 2]; 4],
    /// With local lights' shadows, per environment bit: without and with probe GI.
    subsurface_shadowed: [[LazyPipeline; 2]; 2],
    /// The transparent surfaces' pages marked (`vsmMarkTransparent`): their depth tested
    /// against the prepass's, nothing written but the page tables.
    pub(super) mark: LazyPipeline,
}

impl VsmShading {
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
        let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
        let vertex = words(VERTEX)?;
        let blended = [words(BLENDED[0].0)?, words(BLENDED[1].0)?];
        let blended_shadowed = [words(BLENDED_SHADOWED[0].0)?, words(BLENDED_SHADOWED[1].0)?];
        let blended_atmosphere = [
            words(BLENDED_ATMOSPHERE[0].0)?,
            words(BLENDED_ATMOSPHERE[1].0)?,
        ];
        let blended_atmosphere_shadowed = [
            words(BLENDED_ATMOSPHERE_SHADOWED[0].0)?,
            words(BLENDED_ATMOSPHERE_SHADOWED[1].0)?,
        ];
        let subsurface = [
            words(SUBSURFACE[0].0)?,
            words(SUBSURFACE[1].0)?,
            words(SUBSURFACE[2].0)?,
            words(SUBSURFACE[3].0)?,
        ];
        let mark = words(MARK)?;
        // As `ForwardRenderer::new`'s transparent and subsurface pipelines.
        let plain_formats = [color_format];
        let reactive_formats = [color_format, REACTIVE_FORMAT];
        let formats: [&[vk::Format]; 2] = [&plain_formats, &reactive_formats];
        let subsurface_formats = [color_format, IRRADIANCE_FORMAT, ALBEDO_FORMAT];
        let mut descs = Vec::with_capacity(31);
        // Transparent: premultiplied, depth tested, not written.
        let blended_sets: [BlendedSet<'_>; 4] = [
            (&blended, &BLENDED, &[0, 1, 2, 3]),
            (&blended_shadowed, &BLENDED_SHADOWED, &[1, 3]),
            (&blended_atmosphere, &BLENDED_ATMOSPHERE, &[2, 3]),
            (
                &blended_atmosphere_shadowed,
                &BLENDED_ATMOSPHERE_SHADOWED,
                &[3],
            ),
        ];
        for (fragments, table, variants) in blended_sets {
            for &variant in variants {
                for reactive in 0..2 {
                    descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
                        depth_write: false,
                        depth_compare: vk::CompareOp::LESS,
                        blend: Blend::Premultiplied,
                        ..shading_desc(
                            &vertex,
                            &fragments[reactive],
                            table[reactive].1,
                            formats[reactive],
                            depth_format,
                            &VARIANTS[variant],
                        )
                    }));
                }
            }
        }
        // Subsurface: after the prepass, depth tested for equality; without and with
        // probe GI.
        let subsurface_sets: [(usize, usize, &[usize]); 2] =
            [(0, 2, &[0, 1, 2, 3]), (1, 3, &[1, 3])];
        for (plain, gi, variants) in subsurface_sets {
            for &variant in variants {
                for k in [plain, gi] {
                    descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
                        depth_write: false,
                        depth_compare: vk::CompareOp::EQUAL,
                        ..shading_desc(
                            &vertex,
                            &subsurface[k],
                            SUBSURFACE[k].1,
                            &subsurface_formats,
                            depth_format,
                            &VARIANTS[variant],
                        )
                    }));
                }
            }
        }
        descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            depth_write: false,
            depth_compare: vk::CompareOp::LESS,
            ..shading_desc(
                &vertex,
                &mark,
                c"vsmMarkTransparent",
                &[],
                depth_format,
                &VARIANTS[0],
            )
        }));
        let mut pipelines =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &descs)
                .into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "virtual shadow shading pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let mut pair = || -> Result<[LazyPipeline; 2], RhiError> { Ok([next()?, next()?]) };
        let blended = [pair()?, pair()?, pair()?, pair()?];
        let blended_shadowed = [pair()?, pair()?];
        let blended_atmosphere = [pair()?, pair()?];
        let blended_atmosphere_shadowed = pair()?;
        let subsurface = [pair()?, pair()?, pair()?, pair()?];
        let subsurface_shadowed = [pair()?, pair()?];
        Ok(Self {
            blended,
            blended_shadowed,
            blended_atmosphere,
            blended_atmosphere_shadowed,
            subsurface,
            subsurface_shadowed,
            mark: next()?,
        })
    }

    /// The transparent surfaces' shading of `variant` (bit 0 local lights, bit 1 an
    /// environment): under an atmosphere or fog, with local lights' shadows (the local-light
    /// variants only), writing the reactive mask (`reactive` 1).
    pub(super) fn blended(
        &self,
        variant: usize,
        atmosphere: bool,
        shadowed: bool,
        reactive: usize,
    ) -> &LazyPipeline {
        let shadowed = shadowed && variant & 1 != 0;
        match (atmosphere, shadowed) {
            (true, true) => &self.blended_atmosphere_shadowed[reactive],
            (true, false) => &self.blended_atmosphere[variant & 1][reactive],
            (false, true) => &self.blended_shadowed[shadowed_index(variant)][reactive],
            (false, false) => &self.blended[variant & 3][reactive],
        }
    }

    /// Subsurface materials' shading of `variant`, with local lights' shadows (the
    /// local-light variants only) and probe GI.
    pub(super) fn subsurface(&self, variant: usize, shadowed: bool, gi: bool) -> &LazyPipeline {
        let gi = usize::from(gi);
        if shadowed && variant & 1 != 0 {
            &self.subsurface_shadowed[shadowed_index(variant)][gi]
        } else {
            &self.subsurface[variant & 3][gi]
        }
    }
}
