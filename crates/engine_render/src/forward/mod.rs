//! Forward PBR renderer with a directional light and cascaded shadow maps (ADR 0014,
//! ADR 0044), drawn
//! GPU-driven (ADR 0034) with optional two-phase occlusion culling (ADR 0035), shading
//! glTF materials from a [`MaterialTable`] (ADR 0040).
//!
//! [`ForwardRenderer::add_passes`] turns the draw list into instances and batches and
//! declares these passes in the frame's render graph. A batch is a run of consecutive
//! draws that share a mesh of the [`GeometryPool`] and a pipeline state, drawn as one
//! indexed, instanced draw. There are eight states: opaque or masked (alpha-tested)
//! materials, single- or double-sided, drawn with a mirrored transform or not; batches
//! are ordered by state, opaque first, so every state is one indirect draw call.
//!
//! 1. **cull setup**: copies the batches' indirect draw commands, with no instances yet,
//!    from the upload ring into a graph buffer;
//! 2. **cull**: a compute pass tests every instance's world-space bounding box against the
//!    camera frustum, appends the visible ones to their batch's range of a visible list
//!    and counts them in the batch's command;
//! 3. **shadow cull** and **shadow**: the culling shader tests every instance against each
//!    shadow cascade ([`crate::shadow`]) and appends the casters to that cascade's
//!    commands; a depth-only pass draws each cascade into its layer of the shadow map (with
//!    depth bias); masked materials test their alpha. With a [`CascadeCache`] the layers
//!    live across frames and each frame draws only the strips the camera's movement
//!    exposed (ADR 0093); without one, a transient map is drawn whole;
//! 4. **forward**: metallic-roughness PBR (GGX, height-correlated Smith, Schlick; after
//!    Filament's documentation) with normal maps, occlusion, emission, geometric specular
//!    antialiasing and 3×3 PCF shadows from the finest cascade holding each pixel, into the
//!    view's color and depth targets.
//!
//! With an [`Occlusion`] state in the [`ForwardView`], culling and drawing run in two
//! phases: **cull early** keeps the instances that were visible in the view's previous
//! frame and **forward early** draws them; **hiz** reduces that depth into a pyramid of
//! farthest depths; **cull late** tests every instance against the frustum and the
//! pyramid, records what is visible for the next frame, and **forward late** draws the
//! visible instances the early phase missed. Objects hidden behind others are not drawn,
//! and nothing visible is ever missing: a wrong guess from the previous frame only costs
//! time.
//!
//! Materials with subsurface scattering (ADR 0059, [`crate::subsurface`]) are shaded by a
//! **forward subsurface** pass after the forward pass (the depth prepass then always
//! runs), which keeps their diffuse light apart; a **subsurface scatter** pass draws them
//! again and adds that light spread by their profile.
//!
//! Each drawing pass records one `vkCmdDrawIndexedIndirect` per pipeline state in use, so
//! beyond writing 64 bytes per instance into the [`UploadRing`] the CPU cost of a frame
//! does not grow with the number of draws. Shaders read everything through device
//! addresses and find the shadow map and material textures through the bindless set.
//!
//! [`GeometryPool`]: crate::mesh::GeometryPool
//! [`MaterialTable`]: crate::material::MaterialTable

use crate::ao::{AmbientOcclusion, AoSettings};
use crate::atmosphere::{AtmosphereTextures, AtmosphereView, COMPOSITE_FRAGMENT, COMPOSITE_VERTEX};
use crate::compute::{ComputePass, ComputeShader, ComputeSource, Dispatch};
use crate::contact::{ContactShadows, ContactView};
use crate::cull::Frustum;
use crate::decals::{DecalPasses, DecalTargets};
use crate::environment::{Environment, EnvironmentFrame, PREFILTERED_LEVELS};
use crate::gi::{GiFrame, GiSky};
use crate::gpu_scene::SceneFrame;
use crate::graph::{
    Access, Buffer, BufferDesc, BufferState, FrameGraph, ImportedBuffer, LoadOp, PassBuilder,
    ResolveMode, ShaderStages, StoreOp, Texture, TextureDesc,
};
use crate::hiz::{self, DepthPyramid, PyramidLayout};
use crate::lights::{
    LightData, LightLayout, ViewAxis, Z_BINS, cull_lights, write_bins, write_lights, write_rects,
};
use crate::local_shadows::{
    ChangeSource, Continuity, NO_SHADOW, Plan, ShadowCache, ShadowViewData,
};
use crate::material::{MaterialFrame, MaterialId, MaterialKind, MaterialTypeId};
use crate::mesh::{GeometryFrame, MeshId};
use crate::rays::RayFrame;
use crate::reflections::{
    MsComposite, NORMAL_ROUGHNESS_FORMAT, ProbeTextures, ReflectionPasses, ReflectionProbes,
    ViewInputs, ViewSetup, WEIGHT_FORMAT,
};
use crate::shadow::{self, CascadeFit, MAX_CASCADES};
use crate::subsurface::{
    self, ALBEDO_FORMAT, Footprint, GpuProfile, IRRADIANCE_FORMAT, SubsurfaceProfile,
    SubsurfaceSettings,
};
use crate::upload::UploadRing;
use core::cell::{Cell, RefCell};
use engine_core::math::{Mat4, Vec3, Vec4};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Blend, DepthBias, Device, GraphicsPipelineDesc, LazyPipeline,
    MemoryLocation, Pipeline, PipelineDesc, RhiError, SamplerKind, ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../../shaders/forward.vertexMain.spv");
const SHADOW_VERTEX: &[u8] = include_bytes!("../../shaders/forward.shadowVertex.spv");
const SHADOW_VERTEX_MASKED: &[u8] = include_bytes!("../../shaders/forward.shadowVertexMasked.spv");
// A cached cascade's pieces drawn at once (ADR 0093): each instance into its piece's
// viewport.
const SHADOW_VERTEX_PIECES: &[u8] = include_bytes!("../../shaders/forward.shadowVertexPieces.spv");
const SHADOW_VERTEX_MASKED_PIECES: &[u8] =
    include_bytes!("../../shaders/forward.shadowVertexMaskedPieces.spv");
pub(crate) const SHADOW_FRAGMENT_MASKED: &[u8] =
    include_bytes!("../../shaders/forward.shadowFragmentMasked.spv");
const FRAGMENT: &[u8] = include_bytes!("../../shaders/forward.fragmentMain.spv");
const FRAGMENT_MASKED: &[u8] = include_bytes!("../../shaders/forward.fragmentMasked.spv");
pub(crate) const DEPTH_VERTEX: &[u8] = include_bytes!("../../shaders/forward.depthVertex.spv");
const FRAGMENT_MOTION: &[u8] = include_bytes!("../../shaders/forward.fragmentMainMotion.spv");
const FRAGMENT_AO: &[u8] = include_bytes!("../../shaders/forward.fragmentMainAo.spv");
const FRAGMENT_MASKED_MOTION: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMaskedMotion.spv");
const BACKGROUND_VERTEX: &[u8] = include_bytes!("../../shaders/forward.backgroundVertex.spv");
const BACKGROUND_MOTION: &[u8] = include_bytes!("../../shaders/forward.backgroundMotion.spv");
const SKY_FRAGMENT: &[u8] = include_bytes!("../../shaders/forward.skyFragment.spv");
const FRAGMENT_SUBSURFACE: &[u8] = include_bytes!("../../shaders/forward.fragmentSubsurface.spv");
const SCATTER_FRAGMENT: &[u8] = include_bytes!("../../shaders/forward.scatterFragment.spv");
// Local lights' shadows (ADR 0071): culling and drawing the views, and the shading that
// looks them up.
const CULL_SHADOW_VIEWS: &[u8] = include_bytes!("../../shaders/forward.cullShadowViews.spv");
const LOCAL_SHADOW_VERTEX: &[u8] = include_bytes!("../../shaders/forward.localShadowVertex.spv");
const LOCAL_SHADOW_VERTEX_MASKED: &[u8] =
    include_bytes!("../../shaders/forward.localShadowVertexMasked.spv");
const FRAGMENT_SHADOWED: &[u8] = include_bytes!("../../shaders/forward.fragmentMainShadowed.spv");
const FRAGMENT_MASKED_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMaskedShadowed.spv");
const FRAGMENT_MOTION_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMainMotionShadowed.spv");
const FRAGMENT_MASKED_MOTION_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMaskedMotionShadowed.spv");
const FRAGMENT_BLENDED_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentBlendedShadowed.spv");
const FRAGMENT_BLENDED_REACTIVE_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentBlendedReactiveShadowed.spv");
const FRAGMENT_BLENDED_ATMOSPHERE_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentBlendedAtmosphereShadowed.spv");
const FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentBlendedAtmosphereReactiveShadowed.spv");
const FRAGMENT_AO_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMainAoShadowed.spv");
const FRAGMENT_SUBSURFACE_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentSubsurfaceShadowed.spv");
// The sun's contact shadows (ADR 0071): shading after the prepass that reads their mask.
const FRAGMENT_CONTACT: &[u8] = include_bytes!("../../shaders/forward.fragmentMainContact.spv");
const FRAGMENT_AO_CONTACT: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMainAoContact.spv");
const FRAGMENT_CONTACT_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMainContactShadowed.spv");
const FRAGMENT_AO_CONTACT_SHADOWED: &[u8] =
    include_bytes!("../../shaders/forward.fragmentMainAoContactShadowed.spv");

mod bent;
mod cascade_cache;
mod declare;
mod layout;
mod material_types;
mod msaa;
mod pipelines;
mod shadow_atlas;
mod vsm_shading;

/// A material type's parameters' layout ([`ForwardRenderer::material_params`]).
pub use engine_shader::StructLayout;
use layout::{Changes, Layout, MESHLET_ARGS, MESHLET_HEADER_BYTES, Source, local_box};
pub(crate) use layout::{States, TypedPipelines, faces, mirrored, state};
pub use material_types::{
    ENGINE_SHADER_DIR, MaterialTypeDesc, RELOAD_QUIET, material_type_options,
    reflect_material_params,
};
use pipelines::{
    AoPasses, ContactState, DecalShading, GiFeatures, GiShading, ReflectShadingTable,
    ReflectionShading, Shading, ShadowedPipelines, SubsurfacePipelines, VARIANTS, lazy_statistics,
    ready, shading_desc, shadowed_index,
};
use shadow_atlas::{AtlasImage, LocalShadowState};
use vsm_shading::VsmShading;

use cascade_cache::whole_plans;
pub use cascade_cache::{CascadeCache, CascadeCacheStats};

pub use crate::contact::ContactShadowSettings;
pub use crate::decals::Decal;
pub use crate::lights::{LocalLight, Spot};
pub use crate::local_shadows::{LocalShadowSettings, LocalShadowStats};
pub use crate::reflections::Reflections;

// Reflections (ADR 0073): the shading of views with them (always after the depth prepass,
// which writes the motion vectors), by `(ambient occlusion × 2 + contact shadows) × 2 +
// local shadows`.
const REFLECT_FRAGMENTS: [(&[u8], &core::ffi::CStr); 8] = [
    (
        include_bytes!("../../shaders/forward.fragmentReflect.spv"),
        c"fragmentReflect",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectShadowed.spv"),
        c"fragmentReflectShadowed",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectContact.spv"),
        c"fragmentReflectContact",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectContactShadowed.spv"),
        c"fragmentReflectContactShadowed",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAo.spv"),
        c"fragmentReflectAo",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAoShadowed.spv"),
        c"fragmentReflectAoShadowed",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAoContact.spv"),
        c"fragmentReflectAoContact",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAoContactShadowed.spv"),
        c"fragmentReflectAoContactShadowed",
    ),
];
// Decals (ADR 0082): the shading after the depth prepass of views with decals, by
// `(ambient occlusion × 2 + contact shadows) × 2 + local shadows`, without and with
// reflections.
const DECAL_FRAGMENTS: [(&[u8], &core::ffi::CStr); 8] = [
    (
        include_bytes!("../../shaders/forward.fragmentMainDecals.spv"),
        c"fragmentMainDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentMainShadowedDecals.spv"),
        c"fragmentMainShadowedDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentMainContactDecals.spv"),
        c"fragmentMainContactDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentMainContactShadowedDecals.spv"),
        c"fragmentMainContactShadowedDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentMainAoDecals.spv"),
        c"fragmentMainAoDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentMainAoShadowedDecals.spv"),
        c"fragmentMainAoShadowedDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentMainAoContactDecals.spv"),
        c"fragmentMainAoContactDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentMainAoContactShadowedDecals.spv"),
        c"fragmentMainAoContactShadowedDecals",
    ),
];
const REFLECT_DECAL_FRAGMENTS: [(&[u8], &core::ffi::CStr); 8] = [
    (
        include_bytes!("../../shaders/forward.fragmentReflectDecals.spv"),
        c"fragmentReflectDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectShadowedDecals.spv"),
        c"fragmentReflectShadowedDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectContactDecals.spv"),
        c"fragmentReflectContactDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectContactShadowedDecals.spv"),
        c"fragmentReflectContactShadowedDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAoDecals.spv"),
        c"fragmentReflectAoDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAoShadowedDecals.spv"),
        c"fragmentReflectAoShadowedDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAoContactDecals.spv"),
        c"fragmentReflectAoContactDecals",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentReflectAoContactShadowedDecals.spv"),
        c"fragmentReflectAoContactShadowedDecals",
    ),
];
// Probe-volume GI (ADR 0088): the shading after the depth prepass of views with a probe
// volume, by decals, then `(ambient occlusion × 2 + contact shadows) × 2 + local shadows`;
// the same writing reflections' inputs; subsurface materials without and with local
// shadows.
const GI_FRAGMENTS: [[(&[u8], &core::ffi::CStr); 8]; 2] = [
    [
        (
            include_bytes!("../../shaders/forward.fragmentMainGi.spv"),
            c"fragmentMainGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainShadowedGi.spv"),
            c"fragmentMainShadowedGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainContactGi.spv"),
            c"fragmentMainContactGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainContactShadowedGi.spv"),
            c"fragmentMainContactShadowedGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoGi.spv"),
            c"fragmentMainAoGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoShadowedGi.spv"),
            c"fragmentMainAoShadowedGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoContactGi.spv"),
            c"fragmentMainAoContactGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoContactShadowedGi.spv"),
            c"fragmentMainAoContactShadowedGi",
        ),
    ],
    [
        (
            include_bytes!("../../shaders/forward.fragmentMainDecalsGi.spv"),
            c"fragmentMainDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainShadowedDecalsGi.spv"),
            c"fragmentMainShadowedDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainContactDecalsGi.spv"),
            c"fragmentMainContactDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainContactShadowedDecalsGi.spv"),
            c"fragmentMainContactShadowedDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoDecalsGi.spv"),
            c"fragmentMainAoDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoShadowedDecalsGi.spv"),
            c"fragmentMainAoShadowedDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoContactDecalsGi.spv"),
            c"fragmentMainAoContactDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentMainAoContactShadowedDecalsGi.spv"),
            c"fragmentMainAoContactShadowedDecalsGi",
        ),
    ],
];
const GI_REFLECT_FRAGMENTS: [[(&[u8], &core::ffi::CStr); 8]; 2] = [
    [
        (
            include_bytes!("../../shaders/forward.fragmentReflectGi.spv"),
            c"fragmentReflectGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectShadowedGi.spv"),
            c"fragmentReflectShadowedGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectContactGi.spv"),
            c"fragmentReflectContactGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectContactShadowedGi.spv"),
            c"fragmentReflectContactShadowedGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoGi.spv"),
            c"fragmentReflectAoGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoShadowedGi.spv"),
            c"fragmentReflectAoShadowedGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoContactGi.spv"),
            c"fragmentReflectAoContactGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoContactShadowedGi.spv"),
            c"fragmentReflectAoContactShadowedGi",
        ),
    ],
    [
        (
            include_bytes!("../../shaders/forward.fragmentReflectDecalsGi.spv"),
            c"fragmentReflectDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectShadowedDecalsGi.spv"),
            c"fragmentReflectShadowedDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectContactDecalsGi.spv"),
            c"fragmentReflectContactDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectContactShadowedDecalsGi.spv"),
            c"fragmentReflectContactShadowedDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoDecalsGi.spv"),
            c"fragmentReflectAoDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoShadowedDecalsGi.spv"),
            c"fragmentReflectAoShadowedDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoContactDecalsGi.spv"),
            c"fragmentReflectAoContactDecalsGi",
        ),
        (
            include_bytes!("../../shaders/forward.fragmentReflectAoContactShadowedDecalsGi.spv"),
            c"fragmentReflectAoContactShadowedDecalsGi",
        ),
    ],
];
const GI_SUBSURFACE_FRAGMENTS: [(&[u8], &core::ffi::CStr); 2] = [
    (
        include_bytes!("../../shaders/forward.fragmentSubsurfaceGi.spv"),
        c"fragmentSubsurfaceGi",
    ),
    (
        include_bytes!("../../shaders/forward.fragmentSubsurfaceShadowedGi.spv"),
        c"fragmentSubsurfaceShadowedGi",
    ),
];
pub use crate::shadow::ShadowSettings;

/// Format of the motion vector target (ADR 0043).
pub const MOTION_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;
pub(crate) const DEPTH_VERTEX_MASKED: &[u8] =
    include_bytes!("../../shaders/forward.depthVertexMasked.spv");
/// The depth prepass that also writes motion vectors: opaque, masked (queue item 0d).
pub(crate) const DEPTH_VERTEX_MOTION: &[u8] =
    include_bytes!("../../shaders/forward.depthVertexMotion.spv");
pub(crate) const PREPASS_MOTION: &[u8] = include_bytes!("../../shaders/forward.prepassMotion.spv");
pub(crate) const DEPTH_VERTEX_MASKED_MOTION: &[u8] =
    include_bytes!("../../shaders/forward.depthVertexMaskedMotion.spv");
const PREPASS_MASKED_MOTION: &[u8] =
    include_bytes!("../../shaders/forward.prepassMaskedMotion.spv");
const CULL: &[u8] = include_bytes!("../../shaders/forward.cullMain.spv");
const CULL_MESHLETS: &[u8] = include_bytes!("../../shaders/forward.cullMainMeshlets.spv");
// The sun's cascades' cull (ADR 0093): their own entry points, so the view's keep their
// registers.
const CULL_SHADOWS: &[u8] = include_bytes!("../../shaders/forward.cullShadows.spv");
const CULL_SHADOWS_MESHLETS: &[u8] =
    include_bytes!("../../shaders/forward.cullShadowsMeshlets.spv");
const MESHLET_CULL: &[u8] = include_bytes!("../../shaders/forward.cullMeshlets.spv");
// The sun's virtual shadow pages (ADR 0093 step 3b): their culls and their casters' draws.
const CULL_VSM: &[u8] = include_bytes!("../../shaders/forward.cullVsm.spv");
const CULL_VSM_MESHLETS: &[u8] = include_bytes!("../../shaders/forward.cullVsmMeshlets.spv");
const VSM_VERTEX: &[u8] = include_bytes!("../../shaders/forward.vsmVertex.spv");
const VSM_VERTEX_MASKED: &[u8] = include_bytes!("../../shaders/forward.vsmVertexMasked.spv");
const VSM_FRAGMENT: &[u8] = include_bytes!("../../shaders/forward.vsmFragment.spv");
const VSM_FRAGMENT_MASKED: &[u8] = include_bytes!("../../shaders/forward.vsmFragmentMasked.spv");
const MESHLET_PIECES_CULL: &[u8] = include_bytes!("../../shaders/forward.cullMeshletPieces.spv");
const FRAGMENT_BLENDED: &[u8] = include_bytes!("../../shaders/forward.fragmentBlended.spv");
const FRAGMENT_BLENDED_REACTIVE: &[u8] =
    include_bytes!("../../shaders/forward.fragmentBlendedReactive.spv");
const FRAGMENT_BLENDED_ATMOSPHERE: &[u8] =
    include_bytes!("../../shaders/forward.fragmentBlendedAtmosphere.spv");
const FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE: &[u8] =
    include_bytes!("../../shaders/forward.fragmentBlendedAtmosphereReactive.spv");

/// Format of the reactive mask the transparent pass writes for temporal upsampling: how much
/// of each pixel transparent surfaces cover (their combined alpha).
pub const REACTIVE_FORMAT: vk::Format = vk::Format::R8_UNORM;
const LIGHT_TILES: &[u8] = include_bytes!("../../shaders/forward.lightTiles.spv");

/// `PHASE_SHADOW_MESHLETS` in `forward.slang`: the meshlet cull of shadow cascade k runs
/// with this phase plus k.
const PHASE_SHADOW_MESHLETS: u32 = 16;
/// `PHASE_VSM` in `forward.slang`: the virtual shadow pages' culls.
const PHASE_VSM: u32 = 24;
/// `VSM_LIST` in `forward.slang`: the virtual shadow pages' meshlet lists.
const VSM_LIST: usize = 6;
/// `SUN_IN_MASK` in `forward.slang`: in the `phase` of the shading that reads the sun's
/// mask, the view's virtual shadow pages put the sun's whole visibility in it.
const SUN_IN_MASK: u32 = 1 << 31;

/// `PHASE_*` in `forward.slang`.
const PHASE_ALL: u32 = 0;
const PHASE_EARLY: u32 = 1;
const PHASE_LATE: u32 = 2;

/// Pipeline states of batches, drawn in this order: bit 4 subsurface scattering, bit 3
/// blended, bit 2 masked, bit 1 double-sided, bit 0 mirrored (negative determinant, so
/// front faces wind clockwise on screen).
const STATES: usize = 32;
/// Subsurface scattering (ADR 0059): shaded by the forward subsurface pass, after the
/// depth prepass. Never blended.
pub(crate) const SUBSURFACE: usize = 16;
/// Blended surfaces: drawn by the transparent pass only, one batch per instance, back to
/// front (ADR 0054).
pub(crate) const BLENDED: usize = 8;
pub(crate) const MASKED: usize = 4;
pub(crate) const DOUBLE_SIDED: usize = 2;
pub(crate) const MIRRORED: usize = 1;
/// Bitmasks of states for [`States::draw`]: opaque and masked without subsurface
/// scattering (0-7), blended (8-15), with subsurface scattering (16-23).
const PLAIN_STATES: u32 = 0x0000_00FF;
pub(crate) const BLENDED_STATES: u32 = 0x0000_FF00;
const SUBSURFACE_STATES: u32 = 0x00FF_0000;
/// Everything that writes depth and casts shadows.
pub(crate) const DEPTH_STATES: u32 = PLAIN_STATES | SUBSURFACE_STATES;

/// One object to draw.
///
/// Consecutive items with the same mesh and the same kind of material (opaque or masked,
/// single- or double-sided) are drawn as one instanced batch, so keep the draws of a mesh
/// together; a mirrored transform also starts a batch of its own. With occlusion culling,
/// an item is known by its index in the list, so also keep the order stable from frame to
/// frame. Items of removed meshes draw nothing; items of removed materials draw with the
/// default material.
#[derive(Debug, Clone, Copy)]
pub struct DrawItem {
    /// Its mesh in the geometry pool.
    pub mesh: MeshId,
    /// Its material in the material table.
    pub material: MaterialId,
    /// Object to world transform (affine: the bottom row is ignored). A negative
    /// determinant mirrors the object; its faces keep their orientation (glTF).
    pub world_from_object: Mat4,
}

/// The directional light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectionalLight {
    /// Direction the light travels (from the light into the scene).
    pub direction: Vec3,
    /// Linear color × intensity.
    pub color: Vec3,
    /// Constant ambient term (linear), multiplied by the base color.
    pub ambient: Vec3,
}

/// The camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Clip from world (projection × view).
    pub clip_from_world: Mat4,
    /// World-space position (for specular).
    pub position: Vec3,
}

/// A perspective camera's view and projection, recovered from its `clip_from_world`, for
/// screen-space effects (ADR 0058). View space is right-handed, looking down −z; the
/// projection maps it as `clip = (p00·x + p20·z, p11·y + p21·z, p22·z + p32, −z)` (`p20`
/// and `p21` hold an off-center projection or the TAA jitter).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewSpace {
    /// View from world: the camera's rotation and position.
    pub view_from_world: Mat4,
    /// Horizontal scale.
    pub p00: f32,
    /// Vertical scale (negative for Vulkan's downward y).
    pub p11: f32,
    /// Horizontal offset.
    pub p20: f32,
    /// Vertical offset.
    pub p21: f32,
    /// Depth terms: `depth = p22 + p32 / z`.
    pub p22: f32,
    /// See `p22`.
    pub p32: f32,
}

impl ViewSpace {
    /// Distance along the view direction of a depth-buffer value.
    pub fn linear_depth(&self, depth: f32) -> f32 {
        self.p32 / (depth + self.p22)
    }
}

impl Camera {
    /// The view and projection of a perspective camera whose view is rigid (rotation and
    /// translation), or `None` (orthographic, scaled or skewed).
    pub fn view_space(&self) -> Option<ViewSpace> {
        let m = self.clip_from_world;
        let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
        // clip.w = −z: the fourth row is minus the view's third row, a unit vector for a
        // rigid view (zero for an orthographic projection).
        let back = -r3.truncate();
        let tz = -r3.w;
        if (back.length() - 1.0).abs() > 1e-3 {
            return None;
        }
        let p22 = r2.truncate().dot(back);
        let p32 = r2.w - p22 * tz;
        let p20 = r0.truncate().dot(back);
        let right = r0.truncate() - p20 * back;
        let p00 = right.length();
        let p21 = r1.truncate().dot(back);
        let up = r1.truncate() - p21 * back;
        let mut p11 = up.length();
        if p00 < 1e-6 || p11 < 1e-6 {
            return None;
        }
        let right = right / p00;
        let mut up = up / p11;
        // Right-handed: right × up = back; Vulkan's projection flips y.
        if right.cross(up).dot(back) < 0.0 {
            up = -up;
            p11 = -p11;
        }
        if right.dot(up).abs() > 1e-3 {
            return None;
        }
        let tx = (r0.w - p20 * tz) / p00;
        let ty = (r1.w - p21 * tz) / p11;
        let view_from_world = Mat4::from_cols(
            Vec4::new(right.x, up.x, back.x, 0.0),
            Vec4::new(right.y, up.y, back.y, 0.0),
            Vec4::new(right.z, up.z, back.z, 0.0),
            Vec4::new(tx, ty, tz, 1.0),
        );
        Some(ViewSpace {
            view_from_world,
            p00,
            p11,
            p20,
            p21,
            p22,
            p32,
        })
    }
}

/// What one [`ForwardRenderer::add_passes`] call renders, and where.
#[derive(Debug, Clone, Copy)]
pub struct ForwardView<'a> {
    /// The camera.
    pub camera: Camera,
    /// The light.
    pub light: DirectionalLight,
    /// Color target.
    pub color: Texture,
    /// Depth target, the size of `color`.
    pub depth: Texture,
    /// Clear color, or `None` to draw over the target's contents.
    pub clear: Option<[f32; 4]>,
    /// This view's occlusion-culling state, or `None` for frustum culling only.
    pub occlusion: Option<&'a Occlusion>,
    /// This view's temporal state, for a jittered projection and motion vectors (TAA,
    /// upscalers, motion blur), or `None`.
    pub temporal: Option<&'a Temporal>,
    /// This view's sun shadows kept across frames (ADR 0093): each frame then draws only
    /// what changed. `None` draws every cascade whole every frame.
    pub shadow_cache: Option<&'a CascadeCache>,
    /// This view's virtual shadow pages for the sun (ADR 0093 step 3): opaque surfaces take
    /// the sun's shadow from them, at about a texel a pixel, instead of the cascades. They
    /// need [`ForwardRenderer::set_virtual_shadows`] and the depth prepass (which then
    /// runs); transparent surfaces and subsurface transmission keep the cascades.
    pub virtual_shadows: Option<&'a crate::vsm::VirtualShadows>,
    /// Point and spot lights (ADR 0045). Only those reaching the view are shaded, each by
    /// the pixels it can reach.
    pub lights: &'a [LocalLight],
    /// Light from the whole sky, and the sky as background (ADR 0046); `None` for the flat
    /// ambient color of [`DirectionalLight::ambient`] over the clear color.
    pub environment: Option<&'a Environment>,
    /// Local reflections: probes and screen-space reflections (ADR 0073;
    /// [`Reflections::NONE`] for the sky's alone). They need an environment and
    /// [`ForwardRenderer::set_reflections`]; perspective views only.
    pub reflections: Reflections<'a>,
    /// Decals (ADR 0082, [`crate::decals`]), drawn over the opaque surfaces through a
    /// decal buffer after the depth prepass (which then runs). They need
    /// [`ForwardRenderer::set_decals`]; not drawn in multisampled views or probe captures,
    /// nor on transparent surfaces and subsurface materials.
    pub decals: &'a [Decal],
    /// Global illumination: the diffuse light of opaque surfaces and subsurface materials,
    /// from a volume of probes (ADR 0088, [`crate::gi`]) or computed per pixel by another
    /// crate (ADR 0090), after the depth prepass (which then runs). It needs
    /// [`ForwardRenderer::set_global_illumination`]; not in multisampled views;
    /// transparent surfaces keep the sky's light.
    pub gi: Option<ViewGi<'a>>,
    /// The renderer's features this view draws with ([`ViewFeatures::ALL`]: every one the
    /// renderer has on).
    pub features: ViewFeatures,
}

/// Which of the renderer's features a view draws with (ADR 0099 step 4). A feature runs in
/// a view when the renderer has it on (its `ForwardRenderer::set_*`) and the view wants
/// it: the views of one frame (split screen, the editor's viewports) can differ. A view
/// without a feature costs as if the renderer had it off: its passes are not declared
/// and its shading variants are not drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewFeatures {
    /// Ambient occlusion ([`ForwardRenderer::set_ambient_occlusion`]).
    pub ambient_occlusion: bool,
    /// The sun's contact shadows ([`ForwardRenderer::set_contact_shadows`]).
    pub contact_shadows: bool,
    /// Local lights' shadows ([`ForwardRenderer::set_local_shadows`]): the shadow views
    /// this view plans and draws, and its shading with them.
    pub local_shadows: bool,
    /// Subsurface scattering ([`ForwardRenderer::set_subsurface`]); without it subsurface
    /// materials shade as the renderer shades them with subsurface scattering off.
    pub subsurface: bool,
    /// Probe and screen-space reflections ([`ForwardRenderer::set_reflections`]).
    pub reflections: bool,
    /// Decals ([`ForwardRenderer::set_decals`]).
    pub decals: bool,
    /// Global illumination ([`ForwardRenderer::set_global_illumination`]).
    pub global_illumination: bool,
    /// The depth prepass where no other feature needs it
    /// ([`ForwardRenderer::set_depth_prepass`]).
    pub depth_prepass: bool,
}

impl ViewFeatures {
    /// Every feature the renderer has on.
    pub const ALL: Self = Self {
        ambient_occlusion: true,
        contact_shadows: true,
        local_shadows: true,
        subsurface: true,
        reflections: true,
        decals: true,
        global_illumination: true,
        depth_prepass: true,
    };

    /// None: the plain forward shading (lights, the sun and its cascades, the sky).
    pub const NONE: Self = Self {
        ambient_occlusion: false,
        contact_shadows: false,
        local_shadows: false,
        subsurface: false,
        reflections: false,
        decals: false,
        global_illumination: false,
        depth_prepass: false,
    };
}

impl Default for ViewFeatures {
    fn default() -> Self {
        Self::ALL
    }
}

/// A view's global illumination ([`ForwardView::gi`]).
#[derive(Debug, Clone, Copy)]
pub enum ViewGi<'a> {
    /// A volume of probes (ADR 0088).
    Probes {
        /// The volume in this frame's graph ([`crate::gi::ProbeGi::begin_frame`], once per
        /// frame).
        volume: GiFrame<'a>,
        /// This frame's scene for ray queries ([`crate::rays::RayScene::add_passes`]):
        /// while the volume converges, its updates trace it (on the persistent GPU
        /// scene's path, [`ForwardRenderer::add_scene_passes`]); `None` keeps the volume
        /// as it is.
        rays: Option<RayFrame<'a>>,
    },
    /// Light computed per pixel by another crate after the depth prepass (ADR 0090: AMD's
    /// Brixelizer GI, `engine_brixelizer`). Not in reflection probes' captures, which then
    /// see the sky's light.
    Screen(&'a dyn ScreenGi),
}

/// Diffuse global illumination computed per pixel after the depth prepass (ADR 0090), for
/// [`ViewGi::Screen`]: the GI shading (ADR 0088) reads what it returns instead of a probe
/// volume's.
pub trait ScreenGi: core::fmt::Debug {
    /// Declares the passes that make the view's diffuse light: an `RGBA16_SFLOAT` texture
    /// the size of `input.depth`, the irradiance in rgb and the distance from the camera
    /// to the pixel's surface in alpha (0 for the sky). `None` leaves the view with the
    /// sky's light this frame.
    fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        input: &ScreenGiInput,
    ) -> Option<Texture>;
}

/// What a [`ScreenGi`] gets from the view.
#[derive(Debug, Clone, Copy)]
pub struct ScreenGiInput {
    /// The depth after the prepass, single-sampled (0 at the near plane, 1 at the far
    /// plane and the sky).
    pub depth: Texture,
    /// Motion vectors ([`MOTION_FORMAT`]: texture coordinates now minus last frame's), in
    /// temporal views.
    pub motion: Option<Texture>,
    /// The camera, without the jitter.
    pub camera: Camera,
    /// Last frame's clip from world, without the jitter (this frame's after a reset).
    pub previous_clip_from_world: Mat4,
    /// The sky's radiance cube and its intensity, when the view has an environment.
    pub environment: Option<(Texture, f32)>,
    /// The view's history was reset (a camera cut).
    pub reset: bool,
}

/// Temporal state of one view, kept across frames (ADR 0043): last frame's camera and
/// instance transforms, from which the depth prepass (or, in views without one, the
/// forward shading) writes motion vectors, and the sub-pixel jitter sequence of the
/// projection.
///
/// Instances are known by their index in the draw list, as for [`Occlusion`]: keep the
/// order stable. An instance whose index had no transform last frame counts as still for
/// one frame. Call [`Self::reset`] after a camera cut. Transforms must be the ones drawn
/// in consecutive frames (interpolate fixed-step physics for rendering): motion vectors
/// report what was on screen.
#[derive(Debug)]
pub struct Temporal {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    capacity: u32,
    /// Instances copied into `buffer` last frame.
    previous_count: Cell<u32>,
    previous_clip_from_world: Cell<Option<Mat4>>,
    /// Frames drawn, for the jitter phase.
    frame: Cell<u64>,
    /// Jitter phases (0: no jitter).
    phases: Cell<u32>,
    /// A fixed jitter sequence instead of Halton's (SMAA T2x's two positions); empty for
    /// Halton.
    pattern: Cell<&'static [[f32; 2]]>,
    reset: Cell<bool>,
    /// The state the graph left the buffer in.
    state: Cell<BufferState>,
}

/// What a frame drawn with a [`Temporal`] state adds to [`ForwardPasses`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionOutput {
    /// Motion vectors ([`MOTION_FORMAT`], the size of the color target): for each pixel,
    /// the texture-coordinate delta current − previous of the surface it shows, without
    /// jitter, so that surface was at `uv − motion` last frame. Pixels without geometry
    /// carry the camera's motion.
    pub velocity: Texture,
    /// This frame's jitter in pixels, in [-0.5, 0.5]: the projection moved the image by
    /// this much (x right, y down).
    pub jitter: [f32; 2],
    /// Last frame is unrelated to this one (first frame, or after [`Temporal::reset`]):
    /// accumulated history should be dropped.
    pub reset: bool,
    /// How much of each pixel transparent surfaces cover ([`REACTIVE_FORMAT`]), when there
    /// were any: their pixels do not move with the motion vectors, so a temporal resolve
    /// should trust its history less there.
    pub reactive: Option<Texture>,
    /// The camera this frame was drawn with, jitter included: passes that must match the
    /// forward pass's depth exactly (the skinned motion pass, `crate::skinning`)
    /// project with it.
    pub clip_from_world: Mat4,
    /// Last frame's camera, without jitter (where the motion vectors point back to).
    pub previous_clip_from_world: Mat4,
}

/// The `index`-th element (from 1) of the van der Corput sequence in `base`.
fn halton(mut index: u32, base: u32) -> f32 {
    let (mut result, mut f) = (0.0f32, 1.0f32);
    while index > 0 {
        f /= base as f32;
        result += f * (index % base) as f32;
        index /= base;
    }
    result
}

impl Temporal {
    /// State for up to `capacity` instances (64 bytes each, in device memory), without
    /// jitter.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        capacity: u32,
    ) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(u64::from(capacity.max(1)) * size_of::<InstanceData>() as u64)
                .usage(
                    vk::BufferUsageFlags::STORAGE_BUFFER
                        | vk::BufferUsageFlags::TRANSFER_DST
                        | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
                ),
            MemoryLocation::Device,
        )?;
        // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
        let address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        Ok(Self {
            buffer,
            allocation: Some(allocation),
            address,
            capacity,
            previous_count: Cell::new(0),
            previous_clip_from_world: Cell::new(None),
            frame: Cell::new(0),
            phases: Cell::new(0),
            pattern: Cell::new(&[]),
            reset: Cell::new(true),
            state: Cell::new(BufferState::idle()),
        })
    }

    /// Jitters the projection through `phases` Halton(2, 3) offsets (0: no jitter). TAA
    /// wants at least 8, upscaling by `s` per axis ⌈8 · s²⌉.
    pub fn set_jitter_phases(&self, phases: u32) {
        self.phases.set(phases);
    }

    /// Jitters the projection through `pattern` (pixels, x right, y down) instead of the
    /// Halton phases (SMAA T2x's two positions, ADR 0075); empty returns to Halton.
    pub fn set_jitter_pattern(&self, pattern: &'static [[f32; 2]]) {
        self.pattern.set(pattern);
    }

    /// The jitter of the next frame, in pixels.
    pub fn next_jitter(&self) -> [f32; 2] {
        let pattern = self.pattern.get();
        if !pattern.is_empty() {
            return pattern[(self.frame.get() % pattern.len() as u64) as usize];
        }
        let phases = self.phases.get();
        if phases == 0 {
            return [0.0; 2];
        }
        let k = (self.frame.get() % u64::from(phases)) as u32 + 1;
        [halton(k, 2) - 0.5, halton(k, 3) - 0.5]
    }

    /// Forgets last frame (camera cut, teleport): the next frame has no motion and
    /// reports [`MotionOutput::reset`].
    pub fn reset(&self) {
        self.reset.set(true);
        self.previous_count.set(0);
        self.previous_clip_from_world.set(None);
    }

    /// Instances whose transforms are remembered.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Frees the buffer.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// Occlusion-culling state of one view, kept across frames (ADR 0035): whether each
/// instance was visible in the view's previous frame.
///
/// Instances are known by their index in the draw list, so keep its order stable. A
/// changed order, a camera cut or more instances than [`Self::capacity`] only cost time
/// (more instances wait for the late phase, or are drawn early for nothing); what is
/// visible is always drawn. Use one state per view, and call [`Self::reset`] after a cut.
#[derive(Debug)]
pub struct Occlusion {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    capacity: u32,
    /// The flags must be cleared before their next use.
    stale: Cell<bool>,
}

impl Occlusion {
    /// State for up to `capacity` instances (4 bytes each, in device memory).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        capacity: u32,
    ) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(u64::from(capacity.max(1)) * 4)
                .usage(
                    vk::BufferUsageFlags::STORAGE_BUFFER
                        | vk::BufferUsageFlags::TRANSFER_DST
                        | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
                ),
            MemoryLocation::Device,
        )?;
        // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
        let address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        Ok(Self {
            buffer,
            allocation: Some(allocation),
            address,
            capacity,
            stale: Cell::new(true),
        })
    }

    /// Instances whose visibility is remembered.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Forgets which instances were visible (after a camera cut or a new scene): the next
    /// frame tests every instance in its late phase.
    pub fn reset(&self) {
        self.stale.set(true);
    }

    /// Frees the buffer.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// `Cascade` in `forward.slang` (144 bytes).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct CascadeData {
    scale: [f32; 4],
    offset: [f32; 4],
    /// Cascade NDC to the uv of its layer (where its window starts there, ADR 0093).
    atlas: [f32; 4],
    /// Light view rectangles (x0, y0, x1, y1) drawn this frame; an empty one has
    /// x0 > x1.
    drawn: [[f32; 4]; crate::shadow_cache::MAX_RECTS],
    /// Light view far depth of its projection.
    far: f32,
    /// World-space error its casters' detail levels may have (0: full meshes).
    lod_error: f32,
    /// Drawn in several pieces at once (a view's cache, ADR 0093): the light view x past
    /// which columns are past its layer's seam, the y below which rows are; the pieces
    /// drawn (bit `r * 4 + q`, [`crate::shadow_cache::Plan::piece_slots`]); and where its
    /// meshlet draws' pieces are in the caster list, from their slots.
    seam: [f32; 2],
    pieces: u32,
    masks: u32,
    /// Rectangles in `drawn` (first).
    rects: u32,
    _pad: u32,
}

/// `Frame` in `forward.slang` (1520 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct FrameData {
    clip_from_world: [f32; 16],
    light_from_world: [f32; 16],
    camera_position: [f32; 4],
    to_light: [f32; 4],
    light_color: [f32; 4],
    ambient: [f32; 4],
    planes: [[f32; 4]; 6],
    shadow_sampler: u32,
    cull: u32,
    visibility_capacity: u32,
    hiz_levels: u32,
    depth_size: [u32; 2],
    /// Pixels per object-space unit at distance 1, over the LOD threshold (0: always
    /// the full meshes).
    lod_scale: f32,
    /// Instance slots per command list.
    slot_count: u32,
    hiz_offsets: [u32; hiz::MAX_LEVELS],
    previous_clip_from_world: [f32; 16],
    previous_clip_from_clip: [f32; 16],
    jitter: [f32; 4],
    previous_count: u32,
    /// The subsurface budget (ADR 0059).
    scatter_budget: u32,
    /// Decals (ADR 0082): per 16-pixel tile a bit, set where they reach (0 without).
    decal_tiles: u64,
    cascades: [CascadeData; MAX_CASCADES],
    cascade_count: u32,
    /// Draw commands per list (one per detail level of each batch).
    command_count: u32,
    cascade_blend: f32,
    normal_bias: f32,
    fade_last: u32,
    /// The scatter pass's footprint: pixels per unit, clip w scale and offset.
    scatter_footprint: [f32; 3],
    view_axis: [f32; 4],
    lights: u64,
    zbins: u64,
    light_count: u32,
    light_words: u32,
    tiles_x: u32,
    bin_scale: f32,
    light_culling: u32,
    /// The scatter pass's noise index.
    scatter_frame: u32,
    /// The sky pass: sine and cosine of the sky cube's turn about +Y (an HDR panorama's).
    sky_turn: [f32; 2],
    world_from_clip: [f32; 16],
    environment: u32,
    sky_cube: u32,
    prefiltered: u32,
    dfg_lut: u32,
    env_levels: u32,
    env_intensity: f32,
    linear_sampler: u32,
    /// Subsurface profiles (ADR 0059) at `subsurface`.
    subsurface_count: u32,
    sh: u64,
    subsurface: u64,
    /// The atmosphere's and fog's constants (ADR 0069), for the transparent surfaces.
    atmosphere: u64,
    /// Local lights' shadows (ADR 0071): the atlas's bindless index, ...
    local_shadow_atlas: u32,
    /// The sky pass: the cosine of the light's disk (above 1: none, a panorama's sky).
    sun_disk: f32,
    /// ... per sorted light its first view (or `NO_SHADOW`), the views, the drawn views
    /// and their count, and one texel of the atlas in uv.
    light_shadows: u64,
    shadow_views: u64,
    shadow_draws: u64,
    shadow_draw_count: u32,
    shadow_texel: f32,
    /// Probe GI (ADR 0088): pixels per resolved sample along each axis, and the samples'
    /// grid.
    gi_scale: f32,
    gi_samples: [u32; 2],
    /// The cascades' comparison sampler: repeat (their layers wrap around, ADR 0093).
    cascade_sampler: u32,
    /// Material types (ADR 0092): their parameters, `material_stride` bytes per material
    /// slot.
    material_data: u64,
    material_stride: u32,
    /// 1: cascade `c` is layer `c` of the map (a view's cache, ADR 0093); 0: every cascade
    /// is a tile of layer 0 (the atlas). The shading gets the layer without a load.
    cascade_layers: f32,
    /// The meshlet path (ADR 0085): the batches' meshlets, the pool's index buffer (their
    /// meshlets), and per meshlet lists (0: all or early, 1: late, 2 + k: shadow cascade
    /// k, 6: the virtual shadow pages) the bytes from the command list its culling reads
    /// to them and the first slot of their draws.
    batch_meshlets: u64,
    meshlets: u64,
    meshlet_offset: [u32; 7],
    meshlet_slot: [u32; 7],
    /// The sun's virtual shadow pages (ADR 0093 step 3b, their cull and draw entry
    /// points): their frame (`VsmFrame`), page tables and hierarchy of dirty pages, their
    /// batches (`BatchMeshletData` of every batch), the pool's bindless index, the error
    /// the casters' detail levels may have in texels of the level they are drawn into,
    /// and the frame's allocation requests (the transparent surfaces' marking, step 3d).
    vsm: u64,
    vsm_table: u64,
    vsm_hierarchy: u64,
    vsm_batches: u64,
    vsm_pool: u32,
    vsm_lod_threshold: f32,
    vsm_requests: u64,
    /// To a whole number of 16 bytes, as the shaders read it.
    pad: u64,
}

/// `BatchMeshlets` in `forward.slang` (48 bytes): a batch's levels' meshlets (ADR 0085).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct BatchMeshletData {
    /// Per level: its first meshlet, an index of the pool's index buffer read as meshlets.
    first: [u32; 4],
    /// Per level: its meshlets (0: the level is drawn whole).
    count: [u32; 4],
    /// The batch's pipeline state: the list its meshlet draws go to.
    state: u32,
    /// 1: meshlets facing away may be culled (single-sided).
    cone: u32,
    _pad: [u32; 2],
}

/// `Instance` in `forward.slang` (64 bytes): the rows of the affine
/// `world_from_object`, the material's slot and the batch.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub(crate) struct InstanceData {
    pub(crate) rows: [[f32; 4]; 3],
    pub(crate) material: u32,
    pub(crate) batch: u32,
    pub(crate) _pad: [u32; 2],
}

/// `Batch` in `forward.slang` (64 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct BatchData {
    /// The batch's first draw command (its finest level; the others follow).
    command: u32,
    /// The first command's first instance slot (each level has `count` more).
    slot: u32,
    /// Instances.
    count: u32,
    /// Detail levels (bits 0-7); bit 8: blended (the transparent pass's).
    lods: u32,
    center: [f32; 4],
    extents: [f32; 4],
    /// Each level's error in object-space units.
    errors: [f32; 4],
}

/// `Push` in `forward.slang` (120 bytes).
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct Push {
    pub(crate) frame: u64,
    pub(crate) instances: u64,
    pub(crate) batches: u64,
    pub(crate) visible: u64,
    pub(crate) commands: u64,
    pub(crate) visibility: u64,
    pub(crate) pyramid: u64,
    pub(crate) positions: u64,
    pub(crate) attributes: u64,
    pub(crate) materials: u64,
    pub(crate) previous_instances: u64,
    pub(crate) light_tiles: u64,
    pub(crate) instance_count: u32,
    pub(crate) texture: u32,
    pub(crate) phase: u32,
    /// Bindless index of the view's ambient occlusion, or [`NO_AO`].
    pub(crate) ao: u32,
    /// Bindless indices of the decal buffer (ADR 0082): albedo, surface.
    pub(crate) decal_albedo: u32,
    pub(crate) decal_surface: u32,
    /// Bindless index of the view's diffuse irradiance per pixel (ADR 0088), read by the
    /// GI entry points.
    pub(crate) gi: u32,
    pub(crate) _pad: u32,
}

/// `NO_TEXTURE` in `forward.slang`: no ambient occlusion.
const NO_AO: u32 = u32::MAX;

const _: () = assert!(core::mem::size_of::<CascadeData>() == 144);
const _: () = assert!(core::mem::size_of::<FrameData>() == 1536);
const _: () = assert!(core::mem::size_of::<BatchMeshletData>() == 48);
const _: () = assert!(core::mem::size_of::<InstanceData>() == 64);
const _: () = assert!(core::mem::size_of::<BatchData>() == 64);
const _: () = assert!(crate::mesh::MAX_LODS == 4);
const _: () = assert!(core::mem::size_of::<Push>() == 128);
const COMMAND_BYTES: u64 = core::mem::size_of::<vk::DrawIndexedIndirectCommand>() as u64;
const _: () = assert!(COMMAND_BYTES == 20);

/// What [`ForwardRenderer::add_passes`] declared.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForwardPasses {
    /// Instances (one per draw item), all tested by the culling pass.
    pub instances: u32,
    /// Batches: runs of consecutive draws that share a mesh and a pipeline state.
    pub batches: u32,
    /// Draw commands per list: one per detail level of each batch's mesh (equal to
    /// `batches` without LODs), in the order of `commands`.
    pub draw_commands: u32,
    /// Commands per list in `commands`: `draw_commands`, plus one that draws nothing on
    /// the persistent-scene path (its sink of removed instances, ADR 0064).
    pub list_commands: u32,
    /// Whether culling ran in two phases (occlusion culling).
    pub occlusion: bool,
    /// The forward pass's indirect draw commands, one `vk::DrawIndexedIndirectCommand` (20
    /// bytes) per batch, ordered by pipeline state and within a state in draw order
    /// (`list_commands` per list); with occlusion culling, the early phase's, then the late
    /// phase's.
    /// After culling, each `instance_count` is the number of instances its phase drew of
    /// its batch: read it back to count what was drawn. `None` when there was nothing to
    /// draw.
    pub commands: Option<Buffer>,
    /// Motion vectors and jitter, with a [`Temporal`] state.
    pub motion: Option<MotionOutput>,
    /// The view's ambient occlusion ([`crate::ao::AO_FORMAT`], or
    /// [`crate::ao::AO_BENT_FORMAT`] with its bent normals; sampled by the forward pass),
    /// when it is on: for debugging views and effects of your own.
    pub ao: Option<Texture>,
    /// The environment's atmosphere tables this frame, when it has a physical sky (ADR
    /// 0069): for debugging views and tests.
    pub atmosphere: Option<AtmosphereTextures>,
    /// The sun's contact shadows ([`crate::contact::CONTACT_FORMAT`], sampled by the
    /// forward pass), when they are on (ADR 0071): for debugging views and tests.
    pub contact_shadows: Option<Texture>,
    /// The decal buffer (ADR 0082), when decals drew: for debugging views and tests.
    pub decals: Option<crate::decals::DecalTargets>,
    /// The diffuse irradiance per pixel of probe GI (ADR 0088), when the view had it: for
    /// debugging views and tests.
    pub gi: Option<Texture>,
    /// The view's reflection inputs and screen-space reflections (ADR 0073), when it has
    /// reflections: for effects of your own after the scene passes.
    pub reflections: Option<crate::reflections::ReflectionOutput>,
    /// With multisampled targets (MSAA, ADR 0075): the depth resolved to one value per
    /// pixel, the nearest sample's (what ambient occlusion, contact shadows, reflections
    /// and culling read). The color and the motion vectors stay multisampled: the caller
    /// resolves them (`engine_render::msaa`), after its own passes on them.
    pub resolved_depth: Option<Texture>,
    /// What passes drawing the view's visible instances again need (texture feedback,
    /// [`crate::feedback`]).
    pub(crate) draws: Option<DrawLists>,
    /// The meshlet lists in `commands`, when the view drew by meshlet (ADR 0085): read a
    /// phase's first 32 words back to count its meshlet draws.
    pub meshlets: Option<MeshletLists>,
    /// The sun's virtual shadow pages this frame (ADR 0093 step 3), when the view had them:
    /// for debugging views and tests.
    pub virtual_shadows: Option<crate::vsm::VsmFrame>,
}

/// Where a view's meshlet lists are in [`ForwardPasses::commands`] (ADR 0085): one per
/// culling phase, each starting with the meshlet draws it made per pipeline state (32
/// `u32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshletLists {
    /// Bytes into the command buffer of the first phase's lists.
    pub offset: u64,
    /// Bytes of a phase's lists.
    pub bytes: u64,
    /// Room for meshlet draws in a phase.
    pub draws: u32,
}

/// The view's culled draw lists in this frame's graph, for passes that draw the visible
/// instances again ([`crate::feedback::TextureFeedback`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DrawLists {
    /// The forward pass's push constants without the per-pass fields (`visible`,
    /// `texture`, `light_tiles`, `ao`, `phase`).
    pub(crate) push: Push,
    /// The draw commands: `phases` lists of `command_bytes`.
    pub(crate) commands: Buffer,
    pub(crate) visible: Buffer,
    /// The persistent scene's instances, read by the vertex shaders.
    pub(crate) instances: Option<Buffer>,
    pub(crate) phases: u32,
    pub(crate) command_bytes: u64,
    pub(crate) states: States,
    /// The phases' meshlet lists in `commands`, when the view drew by meshlet (ADR 0085).
    pub(crate) meshlets: Option<MeshletDraws>,
}

/// Where a view's meshlet lists are (ADR 0085), for the passes that draw them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MeshletDraws {
    pub(crate) layout: layout::MeshletLayout,
    /// The first phase's lists in the command buffer, and the bytes of each.
    pub(crate) base: u64,
    pub(crate) bytes: u64,
}

impl MeshletDraws {
    /// Where phase `list`'s lists start (0: all or early; 1: late).
    pub(crate) fn lists(&self, list: u32) -> u64 {
        self.base + u64::from(list) * self.bytes
    }
}

/// Which instance-slot region command list `list` uses: the forward phases' lists
/// (`0..phases`) have their own regions, each cascade's shadow list (`phases + k`) has
/// region `k`, and each local light shadow view's (`phases + cascades + j`) region `j`
/// (each group of lists is culled into its own buffer).
fn list_offset(list: u32, phases: u32, cascades: u32) -> u32 {
    if list < phases {
        list
    } else if list < phases + cascades {
        list - phases
    } else {
        list - phases - cascades
    }
}

/// A batch while the draw list is turned into batches.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// First draw.
    first: u32,
    len: u32,
    /// Pipeline state (`MASKED | DOUBLE_SIDED | MIRRORED` bits).
    state: usize,
    /// The material type of its draws (ADR 0092).
    ty: crate::material::MaterialTypeId,
    /// Position among the batches ordered by state.
    order: u32,
    /// First draw command (the finest level) and its first instance slot.
    command: u32,
    slot: u32,
    /// Detail levels of the mesh.
    lods: u32,
}

/// Per-frame working memory, kept to avoid allocating every frame.
#[derive(Debug, Default)]
struct Scratch {
    /// Material slot of every draw.
    slots: Vec<u32>,
    runs: Vec<Run>,
    /// Sort keys of the runs: state, then distance from the camera, then run index.
    keys: Vec<(u64, u32)>,
    /// The draw commands of one list, before they are copied into each list.
    commands: Vec<vk::DrawIndexedIndirectCommand>,
    lights: crate::lights::LightScratch,
}

/// Names of a view's passes: the main view's, or a reflection probe's capture (whose
/// passes then show apart in GPU timings).
#[derive(Debug)]
struct Names {
    cull_setup: &'static str,
    cull: &'static str,
    shadow_cull: &'static str,
    shadow: &'static str,
    light_tiles_clear: &'static str,
    light_tiles: &'static str,
    depth_prepass: &'static str,
    forward: &'static str,
    forward_subsurface: &'static str,
    subsurface_scatter: &'static str,
    transparent: &'static str,
}

const VIEW_NAMES: Names = Names {
    cull_setup: "cull setup",
    cull: "cull",
    shadow_cull: "shadow cull",
    shadow: "shadow",
    light_tiles_clear: "light tiles clear",
    light_tiles: "light tiles",
    depth_prepass: "depth prepass",
    forward: "forward",
    forward_subsurface: "forward subsurface",
    subsurface_scatter: "subsurface scatter",
    transparent: "transparent",
};

const CAPTURE_NAMES: Names = Names {
    cull_setup: "probe cull setup",
    cull: "probe cull",
    shadow_cull: "probe shadow cull",
    shadow: "probe shadow",
    light_tiles_clear: "probe light tiles clear",
    light_tiles: "probe light tiles",
    depth_prepass: "probe depth prepass",
    forward: "probe forward",
    forward_subsurface: "probe forward subsurface",
    subsurface_scatter: "probe subsurface scatter",
    transparent: "probe transparent",
};

/// What a view's passes read that its caller imported once for the frame.
#[derive(Debug, Clone, Copy)]
struct Imported<'g> {
    environment: Option<EnvironmentFrame>,
    probes: Option<(&'g ReflectionProbes, ProbeTextures)>,
}

/// The shadows of a reflection probe's capture (ADR 0073): one cascade around the probe,
/// drawn by its first face this frame and read by the others.
#[derive(Debug)]
struct CaptureShadow {
    fit: CascadeFit,
    map: Cell<Option<Texture>>,
}

/// The view's jitter in pixels, its camera without and with it.
fn jittered(view: &ForwardView<'_>, depth_desc: &TextureDesc) -> ([f32; 2], Mat4, Mat4) {
    let jitter = view.temporal.map_or([0.0; 2], Temporal::next_jitter);
    let jitter_ndc = [
        2.0 * jitter[0] / depth_desc.width.max(1) as f32,
        2.0 * jitter[1] / depth_desc.height.max(1) as f32,
    ];
    let unjittered = view.camera.clip_from_world;
    let clip_from_world =
        Mat4::from_translation(Vec3::new(jitter_ndc[0], jitter_ndc[1], 0.0)) * unjittered;
    (jitter, unjittered, clip_from_world)
}

/// The forward PBR pipelines.
#[derive(Debug)]
pub struct ForwardRenderer {
    /// Shading by variant: bit 0 local lights (`LOCAL_LIGHTS` in `forward.slang`), bit 1
    /// an environment (`ENVIRONMENT`). Specialization constants: code a frame does not
    /// need takes no registers there.
    shading: [Shading; 4],
    /// Depth prepass: opaque (depth only), masked.
    prepass: [Pipeline; 2],
    /// Depth prepass that also writes motion vectors (views with a temporal state):
    /// opaque, masked.
    prepass_motion: [Pipeline; 2],
    /// Camera motion where no geometry was drawn.
    background: Pipeline,
    /// The environment's sky where no geometry was drawn.
    sky: Pipeline,
    /// With an atmosphere or height fog (ADR 0069): the sky, aerial perspective and fog
    /// in one full-screen pass after the opaque surfaces.
    composite: LazyPipeline,
    /// Transparent surfaces under an atmosphere or fog (the environment variants, without
    /// and with local lights): without and with the reactive mask.
    blended_atmosphere: [[LazyPipeline; 2]; 2],
    /// Shadow map: opaque (depth only), masked.
    shadow: [Pipeline; 2],
    /// A cached cascade's pieces drawn at once (ADR 0093): opaque, masked; described where
    /// vertex shaders pick viewports.
    shadow_pieces: Option<[LazyPipeline; 2]>,
    cull: ComputeShader<Push>,
    /// The sun's cascades' cull (ADR 0093).
    shadow_cull: ComputeShader<Push>,
    /// The meshlet path's culling (ADR 0085), made when first turned on;
    /// `meshlets_on` says whether it is on now.
    meshlet_cull: Option<Box<MeshletCull>>,
    meshlets_on: bool,
    /// The sun's virtual shadow pages' culls and draws (ADR 0093 step 3b), made when they
    /// are first turned on.
    vsm: Option<Box<VsmDraw>>,
    vsm_on: bool,
    /// The tiles' light bitmasks.
    light_tiles: ComputeShader<Push>,
    pyramid: DepthPyramid,
    shadow_settings: ShadowSettings,
    frustum_culling: bool,
    light_culling: bool,
    /// Screen-space error (pixels) a coarser detail level may have; 0 draws full meshes.
    lod_threshold: f32,
    depth_prepass: bool,
    /// Views with motion vectors get a reactive mask even without transparent surfaces,
    /// for transparent effects drawn after the scene ([`Self::set_reactive_effects`]).
    reactive_effects: bool,
    color_format: vk::Format,
    depth_format: vk::Format,
    /// Ambient occlusion's passes and shading pipelines, made when it is first turned on.
    ao: Option<AoPasses>,
    ao_settings: Option<AoSettings>,
    /// Frames drawn with ambient occlusion: its noise index under TAA.
    ao_frame: Cell<u32>,
    /// Subsurface materials' pipelines (ADR 0059), once there are profiles.
    subsurface_pipelines: Option<SubsurfacePipelines>,
    subsurface_settings: Option<SubsurfaceSettings>,
    /// The profiles as the shaders read them.
    subsurface_profiles: Vec<GpuProfile>,
    /// Local lights' shadows (ADR 0071), once turned on; `local_shadows_on` says whether
    /// they are on now.
    local_shadows: Option<LocalShadowState>,
    local_shadows_on: bool,
    /// The sun's contact shadows (ADR 0071), made when first turned on.
    contact: Option<ContactState>,
    contact_settings: Option<ContactShadowSettings>,
    /// Atlases replaced by new settings, freed by [`Self::destroy`].
    retired_atlases: Vec<AtlasImage>,
    /// Reflections' passes and shading pipelines (ADR 0073), made when first turned on.
    /// Boxed: 24 pipelines and the passes' would make every renderer large on the stack.
    reflections: Option<Box<ReflectionShading>>,
    reflections_on: bool,
    /// Decals' passes and shading pipelines (ADR 0082), made when first turned on; boxed
    /// as reflections'.
    decals: Option<Box<DecalShading>>,
    decals_on: bool,
    /// Probe-volume GI's shading pipelines (ADR 0088), described when first turned on.
    gi: Option<Box<GiShading>>,
    gi_on: bool,
    /// Pipelines for multisampled targets (MSAA, ADR 0075): 2, 4, 8 samples, made by
    /// [`Self::set_samples`].
    msaa: [Option<Box<msaa::MsaaPipelines>>; 3],
    /// How a multisampled depth is resolved for what reads it by pixel (the nearest
    /// sample), and for culling's pyramid (the farthest: conservative); sample 0 where
    /// the device has no min / max resolve.
    depth_resolves: [ResolveMode; 2],
    scratch: RefCell<Scratch>,
    /// Material types and their pipelines (ADR 0092).
    material_types: material_types::MaterialTypes,
    /// The shading's variants that read ambient occlusion's bent normal (ADR 0058).
    bent_twins: bent::BentTwins,
}

/// The meshlet path's compute shaders (ADR 0085).
struct MeshletCull {
    /// `cullMain` with the meshlet path: instances whose level has meshlets become work
    /// items.
    instances: ComputeShader<Push>,
    /// The cascades' cull with the meshlet path.
    shadow_instances: ComputeShader<Push>,
    /// The meshlet cull: one group per work item.
    meshlets: ComputeShader<Push>,
    /// Its twin for a cached cascade drawn in several pieces at once (ADR 0093), where
    /// vertex shaders pick viewports.
    pieces: Option<ComputeShader<Push>>,
}

/// The sun's virtual shadow pages' culls and casters' draws (ADR 0093 step 3b).
struct VsmDraw {
    /// One thread per instance: the levels whose dirty pages its box reaches.
    cull: ComputeShader<Push>,
    /// One group per work item: the meshlets that reach a dirty page of its level.
    meshlets: ComputeShader<Push>,
    /// The casters, drawn without attachments (an atomic minimum into the pool): plain,
    /// masked.
    pipelines: [Pipeline; 2],
    /// The shading the visibility pass's mask does not serve, and the transparent
    /// surfaces' marking (step 3d).
    shading: VsmShading,
}

impl core::fmt::Debug for VsmDraw {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VsmDraw").finish_non_exhaustive()
    }
}

impl VsmDraw {
    fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let [cull, meshlets] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::<Push>::new(CULL_VSM, c"cullVsm", &[])?,
                ComputeSource::<Push>::new(CULL_VSM_MESHLETS, c"cullVsmMeshlets", &[])?,
            ],
        )?;
        let vertex = spirv_words(VSM_VERTEX).ok_or_else(invalid)?;
        let vertex_masked = spirv_words(VSM_VERTEX_MASKED).ok_or_else(invalid)?;
        let fragment = spirv_words(VSM_FRAGMENT).ok_or_else(invalid)?;
        let fragment_masked = spirv_words(VSM_FRAGMENT_MASKED).ok_or_else(invalid)?;
        let desc = |vertex, v, fragment, f| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                depth_test: false,
                depth_write: false,
                dynamic_culling: true,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: vertex,
                        entry: v,
                    },
                    Some(ShaderStageDesc {
                        spirv: fragment,
                        entry: f,
                    }),
                )
            })
        };
        let mut pipelines = Pipeline::create_many(
            device,
            bindless.pipeline_layout(),
            &[
                desc(&vertex, c"vsmVertex", &fragment, c"vsmFragment"),
                desc(
                    &vertex_masked,
                    c"vsmVertexMasked",
                    &fragment_masked,
                    c"vsmFragmentMasked",
                ),
            ],
        )?
        .into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "virtual shadow pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        Ok(Self {
            cull,
            meshlets,
            pipelines: [next()?, next()?],
            shading: VsmShading::new(device, bindless, color_format, depth_format)?,
        })
    }
}

impl core::fmt::Debug for MeshletCull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MeshletCull").finish_non_exhaustive()
    }
}

/// The slot of a sample count in `ForwardRenderer::msaa`: 2, 4, 8 samples.
fn msaa_slot(samples: vk::SampleCountFlags) -> Option<usize> {
    match samples {
        vk::SampleCountFlags::TYPE_2 => Some(0),
        vk::SampleCountFlags::TYPE_4 => Some(1),
        vk::SampleCountFlags::TYPE_8 => Some(2),
        _ => None,
    }
}

fn push_bytes(push: &Push) -> &[u8] {
    bytemuck::bytes_of(push)
}

impl ForwardRenderer {
    /// Registers a material type (ADR 0092): materials of it
    /// ([`crate::material::MaterialTable::add_typed`]) are drawn with pipelines compiled
    /// for it, made in the background when a frame first draws it (the material's glTF
    /// fallback draws until then). Call [`Self::update_material_types`] every frame. The
    /// same module and type registered again get the same id.
    pub fn register_material_type(&mut self, desc: MaterialTypeDesc) -> MaterialTypeId {
        self.material_types.register(desc)
    }

    /// [`Self::register_material_type`] with the type's entry points compiled ahead (a
    /// cooked pack's, `(name, SPIR-V)`): its pipelines are made from them, no compiler
    /// needed. Entry points it lacks still compile from source when one is drawn.
    pub fn register_material_type_precompiled(
        &mut self,
        desc: MaterialTypeDesc,
        entries: &[(String, Vec<u32>)],
    ) -> MaterialTypeId {
        self.material_types.register_precompiled(desc, entries)
    }

    /// Watches the files material types compile from (their modules, `forward.slang` and
    /// what they include) and compiles a type again when one changes: its pipelines in use
    /// keep drawing until their successors are made; a module that does not compile keeps
    /// the previous version ([`Self::material_type_error`] says why). Off by default; for
    /// development builds and the editor.
    pub fn set_material_hot_reload(&mut self, enabled: bool) {
        self.material_types.set_hot_reload(enabled);
    }

    /// The latest compile error of material type `ty`, if its last compile failed (its
    /// previous version, or its fallback, draws).
    pub fn material_type_error(&self, ty: MaterialTypeId) -> Option<&str> {
        self.material_types.error(ty)
    }

    /// The layout of material type `ty`'s parameters (`<type>.Params`) as its shaders read
    /// them, reflected on the material compiler's thread, never the frame's (ADR 0120's
    /// E10-1b): `None` while it is on its way (asked for the first time here, sent by the
    /// next [`Self::update_material_types`]); then the layout, or why it could not be
    /// reflected, and its revision. A reload of the type's module reflects it again and
    /// moves the revision; the previous layout stands until then.
    pub fn material_params(
        &self,
        ty: MaterialTypeId,
    ) -> Option<(Result<&engine_shader::StructLayout, &str>, u32)> {
        self.material_types.params(ty)
    }

    /// Starts frame `frame` for material types: pipelines a hot reload replaced are
    /// destroyed once the frames up to `completed` are done. Before
    /// [`Self::update_material_types`] (the frame renderer calls both).
    pub fn collect_material_types(&mut self, frame: u64, completed: u64) {
        self.material_types.collect(frame, completed);
    }

    /// A registered type's description.
    pub fn material_type(&self, ty: MaterialTypeId) -> Option<&MaterialTypeDesc> {
        self.material_types.desc(ty)
    }

    /// Where `forward.slang` and its modules are read from to compile material types (by
    /// default the engine's own shader folder).
    pub fn set_material_shader_dir(&mut self, dir: std::path::PathBuf) {
        self.material_types.set_shader_dir(dir);
    }

    /// Hands the material types' requests of the last frames to their compiler and
    /// describes the pipelines whose SPIR-V arrived (created by the device's warm-up
    /// thread). Once per frame, before declaring its passes; costs nothing when no type
    /// waits for anything. Describes the variants that read ambient occlusion's bent normal
    /// the last frames asked for too ([`AoSettings::bent_normals`]): without this call
    /// frames draw without the bent normal.
    pub fn update_material_types(&mut self, device: &Device) {
        self.material_types.update(device);
        self.bent_twins.update(device);
    }

    /// Every material type's entry points frames drew it with so far (sorted): what a PSO
    /// record keeps (`engine_assets::pso_record`, ADR 0092), so that a cook compiles only
    /// those. Playtests save it at exit and merge it with the record on disk.
    pub fn material_type_entries_drawn(&self) -> Vec<(&MaterialTypeDesc, Vec<String>)> {
        self.material_types.drawn()
    }

    /// The entry points frames drew a type registered with precompiled SPIR-V (a pack's)
    /// with that the pack lacks, as (type name, entry point): its PSO record lacked them
    /// (logged; the fallback draws until the entry point compiles, where Slang is). Tools
    /// that check records want none.
    pub fn material_type_pack_misses(&self) -> &[(String, String)] {
        self.material_types.pack_misses()
    }

    /// Material types' pipelines on their way (compiling, or described and not created):
    /// a frame drawing them now draws their fallbacks. Tests and tools wait for 0.
    pub fn material_types_pending(&self) -> usize {
        self.material_types.pending() + self.bent_twins.pending()
    }

    /// Blocks until the material types' pipelines asked for so far are made, or `timeout`
    /// passes (then `false`). For tests and tools: a frame never waits (ADR 0092).
    pub fn wait_for_material_types(
        &mut self,
        device: &Device,
        timeout: std::time::Duration,
    ) -> bool {
        let done = self.material_types.wait(device, timeout);
        self.bent_twins.update(device);
        if self.bent_twins.pending() > 0 {
            device.wait_for_pipeline_warm_up();
        }
        done && self.bent_twins.pending() == 0
    }

    /// The driver's statistics of material type `ty`'s shading (ADR 0092), per shading
    /// variant as [`Self::shading_statistics`]; empty until its pipelines exist. Asks for
    /// them: [`Self::update_material_types`] makes them (tools wait for
    /// [`Self::material_types_pending`] to reach 0, then ask again).
    pub fn material_type_statistics(
        &self,
        device: &Device,
        ty: MaterialTypeId,
    ) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        core::array::from_fn(|k| {
            self.material_types
                .resolve(ty, &self.shading[k].main[0])
                .map(|p| p.statistics(device))
                .unwrap_or_default()
        })
    }

    /// Each typed range's pipelines for a pass whose engine pipelines are `roles`
    /// (`[opaque, masked]`), among the states `mask` draws.
    pub(crate) fn typed_pipelines<'a>(
        &'a self,
        states: &States,
        roles: [&'a LazyPipeline; 2],
        mask: u32,
    ) -> TypedPipelines<'a> {
        self.typed_pipelines_or(states, roles, None, mask)
    }

    /// [`Self::typed_pipelines`], and where a type has no pipeline of `roles` (yet, or its
    /// pack lacks the entry point), its pipeline of `fallback`'s: a feature's variants
    /// (ambient occlusion's bent normals) fall back to the type without the feature, not
    /// to the type's glTF fallback.
    pub(crate) fn typed_pipelines_or<'a>(
        &'a self,
        states: &States,
        roles: [&'a LazyPipeline; 2],
        fallback: Option<[&'a LazyPipeline; 2]>,
        mask: u32,
    ) -> TypedPipelines<'a> {
        let mut out = TypedPipelines::BUILTIN;
        for (i, range) in states.typed.as_slice().iter().enumerate() {
            if mask & (1 << range.state) == 0 {
                continue;
            }
            let k = usize::from(usize::from(range.state) & MASKED != 0);
            let resolved = self
                .material_types
                .resolve(range.ty, roles[k])
                .or_else(|| fallback.and_then(|f| self.material_types.resolve(range.ty, f[k])));
            if let Some(p) = resolved {
                out.ranges[i] = Some([p, p]);
            }
        }
        out
    }

    /// Creates the pipelines for rendering into `color_format` with a `depth_format`
    /// depth buffer.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
        shadow_settings: ShadowSettings,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let shadow_vertex = spirv_words(SHADOW_VERTEX).ok_or_else(invalid)?;
        let shadow_vertex_masked = spirv_words(SHADOW_VERTEX_MASKED).ok_or_else(invalid)?;
        let shadow_vertex_pieces = spirv_words(SHADOW_VERTEX_PIECES).ok_or_else(invalid)?;
        let shadow_vertex_masked_pieces =
            spirv_words(SHADOW_VERTEX_MASKED_PIECES).ok_or_else(invalid)?;
        let shadow_fragment_masked = spirv_words(SHADOW_FRAGMENT_MASKED).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT).ok_or_else(invalid)?;
        let fragment_masked = spirv_words(FRAGMENT_MASKED).ok_or_else(invalid)?;
        let depth_vertex = spirv_words(DEPTH_VERTEX).ok_or_else(invalid)?;
        let depth_vertex_masked = spirv_words(DEPTH_VERTEX_MASKED).ok_or_else(invalid)?;
        let depth_vertex_motion = spirv_words(DEPTH_VERTEX_MOTION).ok_or_else(invalid)?;
        let prepass_motion_fragment = spirv_words(PREPASS_MOTION).ok_or_else(invalid)?;
        let depth_vertex_masked_motion =
            spirv_words(DEPTH_VERTEX_MASKED_MOTION).ok_or_else(invalid)?;
        let prepass_masked_motion = spirv_words(PREPASS_MASKED_MOTION).ok_or_else(invalid)?;
        let fragment_motion = spirv_words(FRAGMENT_MOTION).ok_or_else(invalid)?;
        let fragment_masked_motion = spirv_words(FRAGMENT_MASKED_MOTION).ok_or_else(invalid)?;
        let background_vertex = spirv_words(BACKGROUND_VERTEX).ok_or_else(invalid)?;
        let background_motion = spirv_words(BACKGROUND_MOTION).ok_or_else(invalid)?;
        let sky_fragment = spirv_words(SKY_FRAGMENT).ok_or_else(invalid)?;
        let fragment_blended = spirv_words(FRAGMENT_BLENDED).ok_or_else(invalid)?;
        let fragment_blended_reactive =
            spirv_words(FRAGMENT_BLENDED_REACTIVE).ok_or_else(invalid)?;
        let reactive_formats = [color_format, REACTIVE_FORMAT];
        let formats = [color_format];
        let motion_formats = [color_format, MOTION_FORMAT];
        // Every graphics pipeline is described first and created in parallel below
        // (ADR 0055); they are taken back in the order they were described.
        let main = |fragment, entry, formats, equal: bool, variant: usize| GraphicsPipelineDesc {
            // After the depth prepass: tested for equality, not written.
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
        // The shading variants are described (made on first use, ADR 0079), the rest made.
        let mut lazy_descs = Vec::with_capacity(7 * VARIANTS.len() + 5);
        let mut descs = Vec::with_capacity(8);
        for lit in 0..VARIANTS.len() {
            lazy_descs.extend([
                main(&fragment, c"fragmentMain", &formats, false, lit),
                main(&fragment_masked, c"fragmentMasked", &formats, false, lit),
                main(&fragment, c"fragmentMain", &formats, true, lit),
                main(
                    &fragment_motion,
                    c"fragmentMainMotion",
                    &motion_formats,
                    false,
                    lit,
                ),
                main(
                    &fragment_masked_motion,
                    c"fragmentMaskedMotion",
                    &motion_formats,
                    false,
                    lit,
                ),
                blended(&fragment_blended, c"fragmentBlended", &formats, lit),
                blended(
                    &fragment_blended_reactive,
                    c"fragmentBlendedReactive",
                    &reactive_formats,
                    lit,
                ),
            ]);
        }
        let background_formats = [MOTION_FORMAT];
        let full_screen = |color_formats, fragment| GraphicsPipelineDesc {
            color_formats,
            depth_format,
            cull_mode: vk::CullModeFlags::NONE,
            depth_write: false,
            depth_compare: vk::CompareOp::EQUAL,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &background_vertex,
                    entry: c"backgroundVertex",
                },
                Some(fragment),
            )
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
        let masked_depth = ShaderStageDesc {
            spirv: &shadow_fragment_masked,
            entry: c"shadowFragmentMasked",
        };
        let prepass = |spirv, entry, fragment| GraphicsPipelineDesc {
            depth_format,
            dynamic_culling: true,
            ..GraphicsPipelineDesc::new(ShaderStageDesc { spirv, entry }, fragment)
        };
        descs.push(prepass(&depth_vertex, c"depthVertex", None));
        descs.push(prepass(
            &depth_vertex_masked,
            c"depthVertexMasked",
            Some(masked_depth),
        ));
        // With motion vectors, which the prepass then writes (the shading after it draws
        // without them).
        let motion_only = [MOTION_FORMAT];
        descs.push(GraphicsPipelineDesc {
            color_formats: &motion_only,
            ..prepass(
                &depth_vertex_motion,
                c"depthVertexMotion",
                Some(ShaderStageDesc {
                    spirv: &prepass_motion_fragment,
                    entry: c"prepassMotion",
                }),
            )
        });
        descs.push(GraphicsPipelineDesc {
            color_formats: &motion_only,
            ..prepass(
                &depth_vertex_masked_motion,
                c"depthVertexMaskedMotion",
                Some(ShaderStageDesc {
                    spirv: &prepass_masked_motion,
                    entry: c"prepassMaskedMotion",
                }),
            )
        });
        let shadow = |spirv, entry, fragment| GraphicsPipelineDesc {
            depth_format: shadow_settings.format,
            dynamic_culling: true,
            depth_bias: Some(DepthBias {
                constant: 1.0,
                slope: 1.5,
                clamp: 0.0,
            }),
            ..GraphicsPipelineDesc::new(ShaderStageDesc { spirv, entry }, fragment)
        };
        descs.push(shadow(&shadow_vertex, c"shadowVertex", None));
        descs.push(shadow(
            &shadow_vertex_masked,
            c"shadowVertexMasked",
            Some(masked_depth),
        ));
        // A cached cascade's pieces drawn at once (ADR 0093): a viewport per piece, the
        // vertex shaders picking one; described last.
        let pieces = device.adapter().caps.viewport_index;
        let piece_descs = [
            GraphicsPipelineDesc {
                viewports: crate::shadow_cache::MAX_PIECES as u32,
                ..shadow(&shadow_vertex_pieces, c"shadowVertexPieces", None)
            },
            GraphicsPipelineDesc {
                viewports: crate::shadow_cache::MAX_PIECES as u32,
                ..shadow(
                    &shadow_vertex_masked_pieces,
                    c"shadowVertexMaskedPieces",
                    Some(masked_depth),
                )
            },
        ];
        // The atmosphere's full-screen pass (blended premultiplied: light + target ×
        // transmittance; it samples the depth) and the transparent surfaces under it, in
        // the environment variants only.
        let composite_vertex = spirv_words(COMPOSITE_VERTEX).ok_or_else(invalid)?;
        let composite_fragment = spirv_words(COMPOSITE_FRAGMENT).ok_or_else(invalid)?;
        lazy_descs.push(GraphicsPipelineDesc {
            color_formats: &formats,
            cull_mode: vk::CullModeFlags::NONE,
            depth_test: false,
            depth_write: false,
            blend: Blend::Premultiplied,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &composite_vertex,
                    entry: c"compositeVertex",
                },
                Some(ShaderStageDesc {
                    spirv: &composite_fragment,
                    entry: c"compositeFragment",
                }),
            )
        });
        let fragment_blended_atmosphere =
            spirv_words(FRAGMENT_BLENDED_ATMOSPHERE).ok_or_else(invalid)?;
        let fragment_blended_atmosphere_reactive =
            spirv_words(FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE).ok_or_else(invalid)?;
        for lit in [2, 3] {
            lazy_descs.push(blended(
                &fragment_blended_atmosphere,
                c"fragmentBlendedAtmosphere",
                &formats,
                lit,
            ));
            lazy_descs.push(blended(
                &fragment_blended_atmosphere_reactive,
                c"fragmentBlendedAtmosphereReactive",
                &reactive_formats,
                lit,
            ));
        }
        // The compute passes' shaders, created with the rest.
        let cull = ComputeSource::<Push>::new(CULL, c"cullMain", &[])?;
        let shadow_cull = ComputeSource::<Push>::new(CULL_SHADOWS, c"cullShadows", &[])?;
        let light_tiles = ComputeSource::<Push>::new(LIGHT_TILES, c"lightTiles", &[])?;
        let reduce = DepthPyramid::source()?;
        let mut all: Vec<PipelineDesc<'_>> =
            descs.iter().copied().map(PipelineDesc::Graphics).collect();
        all.extend([
            cull.desc(),
            shadow_cull.desc(),
            light_tiles.desc(),
            reduce.desc(),
        ]);
        let layout = bindless.pipeline_layout();
        let mut pipelines = Pipeline::create_many(device, layout, &all)?.into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "forward pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let background = next()?;
        let sky = next()?;
        let prepass = [next()?, next()?];
        let prepass_motion = [next()?, next()?];
        let shadow = [next()?, next()?];
        let cull = cull.finish(next()?);
        let shadow_cull = shadow_cull.finish(next()?);
        let light_tiles = light_tiles.finish(next()?);
        let pyramid = DepthPyramid::from_source(&reduce, next()?);
        if pieces {
            lazy_descs.extend(piece_descs);
        }
        let lazy: Vec<PipelineDesc<'_>> = lazy_descs
            .iter()
            .copied()
            .map(PipelineDesc::Graphics)
            .collect();
        let mut described =
            LazyPipeline::describe_many(device, bindless.shared_pipeline_layout(), &lazy)
                .into_iter();
        let mut next = || {
            described.next().ok_or(RhiError::Vulkan {
                call: "forward shading (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let mut shading = || -> Result<Shading, RhiError> {
            Ok(Shading {
                main: [next()?, next()?],
                shade: next()?,
                main_motion: [next()?, next()?],
                blended: [next()?, next()?],
            })
        };
        let shading = [shading()?, shading()?, shading()?, shading()?];
        let composite = next()?;
        let blended_atmosphere = [[next()?, next()?], [next()?, next()?]];
        let shadow_pieces = if pieces {
            Some([next()?, next()?])
        } else {
            None
        };
        Ok(Self {
            shading,
            prepass,
            prepass_motion,
            background,
            sky,
            composite,
            blended_atmosphere,
            shadow,
            shadow_pieces,
            cull,
            shadow_cull,
            meshlet_cull: None,
            vsm: None,
            vsm_on: false,
            meshlets_on: false,
            light_tiles,
            pyramid,
            shadow_settings,
            frustum_culling: true,
            light_culling: true,
            lod_threshold: 1.0,
            depth_prepass: false,
            reactive_effects: false,
            color_format,
            depth_format,
            ao: None,
            ao_settings: None,
            ao_frame: Cell::new(0),
            subsurface_pipelines: None,
            subsurface_settings: Some(SubsurfaceSettings::default()),
            subsurface_profiles: Vec::new(),
            local_shadows: None,
            local_shadows_on: false,
            contact: None,
            contact_settings: None,
            retired_atlases: Vec::new(),
            reflections: None,
            reflections_on: false,
            decals: None,
            decals_on: false,
            gi: None,
            gi_on: false,
            msaa: [None, None, None],
            depth_resolves: {
                let modes = device.adapter().depth_resolve_modes;
                let pick = |flag, mode| {
                    if modes.contains(flag) {
                        mode
                    } else {
                        ResolveMode::SampleZero
                    }
                };
                [
                    pick(vk::ResolveModeFlags::MIN, ResolveMode::Min),
                    pick(vk::ResolveModeFlags::MAX, ResolveMode::Max),
                ]
            },
            scratch: RefCell::new(Scratch::default()),
            material_types: material_types::MaterialTypes::new(),
            bent_twins: bent::BentTwins::default(),
        })
    }

    /// Makes the pipelines for drawing into multisampled targets of `samples` (2, 4 or 8;
    /// MSAA, ADR 0075), with the features turned on now (those turned on later follow).
    /// A view draws multisampled when its `color` and `depth` are; it then always draws
    /// the depth prepass, masked materials turn alpha into coverage, and the passes that
    /// read the scene by pixel read single-sampled resolves (see
    /// [`ForwardPasses::resolved_depth`]). One sample makes nothing.
    pub fn set_samples(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        samples: vk::SampleCountFlags,
    ) -> Result<(), RhiError> {
        let Some(slot) = msaa_slot(samples) else {
            return Ok(());
        };
        if self.msaa[slot].is_none() {
            self.msaa[slot] = Some(Box::new(msaa::MsaaPipelines::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
                samples,
            )?));
        }
        self.ensure_msaa(device, bindless)
    }

    /// The multisampled pipelines of the features turned on, for every sample count made.
    fn ensure_msaa(&mut self, device: &Device, bindless: &Bindless) -> Result<(), RhiError> {
        let features = msaa::Features {
            ao: self.ao.is_some(),
            subsurface: self.subsurface_pipelines.is_some(),
            shadowed: self.local_shadows.is_some(),
            contact: self.contact.is_some(),
            reflect: self.reflections.is_some(),
        };
        let (color, depth) = (self.color_format, self.depth_format);
        for set in self.msaa.iter_mut().flatten() {
            set.ensure(device, bindless, color, depth, features)?;
        }
        Ok(())
    }

    /// The multisampled pipelines for `samples`, when made.
    fn msaa_pipelines(&self, samples: vk::SampleCountFlags) -> Option<&msaa::MsaaPipelines> {
        self.msaa[msaa_slot(samples)?].as_deref()
    }

    /// Turns GPU culling on (the default) or off. Off, every instance counts as visible,
    /// with or without an [`Occlusion`] state: for comparisons and debugging.
    pub fn set_frustum_culling(&mut self, enabled: bool) {
        self.frustum_culling = enabled;
    }

    /// Whether GPU culling is on.
    pub fn frustum_culling(&self) -> bool {
        self.frustum_culling
    }

    /// Turns the depth prepass on or off (the default; ADR 0042). On, a depth-only pass
    /// draws the visible instances first and the forward pass shades each pixel once;
    /// off, the forward pass writes depth itself and shades whatever is nearest so far
    /// (batches are drawn front to back, which avoids most of that).
    ///
    /// It pays where expensive shading hides behind other geometry and vertices are few
    /// (a textured interior on an integrated GPU: Sponza −11%); it costs a second pass
    /// over the vertices, which dense geometry cannot afford (100k cubes: +97%).
    pub fn set_depth_prepass(&mut self, enabled: bool) {
        self.depth_prepass = enabled;
    }

    /// Whether the depth prepass is on.
    pub fn depth_prepass(&self) -> bool {
        self.depth_prepass
    }

    /// Turns on (or off, the default) a reactive mask for every view with motion vectors,
    /// cleared, even when no transparent surface drew one: for transparent effects drawn
    /// after the scene passes, such as particles (ADR 0081), which mark the pixels they
    /// cover so that temporal antialiasing and upscaling trust their history less there
    /// (without it the effects ghost). Costs a clear of an 8-bit mask at the render size.
    pub fn set_reactive_effects(&mut self, enabled: bool) {
        self.reactive_effects = enabled;
    }

    /// Whether views get a reactive mask for effects ([`Self::set_reactive_effects`]).
    pub fn reactive_effects(&self) -> bool {
        self.reactive_effects
    }

    /// Turns ambient occlusion (GTAO, ADR 0058) on with `settings`, or off (`None`, the
    /// default). It darkens the ambient and environment light where the surroundings hide
    /// the sky, not the sun or local lights. It needs the depth before shading, so the
    /// depth prepass runs whenever it is on. Perspective views only: orthographic ones
    /// draw without it. The first time it is turned on its passes and 8 shading pipelines
    /// are made (in parallel, through the pipeline cache: ADR 0055), so a renderer that
    /// never uses it pays nothing, not even registers in its shading.
    pub fn set_ambient_occlusion(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        settings: Option<AoSettings>,
    ) -> Result<(), RhiError> {
        if settings.is_some() && self.ao.is_none() {
            self.ao = Some(AoPasses::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
            )?);
        }
        self.ao_settings = settings;
        self.ensure_shadowed_extras(device, bindless)?;
        self.ensure_msaa(device, bindless)
    }

    /// Local shadows' shading for ambient occlusion and subsurface materials, once both
    /// features are in use.
    fn ensure_shadowed_extras(
        &mut self,
        device: &Device,
        bindless: &Bindless,
    ) -> Result<(), RhiError> {
        let (ao, subsurface) = (self.ao.is_some(), self.subsurface_pipelines.is_some());
        let shadows = self.local_shadows.is_some();
        let (color, depth) = (self.color_format, self.depth_format);
        if let Some(state) = self.local_shadows.as_mut() {
            state
                .pipelines
                .ensure_extras(device, bindless, color, depth, ao, subsurface)?;
        }
        if let Some(state) = self.contact.as_mut() {
            state.ensure_extras(device, bindless, (color, depth), ao, shadows)?;
        }
        self.ensure_decal_shading(device, bindless)
    }

    /// The decal shading for the features in use (ambient occlusion, contact shadows,
    /// local shadows, reflections), when decals are on.
    fn ensure_decal_shading(
        &mut self,
        device: &Device,
        bindless: &Bindless,
    ) -> Result<(), RhiError> {
        let features = pipelines::DecalFeatures {
            ao: self.ao.is_some(),
            contact: self.contact.is_some(),
            shadows: self.local_shadows.is_some(),
            reflections: self.reflections.is_some(),
        };
        let formats = (self.color_format, self.depth_format);
        if let Some(decals) = self.decals.as_mut() {
            decals.ensure(device, bindless, formats, features)?;
        }
        self.ensure_gi_shading(device, bindless)
    }

    /// The probe-GI shading for the features in use, when GI is on.
    fn ensure_gi_shading(&mut self, device: &Device, bindless: &Bindless) -> Result<(), RhiError> {
        let features = GiFeatures {
            ao: self.ao.is_some(),
            contact: self.contact.is_some(),
            shadows: self.local_shadows.is_some(),
            reflections: self.reflections.is_some(),
            decals: self.decals.is_some(),
            subsurface: self.subsurface_pipelines.is_some(),
        };
        let formats = (self.color_format, self.depth_format);
        match self.gi.as_mut() {
            Some(gi) => gi.ensure(device, bindless, formats, features),
            None => Ok(()),
        }
    }

    /// Turns global illumination from a volume of probes (ADR 0088, [`crate::gi`]) on or
    /// off (the default): views with [`ForwardView::gi`] then take the diffuse light of
    /// their opaque surfaces from it, through a pass after the depth prepass (which runs
    /// in those views). The first time it is turned on its shading pipelines are
    /// described (up to 78 with every other feature on; made on first use or by the
    /// device's warm-up, ADR 0079), so a renderer that never uses it pays nothing, not
    /// even registers in its shading.
    pub fn set_global_illumination(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        enabled: bool,
    ) -> Result<(), RhiError> {
        if enabled && self.gi.is_none() {
            self.gi = Some(Box::default());
            self.ensure_gi_shading(device, bindless)?;
        }
        self.gi_on = enabled;
        Ok(())
    }

    /// Whether global illumination is on.
    pub fn global_illumination(&self) -> bool {
        self.gi_on
    }

    /// Turns the sun's contact shadows on with `settings`, or off (`None`, the default;
    /// ADR 0071): what the depth buffer hides from the sun at scales its cascades cannot
    /// resolve (a pebble, a hand on a table, grass), as a mask multiplied into the sun's
    /// light. They need the depth before shading, so the depth prepass runs whenever they
    /// are on; perspective views only. Transparent surfaces and subsurface materials do
    /// not get them. The first time, their pass and up to 16 shading pipelines are made
    /// (in parallel, through the pipeline cache: ADR 0055); a renderer that never uses
    /// them pays nothing, not even registers in its shading.
    pub fn set_contact_shadows(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        settings: Option<ContactShadowSettings>,
    ) -> Result<(), RhiError> {
        if settings.is_some() && self.contact.is_none() {
            self.contact = Some(ContactState::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
            )?);
            self.ensure_shadowed_extras(device, bindless)?;
            self.ensure_msaa(device, bindless)?;
        }
        self.contact_settings = settings;
        Ok(())
    }

    /// The contact shadow settings, when they are on.
    pub fn contact_shadows(&self) -> Option<ContactShadowSettings> {
        self.contact_settings
    }

    /// The ambient occlusion settings, when it is on.
    pub fn ambient_occlusion(&self) -> Option<AoSettings> {
        self.ao_settings
    }

    /// Turns local reflections (ADR 0073, [`crate::reflections`]) on or off (the
    /// default): views with [`ForwardView::reflections`] then use their probes and
    /// screen-space reflections. The first time they are turned on their passes and 24
    /// shading pipelines are made (in parallel, through the pipeline cache: ADR 0055), so
    /// a renderer that never uses them pays nothing, not even registers in its shading.
    /// They need the depth before shading, so the depth prepass runs in views with
    /// reflections.
    pub fn set_reflections(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        enabled: bool,
    ) -> Result<(), RhiError> {
        if enabled && self.reflections.is_none() {
            self.reflections = Some(Box::new(ReflectionShading::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
            )?));
            self.ensure_msaa(device, bindless)?;
            self.ensure_decal_shading(device, bindless)?;
        }
        self.reflections_on = enabled;
        Ok(())
    }

    /// Whether local reflections are on.
    pub fn reflections(&self) -> bool {
        self.reflections_on
    }

    /// Draws the instances whose detail level has meshlets (ADR 0085) one indexed draw per
    /// meshlet that may be seen, in the forward phases (the depth prepass, the shading,
    /// texture feedback): culling tests each meshlet's bounding sphere against the
    /// frustum, its normal cone against the camera (single-sided materials, uniformly
    /// scaled instances) and, in occlusion culling's second phase, against the depth
    /// pyramid. The sun's shadow cascades draw by meshlet too, with or without a prepass:
    /// each cascade's bounds, and the normal cone seen from the light. Blended instances,
    /// those of material types and those beyond a frame's room for meshlet draws keep their
    /// instanced draw, as do local lights' shadow views and reflection probes' captures.
    /// Meshes get meshlets from the cook (`engine_assets`). Needs the device's
    /// `drawIndirectCount` and `shaderInt64`; returns whether it is on.
    pub fn set_meshlet_culling(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        enabled: bool,
    ) -> Result<bool, RhiError> {
        let caps = &device.adapter().caps;
        let supported = caps.draw_indirect_count && caps.shader_int64;
        if enabled && supported && self.meshlet_cull.is_none() {
            let [instances, shadow_instances, meshlets] = ComputeSource::create_all(
                device,
                bindless,
                [
                    ComputeSource::<Push>::new(CULL_MESHLETS, c"cullMainMeshlets", &[])?,
                    ComputeSource::<Push>::new(CULL_SHADOWS_MESHLETS, c"cullShadowsMeshlets", &[])?,
                    ComputeSource::<Push>::new(MESHLET_CULL, c"cullMeshlets", &[])?,
                ],
            )?;
            let pieces = if caps.viewport_index {
                let [pieces] = ComputeSource::create_all(
                    device,
                    bindless,
                    [ComputeSource::<Push>::new(
                        MESHLET_PIECES_CULL,
                        c"cullMeshletPieces",
                        &[],
                    )?],
                )?;
                Some(pieces)
            } else {
                None
            };
            self.meshlet_cull = Some(Box::new(MeshletCull {
                instances,
                shadow_instances,
                meshlets,
                pieces,
            }));
        }
        self.meshlets_on = enabled && supported;
        Ok(self.meshlets_on)
    }

    /// Whether views draw by meshlet ([`Self::set_meshlet_culling`]).
    pub fn meshlet_culling(&self) -> bool {
        self.meshlets_on
    }

    /// The meshlet path's shaders while it is on.
    fn meshlet_shaders(&self) -> Option<&MeshletCull> {
        self.meshlet_cull.as_deref().filter(|_| self.meshlets_on)
    }

    /// Turns the sun's virtual shadow pages (ADR 0093 step 3) on or off (the default):
    /// views with [`ForwardView::virtual_shadows`] then mark them from their depth prepass
    /// (which then runs), draw the casters into their dirty pages, and take the sun's
    /// shadow on opaque surfaces from them. Their culls and pipelines are made the first
    /// time they are turned on, so a renderer that never uses them pays nothing. Needs the
    /// device's `fragmentStoresAndAtomics`, `drawIndirectCount`, `shaderInt64` and
    /// `shaderClipDistance`; returns whether they are on.
    pub fn set_virtual_shadows(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        enabled: bool,
    ) -> Result<bool, RhiError> {
        let caps = &device.adapter().caps;
        let supported = caps.fragment_stores_and_atomics
            && caps.draw_indirect_count
            && caps.shader_int64
            && caps.shader_clip_distance;
        if enabled && supported && self.vsm.is_none() {
            self.vsm = Some(Box::new(VsmDraw::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
            )?));
        }
        // Their views shade with the variants that read the sun's mask (the contact
        // shadows'), described now if contact shadows never were.
        if enabled && supported && self.contact.is_none() {
            self.contact = Some(ContactState::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
            )?);
            self.ensure_shadowed_extras(device, bindless)?;
            self.ensure_msaa(device, bindless)?;
        }
        self.vsm_on = enabled && supported;
        Ok(self.vsm_on)
    }

    /// Whether views with virtual shadow pages use them ([`Self::set_virtual_shadows`]).
    pub fn virtual_shadows(&self) -> bool {
        self.vsm_on
    }

    /// The virtual shadow pages' culls and draws while they are on.
    fn vsm_draw(&self) -> Option<&VsmDraw> {
        self.vsm.as_deref().filter(|_| self.vsm_on)
    }

    /// Turns decals (ADR 0082, [`crate::decals`]) on or off (the default): views with
    /// [`ForwardView::decals`] then draw them through a decal buffer after the depth
    /// prepass, which runs in those views. The first time they are turned on their passes
    /// are made and their shading pipelines described (up to 36 with every other feature
    /// on; made on first use or by the device's warm-up, ADR 0079), so a renderer that
    /// never uses them pays nothing, not even registers in its shading.
    pub fn set_decals(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        enabled: bool,
    ) -> Result<(), RhiError> {
        if enabled && self.decals.is_none() {
            self.decals = Some(Box::new(DecalShading::new(device, bindless)?));
            self.ensure_decal_shading(device, bindless)?;
        }
        self.decals_on = enabled;
        Ok(())
    }

    /// Whether decals are on.
    pub fn decals(&self) -> bool {
        self.decals_on
    }

    /// The driver's statistics of the decal shading (ADR 0082) per shading variant, as
    /// [`Self::shading_statistics`] (after the prepass, without other features). Empty
    /// unless the device captures statistics and decals were turned on.
    pub fn decal_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        match &self.decals {
            Some(d) => core::array::from_fn(|k| {
                d.pipeline(k, false, false, false, false)
                    .map(|p| lazy_statistics(p, device))
                    .unwrap_or_default()
            }),
            None => Default::default(),
        }
    }

    /// The driver's statistics of the reflection pipelines (ADR 0073): the opaque shading
    /// of views with reflections without and with local lights, then the composite pass
    /// with probes, with screen-space reflections, with both. Empty unless the device
    /// captures statistics and reflections were turned on.
    pub fn reflection_statistics(
        &self,
        device: &Device,
    ) -> [Vec<engine_rhi::PipelineStatistic>; 5] {
        match &self.reflections {
            Some(r) => {
                let [a, b, c] = r.passes.composite_statistics(device);
                let stats = |lights| {
                    r.pipeline(lights, false, false, false)
                        .map(|p| lazy_statistics(p, device))
                        .unwrap_or_default()
                };
                [stats(false), stats(true), a, b, c]
            }
            None => Default::default(),
        }
    }

    /// Turns subsurface scattering (ADR 0059) on with `settings` (the default) or off
    /// (`None`: subsurface materials shade as ordinary ones). It needs the depth before
    /// shading, so the depth prepass runs whenever a subsurface material is drawn.
    pub fn set_subsurface(&mut self, settings: Option<SubsurfaceSettings>) {
        self.subsurface_settings = settings;
    }

    /// The subsurface scattering settings, when it is on.
    pub fn subsurface(&self) -> Option<SubsurfaceSettings> {
        self.subsurface_settings
    }

    /// Sets the diffusion profiles that subsurface materials name by index
    /// ([`crate::material::Subsurface::profile`]; at most [`subsurface::MAX_PROFILES`],
    /// the rest are ignored). Without profiles, subsurface materials shade as ordinary
    /// ones. The first profiles also create subsurface materials' 9 pipelines (in
    /// parallel, through the pipeline cache: ADR 0055), so a game without them never
    /// compiles them; each profile's sample kernels take well under a millisecond. Call it
    /// when profiles change (a level load), not every frame.
    pub fn set_subsurface_profiles(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        profiles: &[SubsurfaceProfile],
    ) -> Result<(), RhiError> {
        if !profiles.is_empty() && self.subsurface_pipelines.is_none() {
            self.subsurface_pipelines = Some(SubsurfacePipelines::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
            )?);
        }
        self.subsurface_profiles.clear();
        self.subsurface_profiles.extend(
            profiles
                .iter()
                .take(subsurface::MAX_PROFILES)
                .map(SubsurfaceProfile::gpu),
        );
        self.ensure_shadowed_extras(device, bindless)?;
        self.ensure_msaa(device, bindless)
    }

    /// Turns shadows of point and spot lights on with `settings`, or off (`None`, the
    /// default; ADR 0071). Lights cast them when marked ([`LocalLight::shadow`]). The
    /// first time, the atlas (`atlas_size`² texels of `format`, 32 MiB at the default) and
    /// 22 pipelines are made (in parallel, through the pipeline cache: ADR 0055); a view
    /// with no shadowed light in reach keeps its shading pipelines, which carry no code
    /// for it. New settings that change the atlas start over with an empty one (the old
    /// one is freed by [`Self::destroy`]); off keeps the atlas for later.
    ///
    /// The atlas is a cache: shadows are drawn again only when their light or a caster in
    /// its range changes (see [`crate::local_shadows`]). Views of one renderer share it:
    /// give them the same light list.
    pub fn set_local_shadows(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        settings: Option<LocalShadowSettings>,
    ) -> Result<(), RhiError> {
        let Some(settings) = settings.map(|s| s.sanitized()) else {
            self.local_shadows_on = false;
            return Ok(());
        };
        let rebuild_atlas = self.local_shadows.as_ref().is_none_or(|state| {
            let old = state.cache.borrow().settings().to_owned();
            old.atlas_size != settings.atlas_size || old.format != settings.format
        });
        let rebuild_pipelines = self.local_shadows.as_ref().is_none_or(|state| {
            let old = state.cache.borrow().settings().to_owned();
            old.format != settings.format || old.depth_bias != settings.depth_bias
        });
        if rebuild_atlas || rebuild_pipelines {
            let pipelines = if rebuild_pipelines {
                let mut p = ShadowedPipelines::new(
                    device,
                    bindless,
                    self.color_format,
                    self.depth_format,
                    &settings,
                )?;
                p.ensure_extras(
                    device,
                    bindless,
                    self.color_format,
                    self.depth_format,
                    self.ao.is_some(),
                    self.subsurface_pipelines.is_some(),
                )?;
                Some(p)
            } else {
                None
            };
            let atlas = if rebuild_atlas {
                Some(AtlasImage::new(device, allocator, bindless, &settings)?)
            } else {
                None
            };
            match self.local_shadows.take() {
                Some(old) => {
                    let LocalShadowState {
                        atlas: old_atlas,
                        pipelines: old_pipelines,
                        ..
                    } = old;
                    let atlas = match atlas {
                        Some(new) => {
                            self.retired_atlases.push(old_atlas);
                            new
                        }
                        None => old_atlas,
                    };
                    self.local_shadows = Some(LocalShadowState {
                        atlas,
                        pipelines: pipelines.unwrap_or(old_pipelines),
                        cache: RefCell::new(ShadowCache::new(settings)),
                        plan: RefCell::new(Plan::default()),
                        imported: Cell::new(None),
                        rects: RefCell::new(Vec::new()),
                    });
                }
                None => {
                    self.local_shadows = Some(LocalShadowState {
                        atlas: atlas.ok_or(RhiError::Vulkan {
                            call: "local shadow atlas",
                            result: vk::Result::ERROR_UNKNOWN,
                        })?,
                        pipelines: pipelines.ok_or(RhiError::Vulkan {
                            call: "local shadow pipelines",
                            result: vk::Result::ERROR_UNKNOWN,
                        })?,
                        cache: RefCell::new(ShadowCache::new(settings)),
                        plan: RefCell::new(Plan::default()),
                        imported: Cell::new(None),
                        rects: RefCell::new(Vec::new()),
                    });
                }
            }
        } else if let Some(state) = &self.local_shadows {
            // Same atlas and pipelines: new tile sizes or budgets start from scratch.
            let mut cache = state.cache.borrow_mut();
            if *cache.settings() != settings {
                *cache = ShadowCache::new(settings);
            }
        }
        self.local_shadows_on = true;
        self.ensure_shadowed_extras(device, bindless)?;
        self.ensure_msaa(device, bindless)
    }

    /// The local shadow settings, when they are on.
    pub fn local_shadows(&self) -> Option<LocalShadowSettings> {
        self.local_shadows
            .as_ref()
            .filter(|_| self.local_shadows_on)
            .map(|s| *s.cache.borrow().settings())
    }

    /// What the last frame did with local lights' shadows: lights in view, drawn, views
    /// drawn, waiting for the budget.
    pub fn local_shadow_stats(&self) -> LocalShadowStats {
        self.local_shadows
            .as_ref()
            .map_or_else(LocalShadowStats::default, |s| s.cache.borrow().stats())
    }

    /// Forgets every cached local shadow: each is drawn again when next in view (after a
    /// level load, or edits the renderer cannot see, such as a mesh's vertices replaced in
    /// place).
    pub fn invalidate_local_shadows(&self) {
        if let Some(state) = &self.local_shadows {
            state.cache.borrow_mut().clear();
        }
    }

    /// The driver's statistics of the culling and the sun's shadow pipelines, by name:
    /// the view's cull (and with meshlets its meshlet path and meshlet cull), the shadow
    /// cascades' (ADR 0093), and the shadow pass's opaque and masked pipelines. Empty
    /// unless the device captures statistics.
    pub fn cull_statistics(
        &self,
        device: &Device,
    ) -> Vec<(&'static str, Vec<engine_rhi::PipelineStatistic>)> {
        let mut out = vec![
            ("cull", self.cull.pipeline().statistics(device)),
            (
                "shadow cull",
                self.shadow_cull.pipeline().statistics(device),
            ),
        ];
        if let Some(m) = self.meshlet_cull.as_deref() {
            out.push((
                "cull, meshlet path",
                m.instances.pipeline().statistics(device),
            ));
            out.push((
                "shadow cull, meshlet path",
                m.shadow_instances.pipeline().statistics(device),
            ));
            out.push(("meshlet cull", m.meshlets.pipeline().statistics(device)));
        }
        out.push(("shadow", self.shadow[0].statistics(device)));
        out.push(("shadow, masked", self.shadow[1].statistics(device)));
        out
    }

    /// The driver's statistics of the opaque forward pipelines that look up local lights'
    /// shadows: without and with an environment. Empty unless local shadows were turned on
    /// and the device captures statistics.
    pub fn shadowed_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 2] {
        match &self.local_shadows {
            Some(s) => {
                core::array::from_fn(|k| lazy_statistics(&s.pipelines.shading[k].main[0], device))
            }
            None => [Vec::new(), Vec::new()],
        }
    }

    /// The driver's statistics of the shading pipelines that read the sun's contact
    /// shadows (after the prepass), per shading variant. Empty unless contact shadows were
    /// turned on and the device captures statistics.
    pub fn contact_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        match &self.contact {
            Some(c) => core::array::from_fn(|k| lazy_statistics(&c.shading[k], device)),
            None => Default::default(),
        }
    }

    /// Frees what the renderer keeps in device memory: the local shadow atlases (ADR
    /// 0071). Nothing to do for a renderer that never turned them on.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) {
        for mut atlas in self.retired_atlases.drain(..) {
            // SAFETY: guaranteed by the caller.
            unsafe { atlas.destroy(device, allocator, bindless) };
        }
        if let Some(state) = self.local_shadows.as_mut() {
            // SAFETY: guaranteed by the caller.
            unsafe { state.atlas.destroy(device, allocator, bindless) };
        }
        self.local_shadows = None;
        self.local_shadows_on = false;
    }

    /// The driver's statistics of the opaque forward pipeline of each shading variant (see
    /// `ForwardRenderer::shading`): without and with local lights, then the same with an
    /// environment. Empty unless the device captures statistics
    /// ([`Device::pipeline_statistics_enabled`]).
    pub fn shading_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        core::array::from_fn(|k| lazy_statistics(&self.shading[k].main[0], device))
    }

    /// The driver's statistics of the probe-GI shading after the depth prepass (ADR 0088),
    /// per shading variant as [`Self::shading_statistics`], without decals, ambient
    /// occlusion or contact shadows. Empty while GI is off.
    pub fn gi_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        core::array::from_fn(|k| {
            self.gi
                .as_deref()
                .and_then(|gi| gi.pipeline(k, false, false, false, false, false))
                .map(|p| lazy_statistics(p, device))
                .unwrap_or_default()
        })
    }

    /// The driver's statistics of what a view with motion vectors (temporal antialiasing
    /// and upscaling) shades with, per shading variant as [`Self::shading_statistics`]: the
    /// shading that writes them (views without the depth prepass), then the shading after
    /// the prepass with ambient occlusion (`fragmentMainAo`: the default settings, whose
    /// prepass writes the motion vectors; empty until ambient occlusion was turned on).
    pub fn motion_statistics(
        &self,
        device: &Device,
    ) -> [[Vec<engine_rhi::PipelineStatistic>; 2]; 4] {
        core::array::from_fn(|k| {
            [
                lazy_statistics(&self.shading[k].main_motion[0], device),
                self.ao
                    .as_ref()
                    .map(|ao| lazy_statistics(&ao.shading[k], device))
                    .unwrap_or_default(),
            ]
        })
    }

    /// The driver's statistics of the shading after the prepass with ambient occlusion and
    /// its bent normals ([`AoSettings::bent_normals`]), per shading variant as
    /// [`Self::shading_statistics`] (made now); empty until ambient occlusion was turned on.
    pub fn bent_statistics(&mut self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        core::array::from_fn(|k| {
            let Some(role) = self.ao.as_ref().map(|ao| ao.shading[k].clone()) else {
                return Vec::new();
            };
            self.bent_twins
                .twin_now(device, &role)
                .map(|twin| lazy_statistics(&twin, device))
                .unwrap_or_default()
        })
    }

    /// The driver's statistics of the depth prepass of opaque materials: depth only, then
    /// also writing motion vectors (views with a temporal state).
    pub fn prepass_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 2] {
        [
            self.prepass[0].statistics(device),
            self.prepass_motion[0].statistics(device),
        ]
    }

    /// The driver's statistics of the multisampled shading pipelines for `samples` (ADR
    /// 0075; made by [`Self::set_samples`]), per shading variant as
    /// [`Self::shading_statistics`], after the prepass without motion vectors, then the
    /// atmosphere's per-sample pass. Empty when not made or not captured.
    pub fn msaa_statistics(
        &self,
        device: &Device,
        samples: vk::SampleCountFlags,
    ) -> [Vec<engine_rhi::PipelineStatistic>; 5] {
        match self.msaa_pipelines(samples) {
            Some(m) => core::array::from_fn(|k| match k {
                0..4 => lazy_statistics(&m.shading[k].shade, device),
                _ => lazy_statistics(&m.composite, device),
            }),
            None => Default::default(),
        }
    }

    /// The driver's statistics of the atmosphere's pipelines (ADR 0069): the full-screen
    /// pass, then transparent surfaces under an atmosphere or fog without and with local
    /// lights. Empty unless the device captures statistics.
    pub fn atmosphere_statistics(
        &self,
        device: &Device,
    ) -> [Vec<engine_rhi::PipelineStatistic>; 3] {
        [
            lazy_statistics(&self.composite, device),
            lazy_statistics(&self.blended_atmosphere[0][0], device),
            lazy_statistics(&self.blended_atmosphere[1][0], device),
        ]
    }

    /// Turns light culling on (the default) or off. Off, every pixel evaluates every light
    /// that reaches the view: the reference for tests and comparisons (ADR 0045).
    pub fn set_light_culling(&mut self, enabled: bool) {
        self.light_culling = enabled;
    }

    /// How far (in pixels on screen) a mesh's coarser detail level may stray from the full
    /// mesh to be drawn instead (default 1; ADR 0053). 0 always draws the full meshes.
    /// Orthographic views always draw the full meshes. The sun's cascades take it in their
    /// own texels instead: what they draw never depends on the camera (ADR 0093).
    pub fn set_lod_threshold(&mut self, pixels: f32) {
        self.lod_threshold = pixels.max(0.0);
    }

    /// The sun's shadows: what a view's [`CascadeCache`] is made with.
    pub fn shadow_settings(&self) -> &shadow::ShadowSettings {
        &self.shadow_settings
    }

    /// Declares the culling, shadow and forward passes for `draws` into the view's
    /// targets, drawing from `geometry` (this frame's [`GeometryPool`] buffers) and
    /// shading with `materials` (this frame's [`MaterialTable`]).
    ///
    /// Instances, batches, draw commands and frame constants are written into `ring` now;
    /// the passes record when the graph executes, which must be with this `bindless` set
    /// ([`FrameGraph::execute_with_bindless`]) so the shadow map and the depth target get
    /// indices. Returns `None`, declaring nothing, when `ring` is full.
    ///
    /// [`GeometryPool`]: crate::mesh::GeometryPool
    /// [`MaterialTable`]: crate::material::MaterialTable
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        draws: &[DrawItem],
        view: &ForwardView<'g>,
    ) -> Option<ForwardPasses> {
        engine_profiler::zone!("render::forward::add_passes");
        if draws.is_empty() {
            return Some(empty_frame(fg, view));
        }
        let mut setup = self.setup(fg, view);
        let imported = self.import(fg, ring, bindless, view, &setup)?;
        if let Some((probes, textures)) = imported.probes {
            self.add_captures(
                fg,
                ring,
                bindless,
                geometry,
                materials,
                view,
                &imported,
                probes,
                textures,
                |this, ring, capture, setup| {
                    this.layout_draws(ring, geometry, materials, draws, capture, setup)
                },
            )?;
        }
        setup.shadow_draws =
            self.plan_local_shadows(view, geometry, materials, &setup, Changes::Draws(draws));
        let layout = self.layout_draws(ring, geometry, materials, draws, view, &setup);
        let mesh_box = |mesh| {
            let (center, extents) = local_box(geometry, mesh);
            let revision = geometry.mesh(mesh).map_or(0, |m| m.revision);
            (center, extents, revision)
        };
        if let Some(cache) = view.shadow_cache {
            cache.note_draws(
                draws,
                mesh_box,
                self.caster_settings(view),
                materials.revision(),
            );
        }
        if let Some(pages) = view.virtual_shadows {
            pages.note_draws(
                draws,
                mesh_box,
                self.caster_settings(view),
                materials.revision(),
            );
        }
        let passes = layout.and_then(|layout| {
            self.declare(
                fg, ring, bindless, geometry, materials, view, &setup, &layout, &imported, None,
            )
        });
        if let Some((probes, _)) = imported.probes {
            probes.finish();
        }
        passes
    }

    /// Declares the culling, shadow and forward passes for the instances of a persistent
    /// [`GpuScene`](crate::gpu_scene::GpuScene) (ADR 0064) into the view's targets, as
    /// [`Self::add_passes`] does for a draw list. `scene` is this frame's
    /// ([`GpuScene::add_passes`](crate::gpu_scene::GpuScene::add_passes), called once per
    /// frame before every view that draws it).
    ///
    /// Nothing here depends on the number of instances on the CPU: batches, their draw
    /// commands and the frame constants are written into `ring` (a cost per batch), the
    /// instances are already on the GPU. Per-view state keys off the instances' slots:
    /// occlusion culling ([`Occlusion`], sized to at least
    /// [`GpuScene::slot_count`](crate::gpu_scene::GpuScene::slot_count) to remember every
    /// instance) and motion vectors (the scene keeps last frame's transforms; a
    /// [`Temporal`] state provides the camera and jitter, and its own buffer stays
    /// unused). Returns `None`, declaring nothing, when `ring` is full.
    #[allow(clippy::too_many_arguments)]
    pub fn add_scene_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        scene: &SceneFrame<'g>,
        view: &ForwardView<'g>,
    ) -> Option<ForwardPasses> {
        engine_profiler::zone!("render::forward::add_scene_passes");
        if scene.instances() == 0 {
            return Some(empty_frame(fg, view));
        }
        let mut setup = self.setup(fg, view);
        let imported = self.import(fg, ring, bindless, view, &setup)?;
        // Probe GI (ADR 0088, ADR 0089): the volume's updates while it converges, then its
        // window of the frame, tracing this frame's scene for ray queries; misses see the
        // view's sky.
        if let Some(ViewGi::Probes {
            volume,
            rays: Some(rays),
        }) = view.gi.filter(|_| self.gi_on)
            && volume.needs_updates()
        {
            let sky = match imported.environment {
                Some(e) => GiSky::Cube {
                    texture: e.source,
                    index: e.source_index,
                    intensity: e.intensity,
                    sh: e.sh_address,
                },
                None => GiSky::Flat(view.light.ambient),
            };
            let sun = imported
                .environment
                .and_then(|e| e.sun_color)
                .unwrap_or(view.light.color);
            volume
                .add_update_passes(
                    fg,
                    ring,
                    bindless,
                    &rays,
                    scene,
                    geometry,
                    materials,
                    view.camera.position,
                    Some((-view.light.direction, sun)),
                    sky,
                )
                .ok()?;
        }
        if let Some((probes, textures)) = imported.probes {
            // What moved, appeared or went away in the scene asks the probes that see it
            // for a new capture; instances moving between ticks move in every frame.
            probes.note_changes(scene.changes());
            probes.note_changes(scene.moving_boxes());
            self.add_captures(
                fg,
                ring,
                bindless,
                geometry,
                materials,
                view,
                &imported,
                probes,
                textures,
                |this, ring, capture, setup| {
                    this.layout_scene(ring, geometry, scene, capture, setup)
                },
            )?;
        }
        setup.shadow_draws =
            self.plan_local_shadows(view, geometry, materials, &setup, Changes::Scene(scene));
        if view.shadow_cache.is_some() || view.virtual_shadows.is_some() {
            self.note_scene_casters(geometry, materials, scene, view);
        }
        let layout = self.layout_scene(ring, geometry, scene, view, &setup);
        let passes = layout.and_then(|layout| {
            self.declare(
                fg, ring, bindless, geometry, materials, view, &setup, &layout, &imported, None,
            )
        });
        if let Some((probes, _)) = imported.probes {
            probes.finish();
        }
        passes
    }

    /// Imports what a view's passes read once per frame: its environment (rebuilt first
    /// if its sky changed, ADR 0046; its atmosphere's tables and aerial perspective, ADR
    /// 0069) and its reflection probes' images. `None` when `ring` is full.
    fn import<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        view: &ForwardView<'g>,
        setup: &Setup,
    ) -> Option<Imported<'g>> {
        let (_, unjittered, clip_from_world) = jittered(view, &setup.depth_desc);
        let environment = match view.environment {
            Some(e) => Some(e.add_passes(
                fg,
                ring,
                bindless,
                &AtmosphereView {
                    clip_from_world,
                    world_from_clip: clip_from_world.inverse(),
                    unjittered_world_from_clip: unjittered.inverse(),
                    camera: view.camera.position,
                    to_sun: -view.light.direction.normalize_or(Vec3::NEG_Y),
                    light: view.light.color,
                    size: [setup.depth_desc.width, setup.depth_desc.height],
                },
            )?),
            None => None,
        };
        let probes = view
            .reflections
            .probes
            .filter(|_| self.reflections_on && self.reflections.is_some() && environment.is_some())
            .map(|p| (p, p.import(fg)));
        Some(Imported {
            environment,
            probes,
        })
    }

    /// Draws this frame's faces of a reflection probe's capture (its budget), each as a
    /// view of its own into a transient target copied into the capture cube, then
    /// prefilters the cube when it is complete. `layout` lays the instances out for a
    /// capture view (once per frame: the faces share it).
    #[allow(clippy::too_many_arguments)]
    fn add_captures<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        view: &ForwardView<'g>,
        imported: &Imported<'g>,
        probes: &'g ReflectionProbes,
        textures: ProbeTextures,
        mut layout: impl FnMut(&Self, &mut UploadRing, &ForwardView<'g>, &Setup) -> Option<Layout>,
    ) -> Option<()> {
        engine_profiler::zone!("render::forward::add_captures");
        let environment = imported.environment?;
        let sun = environment.sun_color.unwrap_or(view.light.color);
        let Some(plan) = probes.plan(&view.light, sun, environment.rebuilt, view.camera.position)
        else {
            return Some(());
        };
        let size = probes.settings().resolution;
        // The capture sees the sky from the environment's cube (filled in by the face
        // copy), without the atmosphere's full-screen pass.
        let capture_imported = Imported {
            environment: Some(EnvironmentFrame {
                atmosphere: None,
                ..environment
            }),
            probes: None,
        };
        let mut laid_out = None;
        let mut shadow = None;
        for face in plan.first_face..plan.first_face + plan.faces {
            let color = fg.create_texture(
                "probe face",
                TextureDesc::new_2d(self.color_format, size, size),
            );
            let depth = fg.create_texture(
                "probe face depth",
                TextureDesc::new_2d(self.depth_format, size, size),
            );
            let capture = ForwardView {
                camera: plan.camera(face),
                light: view.light,
                color,
                depth,
                clear: Some([0.0; 4]),
                occlusion: None,
                temporal: None,
                shadow_cache: None,
                virtual_shadows: None,
                lights: view.lights,
                decals: &[],
                environment: view.environment,
                reflections: Reflections::NONE,
                // The probe sees the scene lit as the view's volume lights it (the volume
                // converges through the view, not here); a screen GI sees only the view.
                gi: view.gi.and_then(|g| match g {
                    ViewGi::Probes { volume, .. } => Some(ViewGi::Probes { volume, rays: None }),
                    ViewGi::Screen(_) => None,
                }),
                features: view.features,
            };
            let setup = Setup {
                depth_desc: fg.texture_desc(depth),
                phases: 1,
                cascade_count: self.shadow_settings.cascade_count() as u32,
                shadow_draws: 0,
                capture: true,
                vsm: false,
            };
            if laid_out.is_none() {
                laid_out = Some(layout(self, ring, &capture, &setup)?);
            }
            let laid = laid_out.as_ref()?;
            // One shadow cascade around the probe, shared by its faces this frame.
            let shadow = shadow.get_or_insert_with(|| CaptureShadow {
                fit: shadow::fit_sphere(
                    plan.position,
                    plan.far,
                    view.light.direction,
                    laid.min,
                    laid.max,
                    &self.shadow_settings,
                ),
                map: Cell::new(None),
            });
            self.declare(
                fg,
                ring,
                bindless,
                geometry,
                materials,
                &capture,
                &setup,
                laid,
                &capture_imported,
                Some(shadow),
            )?;
            probes.add_face_pass(fg, bindless, textures, &environment, color, depth, face);
        }
        if plan.finishes() {
            probes.add_prefilter_passes(fg, bindless, textures, plan.slot);
        }
        probes.commit(&plan);
        Some(())
    }

    /// Subsurface materials' pipelines, when they get pipeline states of their own:
    /// scattering on and profiles set. Otherwise they shade as ordinary ones.
    fn subsurface_states(&self) -> Option<&SubsurfacePipelines> {
        self.subsurface_pipelines
            .as_ref()
            .filter(|_| self.subsurface_settings.is_some() && !self.subsurface_profiles.is_empty())
    }

    /// The subsurface pipelines when `view` scatters (ADR 0099 step 4): without them its
    /// subsurface materials shade as ordinary ones.
    fn subsurface_for(&self, view: &ForwardView<'_>) -> Option<&SubsurfacePipelines> {
        self.subsurface_states()
            .filter(|_| view.features.subsurface)
    }

    /// A hash of the settings that change what `view`'s cached cascades draw (ADR 0093):
    /// another one draws them whole.
    fn caster_settings(&self, view: &ForwardView<'_>) -> u64 {
        let hash = cascade_cache::mix(0, u64::from(self.subsurface_for(view).is_some()));
        cascade_cache::mix(hash, u64::from(self.lod_threshold.to_bits()))
    }

    /// Notes what a scene's view with cached cascades or virtual shadow pages (ADR 0093)
    /// casts from this frame: the scene's upload, its changes and moving instances, and its
    /// batches' meshes (and their shapes: another mesh or shape draws them whole).
    fn note_scene_casters(
        &self,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
        scene: &SceneFrame<'_>,
        view: &ForwardView<'_>,
    ) {
        let batches = scene.batches();
        let mut meshes = cascade_cache::mix(0, batches.len() as u64);
        for (id, batch) in batches.iter().enumerate().filter(|(_, b)| b.alive) {
            let shape = geometry.mesh(batch.mesh).map_or(u32::MAX, |m| m.revision);
            meshes = cascade_cache::mix(meshes, id as u64 | u64::from(batch.state) << 32);
            meshes = cascade_cache::mix(meshes, bytemuck::cast(batch.mesh));
            meshes = cascade_cache::mix(meshes, u64::from(shape));
        }
        let settings = self.caster_settings(view);
        if let Some(cache) = view.shadow_cache {
            cache.note_scene(
                scene.serial(),
                meshes,
                settings,
                materials.revision(),
                scene.changes(),
                scene.moving_boxes(),
            );
        }
        if let Some(pages) = view.virtual_shadows {
            pages.note_scene(
                scene.serial(),
                meshes,
                settings,
                materials.revision(),
                scene.changes(),
                scene.moving_boxes(),
            );
        }
    }

    /// What a frame's layout depends on besides the instances.
    fn setup(&self, fg: &FrameGraph<'_>, view: &ForwardView<'_>) -> Setup {
        let depth_desc = fg.texture_desc(view.depth);
        let occlusion = view.occlusion.is_some()
            && self.frustum_culling
            && depth_desc.width > 0
            && depth_desc.height > 0;
        // Marked from a single-sampled depth prepass.
        let vsm = view.virtual_shadows.is_some()
            && self.vsm_draw().is_some()
            && depth_desc.samples == vk::SampleCountFlags::TYPE_1
            && depth_desc.width > 0
            && depth_desc.height > 0
            && crate::lights::ViewAxis::new(view.camera.clip_from_world).is_some();
        Setup {
            depth_desc,
            phases: if occlusion { 2 } else { 1 },
            // The pages hold the sun's whole shadow (ADR 0093 step 3d): no cascade.
            cascade_count: if vsm {
                0
            } else {
                self.shadow_settings.cascade_count() as u32
            },
            shadow_draws: 0,
            capture: false,
            vsm,
        }
    }

    /// Plans this frame's local light shadows (ADR 0071): finds what changed, which
    /// shadowed lights are in view, their tiles and which views are drawn. Returns the
    /// number of views drawn.
    fn plan_local_shadows(
        &self,
        view: &ForwardView<'_>,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
        setup: &Setup,
        changes: Changes<'_, '_>,
    ) -> u32 {
        let Some(state) = self
            .local_shadows
            .as_ref()
            .filter(|_| self.local_shadows_on && view.features.local_shadows)
        else {
            return 0;
        };
        let mut cache = state.cache.borrow_mut();
        let revision = materials.revision();
        match changes {
            Changes::Draws(draws) => {
                let continuity = cache.begin(ChangeSource::Draws, revision);
                cache.diff_draws(
                    draws,
                    |mesh| {
                        let (center, extents) = local_box(geometry, mesh);
                        let revision = geometry.mesh(mesh).map_or(0, |m| m.revision);
                        (center, extents, revision)
                    },
                    continuity == Continuity::Broken,
                );
            }
            Changes::Scene(scene) => {
                match cache.begin(ChangeSource::Scene(scene.serial()), revision) {
                    Continuity::Same => {}
                    Continuity::Next => cache.invalidate(scene.changes(), false),
                    Continuity::Broken => cache.invalidate(&[], true),
                }
                // Instances moving between ticks move in every frame (on the GPU).
                cache.invalidate(scene.moving_boxes(), false);
            }
        }
        let mut plan = state.plan.borrow_mut();
        cache.plan(
            view.lights,
            view.camera.clip_from_world,
            [setup.depth_desc.width, setup.depth_desc.height],
            self.lod_threshold,
            &mut plan,
        );
        plan.drawn.len() as u32
    }
}

/// What a frame's layout depends on besides the instances.
#[derive(Debug, Clone, Copy)]
struct Setup {
    depth_desc: TextureDesc,
    /// Forward command lists: 2 with occlusion culling (early, late), else 1.
    phases: u32,
    cascade_count: u32,
    /// Local light shadow views drawn this frame (ADR 0071): one command list each, after
    /// the cascades'.
    shadow_draws: u32,
    /// A reflection probe's face (ADR 0073): no ambient occlusion, contact or local
    /// shadows, reflections or sky pass; one shared shadow cascade; passes named apart.
    capture: bool,
    /// The view's sun shadow comes from its virtual shadow pages (ADR 0093 steps 3b and
    /// 3d): their lists are laid out, their passes run after the prepass, and no cascade
    /// is drawn (`cascade_count` 0).
    vsm: bool,
}

/// A frame with nothing to draw: the target is the clear color everywhere, so no motion
/// shows. A temporal view still gets its (zero) motion vectors and a stored depth, and its
/// state moves on, so a resolve after it needs no special case.
fn empty_frame<'g>(fg: &mut FrameGraph<'g>, view: &ForwardView<'g>) -> ForwardPasses {
    let (color, depth) = (view.color, view.depth);
    let load = view.clear.map_or(LoadOp::Load, LoadOp::Clear);
    let depth_clear = LoadOp::Clear([1.0, 0.0, 0.0, 0.0]);
    let temporal = view.temporal;
    let depth_desc = fg.texture_desc(depth);
    let velocity = temporal.map(|_| {
        fg.create_texture(
            "motion vectors",
            TextureDesc {
                samples: depth_desc.samples,
                ..TextureDesc::new_2d(MOTION_FORMAT, depth_desc.width, depth_desc.height)
            },
        )
    });
    // MSAA (ADR 0075): the cleared depth, resolved.
    let resolved_depth = (depth_desc.samples != vk::SampleCountFlags::TYPE_1).then(|| {
        fg.create_texture(
            "depth resolved",
            TextureDesc::new_2d(depth_desc.format, depth_desc.width, depth_desc.height),
        )
    });
    let mut pass = fg.add_pass("forward").color(color, load, StoreOp::Store);
    if let Some(velocity) = velocity {
        pass = pass.color(velocity, LoadOp::Clear([0.0; 4]), StoreOp::Store);
    }
    let depth_store = if velocity.is_some() {
        StoreOp::Store
    } else {
        StoreOp::DontCare
    };
    let mut pass = pass.depth(depth, depth_clear, depth_store);
    if let Some(resolved) = resolved_depth {
        pass = pass.resolve(resolved, ResolveMode::SampleZero);
    }
    pass.execute(|_| {});
    let motion = temporal.zip(velocity).map(|(t, velocity)| {
        let jitter = t.next_jitter();
        let reset = t.reset.replace(false);
        t.previous_count.set(0);
        let previous_clip_from_world = t
            .previous_clip_from_world
            .replace(Some(view.camera.clip_from_world))
            .unwrap_or(view.camera.clip_from_world);
        t.frame.set(t.frame.get() + 1);
        MotionOutput {
            velocity,
            jitter,
            reset,
            reactive: None,
            clip_from_world: view.camera.clip_from_world,
            previous_clip_from_world,
        }
    });
    ForwardPasses {
        instances: 0,
        batches: 0,
        draw_commands: 0,
        list_commands: 0,
        occlusion: false,
        commands: None,
        motion,
        ao: None,
        atmosphere: None,
        contact_shadows: None,
        virtual_shadows: None,
        decals: None,
        gi: None,
        reflections: None,
        resolved_depth,
        draws: None,
        meshlets: None,
    }
}

/// # Safety
///
/// `cmd` is recording.
unsafe fn set_viewport(device: &engine_rhi::ash::Device, cmd: vk::CommandBuffer, area: vk::Rect2D) {
    // SAFETY: guaranteed by the caller.
    unsafe { set_draw_area(device, cmd, area.into()) }
}

/// Where draws land: their viewport and the scissor within it (one rectangle for most
/// passes; a cached cascade's pieces shift the viewport across their layer's seam and
/// draw within a scissor, ADR 0093).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrawArea {
    pub(crate) viewport: vk::Rect2D,
    pub(crate) scissor: vk::Rect2D,
}

impl From<vk::Rect2D> for DrawArea {
    fn from(area: vk::Rect2D) -> Self {
        Self {
            viewport: area,
            scissor: area,
        }
    }
}

/// Where a meshlet pass draws: one area, or the viewports and scissors set before it (a
/// cached cascade's pieces drawn at once, each instance into its piece's viewport; ADR
/// 0093).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrawAreas {
    One(DrawArea),
    Set,
}

impl From<DrawArea> for DrawAreas {
    fn from(area: DrawArea) -> Self {
        Self::One(area)
    }
}

impl From<vk::Rect2D> for DrawAreas {
    fn from(area: vk::Rect2D) -> Self {
        Self::One(area.into())
    }
}

/// Sets the viewport and scissor of `area`.
///
/// # Safety
///
/// `cmd` is recording.
pub(crate) unsafe fn set_draw_area(
    device: &engine_rhi::ash::Device,
    cmd: vk::CommandBuffer,
    area: DrawArea,
) {
    let v = area.viewport;
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_set_viewport(
            cmd,
            0,
            &[vk::Viewport {
                x: v.offset.x as f32,
                y: v.offset.y as f32,
                width: v.extent.width as f32,
                height: v.extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
        device.cmd_set_scissor(cmd, 0, &[area.scissor]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{look_at, vulkan_orthographic, vulkan_perspective};

    #[test]
    fn the_view_and_projection_come_back_from_clip_from_world() {
        let views = [
            look_at(Vec3::new(3.0, 2.0, 5.0), Vec3::ZERO, Vec3::Y),
            look_at(
                Vec3::new(-10.0, 40.0, 1.0),
                Vec3::new(2.0, 0.0, -3.0),
                Vec3::Y,
            ),
            look_at(
                Vec3::new(0.5, -2.0, -7.0),
                Vec3::new(0.0, 1.0, 0.0),
                Vec3::X,
            ),
        ];
        for view in views {
            for jitter in [(0.0, 0.0), (0.0013, -0.0021)] {
                let projection = Mat4::from_translation(Vec3::new(jitter.0, jitter.1, 0.0))
                    * vulkan_perspective(1.1, 16.0 / 9.0, 0.1, 300.0);
                let camera = Camera {
                    clip_from_world: projection * view,
                    position: view.inverse().w_axis.truncate(),
                };
                let v = camera.view_space().unwrap();
                assert!(v.view_from_world.abs_diff_eq(view, 1e-4), "{view:?}");
                // The projection rebuilt from its terms.
                let rebuilt = Mat4::from_cols(
                    Vec4::new(v.p00, 0.0, 0.0, 0.0),
                    Vec4::new(0.0, v.p11, 0.0, 0.0),
                    Vec4::new(v.p20, v.p21, v.p22, -1.0),
                    Vec4::new(0.0, 0.0, v.p32, 0.0),
                );
                assert!(
                    rebuilt.abs_diff_eq(projection, 1e-4),
                    "{rebuilt:?} {projection:?}"
                );
                // Linear depth of a point 12 units in front.
                let p = view.inverse().transform_point3(Vec3::new(0.3, -0.2, -12.0));
                let clip = camera.clip_from_world * p.extend(1.0);
                assert!((v.linear_depth(clip.z / clip.w) - 12.0).abs() < 1e-3);
            }
        }
        let ortho = Camera {
            clip_from_world: vulkan_orthographic(-1.0, 1.0, -1.0, 1.0, 0.1, 10.0) * views[0],
            position: Vec3::ZERO,
        };
        assert!(ortho.view_space().is_none());
        let scaled = Camera {
            clip_from_world: vulkan_perspective(1.0, 1.0, 0.1, 10.0)
                * Mat4::from_scale(Vec3::splat(2.0))
                * views[0],
            position: Vec3::ZERO,
        };
        assert!(scaled.view_space().is_none());
    }
}
