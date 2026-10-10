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
//!    commands; a depth-only pass draws each cascade into its tile of a transient atlas
//!    (with depth bias); masked materials test their alpha;
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
use crate::environment::{Environment, EnvironmentFrame, PREFILTERED_LEVELS};
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
use crate::material::{MaterialFrame, MaterialId, MaterialKind};
use crate::mesh::{GeometryFrame, MeshId};
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
    Allocation, Allocator, Bindless, Blend, DepthBias, Device, GraphicsPipelineDesc,
    MemoryLocation, Pipeline, PipelineDesc, RhiError, SamplerKind, ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.vertexMain.spv");
const SHADOW_VERTEX: &[u8] = include_bytes!("../shaders/forward.shadowVertex.spv");
const SHADOW_VERTEX_MASKED: &[u8] = include_bytes!("../shaders/forward.shadowVertexMasked.spv");
const SHADOW_FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.shadowFragmentMasked.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.fragmentMain.spv");
const FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.fragmentMasked.spv");
const DEPTH_VERTEX: &[u8] = include_bytes!("../shaders/forward.depthVertex.spv");
const FRAGMENT_MOTION: &[u8] = include_bytes!("../shaders/forward.fragmentMainMotion.spv");
const FRAGMENT_AO: &[u8] = include_bytes!("../shaders/forward.fragmentMainAo.spv");
const FRAGMENT_MASKED_MOTION: &[u8] = include_bytes!("../shaders/forward.fragmentMaskedMotion.spv");
const BACKGROUND_VERTEX: &[u8] = include_bytes!("../shaders/forward.backgroundVertex.spv");
const BACKGROUND_MOTION: &[u8] = include_bytes!("../shaders/forward.backgroundMotion.spv");
const SKY_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.skyFragment.spv");
const FRAGMENT_SUBSURFACE: &[u8] = include_bytes!("../shaders/forward.fragmentSubsurface.spv");
const SCATTER_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.scatterFragment.spv");
// Local lights' shadows (ADR 0071): culling and drawing the views, and the shading that
// looks them up.
const CULL_SHADOW_VIEWS: &[u8] = include_bytes!("../shaders/forward.cullShadowViews.spv");
const LOCAL_SHADOW_VERTEX: &[u8] = include_bytes!("../shaders/forward.localShadowVertex.spv");
const LOCAL_SHADOW_VERTEX_MASKED: &[u8] =
    include_bytes!("../shaders/forward.localShadowVertexMasked.spv");
const FRAGMENT_SHADOWED: &[u8] = include_bytes!("../shaders/forward.fragmentMainShadowed.spv");
const FRAGMENT_MASKED_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentMaskedShadowed.spv");
const FRAGMENT_MOTION_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentMainMotionShadowed.spv");
const FRAGMENT_MASKED_MOTION_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentMaskedMotionShadowed.spv");
const FRAGMENT_BLENDED_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentBlendedShadowed.spv");
const FRAGMENT_BLENDED_REACTIVE_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentBlendedReactiveShadowed.spv");
const FRAGMENT_BLENDED_ATMOSPHERE_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentBlendedAtmosphereShadowed.spv");
const FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentBlendedAtmosphereReactiveShadowed.spv");
const FRAGMENT_AO_SHADOWED: &[u8] = include_bytes!("../shaders/forward.fragmentMainAoShadowed.spv");
const FRAGMENT_SUBSURFACE_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentSubsurfaceShadowed.spv");
// The sun's contact shadows (ADR 0071): shading after the prepass that reads their mask.
const FRAGMENT_CONTACT: &[u8] = include_bytes!("../shaders/forward.fragmentMainContact.spv");
const FRAGMENT_AO_CONTACT: &[u8] = include_bytes!("../shaders/forward.fragmentMainAoContact.spv");
const FRAGMENT_CONTACT_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentMainContactShadowed.spv");
const FRAGMENT_AO_CONTACT_SHADOWED: &[u8] =
    include_bytes!("../shaders/forward.fragmentMainAoContactShadowed.spv");

#[path = "forward_msaa.rs"]
mod msaa;

pub use crate::contact::ContactShadowSettings;
pub use crate::lights::{LocalLight, Spot};
pub use crate::local_shadows::{LocalShadowSettings, LocalShadowStats};
pub use crate::reflections::Reflections;

// Reflections (ADR 0073): the shading of views with them (always after the depth prepass,
// which writes the motion vectors), by `(ambient occlusion × 2 + contact shadows) × 2 +
// local shadows`.
const REFLECT_FRAGMENTS: [(&[u8], &core::ffi::CStr); 8] = [
    (
        include_bytes!("../shaders/forward.fragmentReflect.spv"),
        c"fragmentReflect",
    ),
    (
        include_bytes!("../shaders/forward.fragmentReflectShadowed.spv"),
        c"fragmentReflectShadowed",
    ),
    (
        include_bytes!("../shaders/forward.fragmentReflectContact.spv"),
        c"fragmentReflectContact",
    ),
    (
        include_bytes!("../shaders/forward.fragmentReflectContactShadowed.spv"),
        c"fragmentReflectContactShadowed",
    ),
    (
        include_bytes!("../shaders/forward.fragmentReflectAo.spv"),
        c"fragmentReflectAo",
    ),
    (
        include_bytes!("../shaders/forward.fragmentReflectAoShadowed.spv"),
        c"fragmentReflectAoShadowed",
    ),
    (
        include_bytes!("../shaders/forward.fragmentReflectAoContact.spv"),
        c"fragmentReflectAoContact",
    ),
    (
        include_bytes!("../shaders/forward.fragmentReflectAoContactShadowed.spv"),
        c"fragmentReflectAoContactShadowed",
    ),
];
pub use crate::shadow::ShadowSettings;

/// Format of the motion vector target (ADR 0043).
pub const MOTION_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;
pub(crate) const DEPTH_VERTEX_MASKED: &[u8] =
    include_bytes!("../shaders/forward.depthVertexMasked.spv");
/// The depth prepass that also writes motion vectors: opaque, masked (queue item 0d).
pub(crate) const DEPTH_VERTEX_MOTION: &[u8] =
    include_bytes!("../shaders/forward.depthVertexMotion.spv");
pub(crate) const PREPASS_MOTION: &[u8] = include_bytes!("../shaders/forward.prepassMotion.spv");
pub(crate) const DEPTH_VERTEX_MASKED_MOTION: &[u8] =
    include_bytes!("../shaders/forward.depthVertexMaskedMotion.spv");
const PREPASS_MASKED_MOTION: &[u8] = include_bytes!("../shaders/forward.prepassMaskedMotion.spv");
const CULL: &[u8] = include_bytes!("../shaders/forward.cullMain.spv");
const FRAGMENT_BLENDED: &[u8] = include_bytes!("../shaders/forward.fragmentBlended.spv");
const FRAGMENT_BLENDED_REACTIVE: &[u8] =
    include_bytes!("../shaders/forward.fragmentBlendedReactive.spv");
const FRAGMENT_BLENDED_ATMOSPHERE: &[u8] =
    include_bytes!("../shaders/forward.fragmentBlendedAtmosphere.spv");
const FRAGMENT_BLENDED_ATMOSPHERE_REACTIVE: &[u8] =
    include_bytes!("../shaders/forward.fragmentBlendedAtmosphereReactive.spv");

/// Format of the reactive mask the transparent pass writes for temporal upsampling: how much
/// of each pixel transparent surfaces cover (their combined alpha).
pub const REACTIVE_FORMAT: vk::Format = vk::Format::R8_UNORM;
const LIGHT_TILES: &[u8] = include_bytes!("../shaders/forward.lightTiles.spv");

/// `PHASE_*` in `forward.slang`.
const PHASE_ALL: u32 = 0;
const PHASE_EARLY: u32 = 1;
const PHASE_LATE: u32 = 2;
const PHASE_SHADOW: u32 = 3;

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
const MASKED: usize = 4;
const DOUBLE_SIDED: usize = 2;
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

/// `Cascade` in `forward.slang` (64 bytes).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct CascadeData {
    scale: [f32; 4],
    offset: [f32; 4],
    atlas: [f32; 4],
    bounds: [f32; 4],
}

/// `Frame` in `forward.slang` (1056 bytes).
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
    _pad2: [u32; 2],
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
    _pad4: [u32; 2],
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
    _pad5: u32,
    /// ... per sorted light its first view (or `NO_SHADOW`), the views, the drawn views
    /// and their count, and one texel of the atlas in uv.
    light_shadows: u64,
    shadow_views: u64,
    shadow_draws: u64,
    shadow_draw_count: u32,
    shadow_texel: f32,
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

/// `Push` in `forward.slang` (112 bytes).
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
}

/// `NO_TEXTURE` in `forward.slang`: no ambient occlusion.
const NO_AO: u32 = u32::MAX;

const _: () = assert!(core::mem::size_of::<CascadeData>() == 64);
const _: () = assert!(core::mem::size_of::<FrameData>() == 1056);
const _: () = assert!(core::mem::size_of::<InstanceData>() == 64);
const _: () = assert!(core::mem::size_of::<BatchData>() == 64);
const _: () = assert!(crate::mesh::MAX_LODS == 4);
const _: () = assert!(core::mem::size_of::<Push>() == 112);
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
    /// The view's ambient occlusion ([`crate::ao::AO_FORMAT`], sampled by the forward
    /// pass), when it is on: for debugging views and effects of your own.
    pub ao: Option<Texture>,
    /// The environment's atmosphere tables this frame, when it has a physical sky (ADR
    /// 0069): for debugging views and tests.
    pub atmosphere: Option<AtmosphereTextures>,
    /// The sun's contact shadows ([`crate::contact::CONTACT_FORMAT`], sampled by the
    /// forward pass), when they are on (ADR 0071): for debugging views and tests.
    pub contact_shadows: Option<Texture>,
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

/// The shading pipelines, compiled with or without local lights.
#[derive(Debug)]
struct Shading {
    /// Forward shading: opaque, masked.
    main: [Pipeline; 2],
    /// Forward shading after the depth prepass: depth tested for equality, not written;
    /// no alpha test (the prepass did it).
    shade: Pipeline,
    /// Forward shading that also writes motion vectors, for views without the depth
    /// prepass (after it the prepass writes them): opaque, masked.
    main_motion: [Pipeline; 2],
    /// Transparent surfaces: premultiplied blending, depth tested but not written; without
    /// and with the reactive mask.
    blended: [Pipeline; 2],
}

/// Ambient occlusion (ADR 0058): its passes, and the shading that reads it after the depth
/// prepass (`fragmentMainAo`), per shading variant. The other entry points have no code
/// for it (see `shade` in `forward.slang`).
#[derive(Debug)]
struct AoPasses {
    passes: AmbientOcclusion,
    shading: [Pipeline; 4],
}

/// The specialization constants of each shading variant: `LOCAL_LIGHTS`, `ENVIRONMENT`
/// (`forward.slang`).
const VARIANTS: [[u32; 2]; 4] = [[0, 0], [1, 0], [0, 1], [1, 1]];

/// A shading pipeline of the forward pass.
fn shading_desc<'a>(
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
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
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
struct SubsurfacePipelines {
    /// Per shading variant, their shading after the prepass (also writing their diffuse
    /// light and albedo).
    shading: [Pipeline; 4],
    /// The scatter pass: the subsurface batches again, adding where their depth is.
    scatter: Pipeline,
}

impl SubsurfacePipelines {
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
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
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
struct ShadowedPipelines {
    shading: [Shading; 2],
    /// Transparent surfaces under an atmosphere or fog: without and with the reactive mask.
    blended_atmosphere: [Pipeline; 2],
    /// With ambient occlusion, per environment bit.
    ao: Option<[Pipeline; 2]>,
    /// Subsurface materials, per environment bit.
    subsurface: Option<[Pipeline; 2]>,
    /// The shadow views: opaque (depth only), masked.
    depth: [Pipeline; 2],
    cull: ComputeShader<Push>,
}

impl ShadowedPipelines {
    fn new(
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
        descs.push(shadow(&shadow_vertex, c"localShadowVertex", None));
        descs.push(shadow(
            &shadow_vertex_masked,
            c"localShadowVertexMasked",
            Some(masked_depth),
        ));
        let cull = ComputeSource::<Push>::new(CULL_SHADOW_VIEWS, c"cullShadowViews", &[])?;
        let mut all: Vec<PipelineDesc<'_>> =
            descs.iter().copied().map(PipelineDesc::Graphics).collect();
        all.push(cull.desc());
        let mut pipelines =
            Pipeline::create_many(device, bindless.pipeline_layout(), &all)?.into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "local shadow pipelines (fewer than described)",
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
        let shading = [shading()?, shading()?];
        let blended_atmosphere = [next()?, next()?];
        let depth = [next()?, next()?];
        let cull = cull.finish(next()?);
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
    fn ensure_extras(
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
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
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
struct ContactState {
    passes: ContactShadows,
    /// Per shading variant.
    shading: [Pipeline; 4],
    ao: Option<[Pipeline; 4]>,
    /// With local lights' shadows, per environment bit (the local-light variants).
    shadowed: Option<[Pipeline; 2]>,
    shadowed_ao: Option<[Pipeline; 2]>,
}

/// Shading pipelines after the depth prepass (depth tested for equality, not written) of
/// `variants`.
fn prepass_shading(
    device: &Device,
    bindless: &Bindless,
    color_format: vk::Format,
    depth_format: vk::Format,
    fragment: (&[u8], &core::ffi::CStr),
    variants: &[usize],
) -> Result<Vec<Pipeline>, RhiError> {
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
    Pipeline::create_many(device, bindless.pipeline_layout(), &descs)
}

/// The first four (or two) of `pipelines` as an array.
fn array_of<const N: usize>(pipelines: Vec<Pipeline>) -> Result<[Pipeline; N], RhiError> {
    pipelines.try_into().map_err(|_| RhiError::Vulkan {
        call: "contact shadow pipelines (fewer than described)",
        result: vk::Result::ERROR_UNKNOWN,
    })
}

impl ContactState {
    fn new(
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
    fn ensure_extras(
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
fn shadowed_index(variant: usize) -> usize {
    variant >> 1
}

/// The atlas of local lights' shadows: a depth image that lives across frames.
#[derive(Debug)]
struct AtlasImage {
    image: vk::Image,
    allocation: Option<Allocation>,
    view: vk::ImageView,
    sampled: u32,
    desc: TextureDesc,
    state: Cell<crate::graph::TextureState>,
}

impl AtlasImage {
    fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        settings: &LocalShadowSettings,
    ) -> Result<Self, RhiError> {
        let size = settings.atlas_size;
        let (image, allocation) = allocator.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(settings.format)
                .extent(vk::Extent3D {
                    width: size,
                    height: size,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            MemoryLocation::Device,
        )?;
        // SAFETY: a view of the image just created.
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(settings.format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::DEPTH,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        };
        let view = match view {
            Ok(view) => view,
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { allocator.destroy_image(image, allocation) };
                return Err(RhiError::vk("vkCreateImageView")(e));
            }
        };
        let Some(sampled) = bindless.add_sampled_image(view) else {
            // SAFETY: never used by the GPU.
            unsafe {
                device.raw().destroy_image_view(view, None);
                allocator.destroy_image(image, allocation);
            }
            return Err(RhiError::Vulkan {
                call: "bindless set full",
                result: vk::Result::ERROR_OUT_OF_POOL_MEMORY,
            });
        };
        Ok(Self {
            image,
            allocation: Some(allocation),
            view,
            sampled,
            desc: TextureDesc::new_2d(settings.format, size, size),
            state: Cell::new(crate::graph::TextureState::undefined()),
        })
    }

    /// # Safety
    ///
    /// The GPU no longer uses the image.
    unsafe fn destroy(&mut self, device: &Device, allocator: &mut Allocator, bindless: &Bindless) {
        if let Some(allocation) = self.allocation.take() {
            bindless.remove_sampled_image(self.sampled, 0);
            // SAFETY: guaranteed by the caller.
            unsafe {
                device.raw().destroy_image_view(self.view, None);
                allocator.destroy_image(self.image, allocation);
            }
        }
    }
}

/// Local lights' shadows (ADR 0071): the atlas, its cache and the pipelines, made when the
/// feature is first turned on.
#[derive(Debug)]
struct LocalShadowState {
    atlas: AtlasImage,
    pipelines: ShadowedPipelines,
    cache: RefCell<ShadowCache>,
    plan: RefCell<Plan>,
    /// The frame (graph serial) the atlas was last imported into, and its handle there.
    imported: Cell<Option<(u64, Texture)>>,
    /// The tiles drawn in that frame, by every view (each view's pass records its range).
    rects: RefCell<Vec<vk::Rect2D>>,
}

/// Reflections (ADR 0073): their passes, and the shading of views with them after the
/// depth prepass, which also writes the normal, roughness and specular weight. By
/// `[lights][ambient occlusion][contact shadows][local shadows]` (local shadows only with
/// local lights).
#[derive(Debug)]
struct ReflectionShading {
    passes: ReflectionPasses,
    shading: ReflectShadingTable,
}

/// [`ReflectionShading::shading`]'s table.
type ReflectShadingTable = [[[[Option<Pipeline>; 2]; 2]; 2]; 2];

impl ReflectionShading {
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
        let pipelines = Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?;
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
    fn pipeline(&self, lights: bool, ao: bool, contact: bool, shadowed: bool) -> Option<&Pipeline> {
        self.shading[usize::from(lights)][usize::from(ao)][usize::from(contact)]
            [usize::from(shadowed && lights)]
        .as_ref()
    }
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
    composite: Pipeline,
    /// Transparent surfaces under an atmosphere or fog (the environment variants, without
    /// and with local lights): without and with the reactive mask.
    blended_atmosphere: [[Pipeline; 2]; 2],
    /// Shadow map: opaque (depth only), masked.
    shadow: [Pipeline; 2],
    cull: ComputeShader<Push>,
    /// The tiles' light bitmasks.
    light_tiles: ComputeShader<Push>,
    pyramid: DepthPyramid,
    shadow_settings: ShadowSettings,
    frustum_culling: bool,
    light_culling: bool,
    /// Screen-space error (pixels) a coarser detail level may have; 0 draws full meshes.
    lod_threshold: f32,
    depth_prepass: bool,
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
    /// Pipelines for multisampled targets (MSAA, ADR 0075): 2, 4, 8 samples, made by
    /// [`Self::set_samples`].
    msaa: [Option<Box<msaa::MsaaPipelines>>; 3],
    /// How a multisampled depth is resolved for what reads it by pixel (the nearest
    /// sample), and for culling's pyramid (the farthest: conservative); sample 0 where
    /// the device has no min / max resolve.
    depth_resolves: [ResolveMode; 2],
    scratch: RefCell<Scratch>,
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
        let mut descs = Vec::with_capacity(7 * VARIANTS.len() + 13);
        for lit in 0..VARIANTS.len() {
            descs.extend([
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
        // The atmosphere's full-screen pass (blended premultiplied: light + target ×
        // transmittance; it samples the depth) and the transparent surfaces under it, in
        // the environment variants only.
        let composite_vertex = spirv_words(COMPOSITE_VERTEX).ok_or_else(invalid)?;
        let composite_fragment = spirv_words(COMPOSITE_FRAGMENT).ok_or_else(invalid)?;
        descs.push(GraphicsPipelineDesc {
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
            descs.push(blended(
                &fragment_blended_atmosphere,
                c"fragmentBlendedAtmosphere",
                &formats,
                lit,
            ));
            descs.push(blended(
                &fragment_blended_atmosphere_reactive,
                c"fragmentBlendedAtmosphereReactive",
                &reactive_formats,
                lit,
            ));
        }
        // The compute passes' shaders, created with the rest.
        let cull = ComputeSource::<Push>::new(CULL, c"cullMain", &[])?;
        let light_tiles = ComputeSource::<Push>::new(LIGHT_TILES, c"lightTiles", &[])?;
        let reduce = DepthPyramid::source()?;
        let mut all: Vec<PipelineDesc<'_>> =
            descs.iter().copied().map(PipelineDesc::Graphics).collect();
        all.extend([cull.desc(), light_tiles.desc(), reduce.desc()]);
        let layout = bindless.pipeline_layout();
        let mut pipelines = Pipeline::create_many(device, layout, &all)?.into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "forward pipelines (fewer than described)",
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
        let background = next()?;
        let sky = next()?;
        let prepass = [next()?, next()?];
        let prepass_motion = [next()?, next()?];
        let shadow = [next()?, next()?];
        let composite = next()?;
        let blended_atmosphere = [[next()?, next()?], [next()?, next()?]];
        Ok(Self {
            shading,
            prepass,
            prepass_motion,
            background,
            sky,
            composite,
            blended_atmosphere,
            shadow,
            cull: cull.finish(next()?),
            light_tiles: light_tiles.finish(next()?),
            pyramid: DepthPyramid::from_source(&reduce, next()?),
            shadow_settings,
            frustum_culling: true,
            light_culling: true,
            lod_threshold: 1.0,
            depth_prepass: false,
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
        Ok(())
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
        }
        self.reflections_on = enabled;
        Ok(())
    }

    /// Whether local reflections are on.
    pub fn reflections(&self) -> bool {
        self.reflections_on
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
                        .map(|p| p.statistics(device))
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

    /// The driver's statistics of the opaque forward pipelines that look up local lights'
    /// shadows: without and with an environment. Empty unless local shadows were turned on
    /// and the device captures statistics.
    pub fn shadowed_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 2] {
        match &self.local_shadows {
            Some(s) => core::array::from_fn(|k| s.pipelines.shading[k].main[0].statistics(device)),
            None => [Vec::new(), Vec::new()],
        }
    }

    /// The driver's statistics of the shading pipelines that read the sun's contact
    /// shadows (after the prepass), per shading variant. Empty unless contact shadows were
    /// turned on and the device captures statistics.
    pub fn contact_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        match &self.contact {
            Some(c) => core::array::from_fn(|k| c.shading[k].statistics(device)),
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
        core::array::from_fn(|k| self.shading[k].main[0].statistics(device))
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
                self.shading[k].main_motion[0].statistics(device),
                self.ao
                    .as_ref()
                    .map(|ao| ao.shading[k].statistics(device))
                    .unwrap_or_default(),
            ]
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
                0..4 => m.shading[k].shade.statistics(device),
                _ => m.composite.statistics(device),
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
            self.composite.statistics(device),
            self.blended_atmosphere[0][0].statistics(device),
            self.blended_atmosphere[1][0].statistics(device),
        ]
    }

    /// Turns light culling on (the default) or off. Off, every pixel evaluates every light
    /// that reaches the view: the reference for tests and comparisons (ADR 0045).
    pub fn set_light_culling(&mut self, enabled: bool) {
        self.light_culling = enabled;
    }

    /// How far (in pixels on screen) a mesh's coarser detail level may stray from the full
    /// mesh to be drawn instead (default 1; ADR 0053). 0 always draws the full meshes.
    /// Orthographic views always draw the full meshes.
    pub fn set_lod_threshold(&mut self, pixels: f32) {
        self.lod_threshold = pixels.max(0.0);
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
        if let Some((probes, textures)) = imported.probes {
            // What moved, appeared or went away in the scene asks the probes that see it
            // for a new capture.
            probes.note_changes(scene.changes());
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
                lights: view.lights,
                environment: view.environment,
                reflections: Reflections::NONE,
            };
            let setup = Setup {
                depth_desc: fg.texture_desc(depth),
                phases: 1,
                cascade_count: self.shadow_settings.cascade_count() as u32,
                shadow_draws: 0,
                capture: true,
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

    /// What a frame's layout depends on besides the instances.
    fn setup(&self, fg: &FrameGraph<'_>, view: &ForwardView<'_>) -> Setup {
        let depth_desc = fg.texture_desc(view.depth);
        let occlusion = view.occlusion.is_some()
            && self.frustum_culling
            && depth_desc.width > 0
            && depth_desc.height > 0;
        Setup {
            depth_desc,
            phases: if occlusion { 2 } else { 1 },
            cascade_count: self.shadow_settings.cascade_count() as u32,
            shadow_draws: 0,
            capture: false,
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
            .filter(|_| self.local_shadows_on)
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

    /// Lays out a draw list: instances, batches (runs of consecutive draws) and draw
    /// commands, all written into `ring`.
    #[allow(clippy::too_many_lines)]
    fn layout_draws(
        &self,
        ring: &mut UploadRing,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
        draws: &[DrawItem],
        view: &ForwardView<'_>,
        setup: &Setup,
    ) -> Option<Layout> {
        let Setup {
            phases,
            cascade_count,
            shadow_draws,
            ..
        } = *setup;
        let lists = phases + cascade_count + shadow_draws;
        let instance_count = draws.len() as u32;
        let mut scratch = self.scratch.borrow_mut();
        let Scratch {
            slots,
            runs,
            keys,
            commands: command_scratch,
            ..
        } = &mut *scratch;
        // Batches in draw order, counted per pipeline state, then ordered by state and,
        // within a state, front to back (by the first instance's box), so the depth test
        // rejects hidden fragments before shading.
        let mut counts = [0u32; STATES];
        // Subsurface materials get a pipeline state of their own while scattering is on
        // and there are profiles; otherwise they shade as ordinary ones.
        let subsurface_on = self.subsurface_states().is_some();
        batches(draws, materials, slots, runs, &mut counts, subsurface_on);
        let subsurface_draws = counts[SUBSURFACE..SUBSURFACE + 8].iter().any(|&c| c > 0);
        let mut starts = [0u32; STATES];
        let mut next = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = next;
            next += count;
        }
        keys.clear();
        keys.extend(runs.iter().enumerate().map(|(k, run)| {
            let draw = &draws[run.first as usize];
            let (center, extents) = local_box(geometry, draw.mesh);
            let (center, _) = crate::cull::transform_box(&draw.world_from_object, center, extents);
            let distance = center.distance(view.camera.position).max(0.0).to_bits();
            // Opaque batches front to back (the depth test rejects hidden fragments early),
            // blended ones back to front (each blends over what is behind it).
            let distance = if run.state & BLENDED != 0 {
                !distance
            } else {
                distance
            };
            (((run.state as u64) << 32) | u64::from(distance), k as u32)
        }));
        keys.sort_unstable();
        for (order, &(_, k)) in keys.iter().enumerate() {
            runs[k as usize].order = order as u32;
        }
        let batch_count = runs.len() as u32;
        // Draw commands: one per detail level of each batch's mesh, a batch's levels
        // together and the batches in state order (so each state stays one indirect draw).
        // Each command has its own range of instance slots, in draw order (without LODs a
        // draw's slot is its index, as before LODs).
        counts = [0; STATES];
        let mut command_count = 0u32;
        for &(_, k) in keys.iter() {
            let run = &mut runs[k as usize];
            run.lods = geometry
                .mesh(draws[run.first as usize].mesh)
                .map_or(1, |m| m.lod_count.clamp(1, crate::mesh::MAX_LODS as u32));
            run.command = command_count;
            command_count += run.lods;
            counts[run.state] += run.lods;
        }
        let mut slot_count = 0u32;
        for run in runs.iter_mut() {
            run.slot = slot_count;
            slot_count += run.lods * run.len;
        }
        let mut next = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = next;
            next += count;
        }
        // Instances in draw order, and the scene's bounding box for the light on the way.
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        let instances = {
            let (out, staged) = ring.alloc::<InstanceData>(draws.len())?;
            for run in runs.iter() {
                let (center, extents) = local_box(geometry, draws[run.first as usize].mesh);
                let range = run.first as usize..(run.first + run.len) as usize;
                let items = draws[range.clone()].iter().zip(&slots[range.clone()]);
                for ((item, &material), slot) in items.zip(&mut out[range]) {
                    let (world_center, world_extents) =
                        crate::cull::transform_box(&item.world_from_object, center, extents);
                    min = min.min(world_center - world_extents);
                    max = max.max(world_center + world_extents);
                    let rows = item.world_from_object.transpose();
                    slot.write(InstanceData {
                        rows: [
                            rows.x_axis.to_array(),
                            rows.y_axis.to_array(),
                            rows.z_axis.to_array(),
                        ],
                        material,
                        batch: run.order,
                        _pad: [0; 2],
                    });
                }
            }
            staged
        };
        let instances_staged = instances;
        let instances = instances.address;
        // The ring's memory takes writes best in order (it may be write-combined device
        // memory): the batch table and the command lists are written front to back.
        let batch_table = {
            let (out, staged) = ring.alloc::<BatchData>(batch_count as usize)?;
            for &(_, k) in keys.iter() {
                let run = &runs[k as usize];
                let mesh = draws[run.first as usize].mesh;
                let (center, extents) = local_box(geometry, mesh);
                let lods = geometry.mesh(mesh).map(|m| m.lods).unwrap_or_default();
                out[run.order as usize].write(BatchData {
                    command: run.command,
                    slot: run.slot,
                    count: run.len,
                    lods: run.lods | u32::from(run.state & BLENDED != 0) << 8,
                    center: center.extend(0.0).to_array(),
                    extents: extents.extend(0.0).to_array(),
                    errors: lods.map(|l| l.error),
                });
            }
            staged.address
        };
        // Draw commands, in state order: the forward pass's for each phase, then the shadow
        // pass's for each cascade, with no instances until culling counts them (each list's
        // slots follow the previous list's).
        let commands = {
            let per_list = command_count as usize;
            let (out, staged) =
                ring.alloc::<vk::DrawIndexedIndirectCommand>(lists as usize * per_list)?;
            // One list's commands, in command order (the sorted batches' levels)...
            command_scratch.clear();
            for &(_, k) in keys.iter() {
                let run = &runs[k as usize];
                // A removed mesh draws nothing.
                let mesh = geometry.mesh(draws[run.first as usize].mesh);
                for lod in 0..run.lods {
                    let level = mesh.map(|m| m.lods[lod as usize]).unwrap_or_default();
                    command_scratch.push(vk::DrawIndexedIndirectCommand {
                        index_count: level.index_count,
                        instance_count: 0,
                        first_index: level.first_index,
                        vertex_offset: mesh.map_or(0, |m| m.first_vertex as i32),
                        first_instance: run.slot + lod * run.len,
                    });
                }
            }
            // ... then copied into each list, which has its own slot region.
            for list in 0..lists {
                let slots = list_offset(list, phases, cascade_count) * slot_count;
                let out = &mut out[list as usize * per_list..(list as usize + 1) * per_list];
                for (out, command) in out.iter_mut().zip(command_scratch.iter()) {
                    out.write(vk::DrawIndexedIndirectCommand {
                        first_instance: command.first_instance + slots,
                        ..*command
                    });
                }
            }
            staged
        };
        drop(scratch);
        Some(Layout {
            instance_count,
            instances,
            source: Source::Ring(instances_staged),
            batch_table,
            commands,
            command_count,
            drawn_commands: command_count,
            slot_count,
            batch_count,
            states: States { starts, counts },
            min,
            max,
            subsurface_draws,
        })
    }

    /// Lays out a persistent scene: its batches and their draw commands, written into
    /// `ring`, with the sink batch's command and slots after them.
    #[allow(clippy::too_many_lines)]
    fn layout_scene(
        &self,
        ring: &mut UploadRing,
        geometry: &GeometryFrame<'_>,
        scene: &SceneFrame<'_>,
        view: &ForwardView<'_>,
        setup: &Setup,
    ) -> Option<Layout> {
        let Setup {
            phases,
            cascade_count,
            shadow_draws,
            ..
        } = *setup;
        let lists = phases + cascade_count + shadow_draws;
        let mut scratch = self.scratch.borrow_mut();
        let Scratch {
            runs,
            keys,
            commands: command_scratch,
            ..
        } = &mut *scratch;
        let subsurface_on = self.subsurface_states().is_some();
        let batches = scene.batches();
        // One run per live batch (`first` holds its id), counted per pipeline state.
        let mut counts = [0u32; STATES];
        runs.clear();
        for (id, batch) in batches.iter().enumerate().skip(1) {
            if !batch.alive || batch.count == 0 {
                continue;
            }
            let mut state = batch.state as usize;
            if !subsurface_on {
                state &= !SUBSURFACE;
            }
            runs.push(Run {
                first: id as u32,
                len: batch.count,
                state,
                order: 0,
                command: 0,
                slot: 0,
                lods: 1,
            });
            counts[state] += 1;
        }
        let subsurface_draws = counts[SUBSURFACE..SUBSURFACE + 8].iter().any(|&c| c > 0);
        // Ordered by state, then front to back (blended: back to front) by the batch's
        // center, as the draw-list path orders its runs.
        keys.clear();
        keys.extend(runs.iter().enumerate().map(|(k, run)| {
            let center = scene.batch_center(run.first);
            let distance = center.distance(view.camera.position).max(0.0).to_bits();
            let distance = if run.state & BLENDED != 0 {
                !distance
            } else {
                distance
            };
            (((run.state as u64) << 32) | u64::from(distance), k as u32)
        }));
        keys.sort_unstable();
        for (order, &(_, k)) in keys.iter().enumerate() {
            runs[k as usize].order = order as u32;
        }
        let batch_count = runs.len() as u32;
        counts = [0; STATES];
        let mut command_count = 0u32;
        for &(_, k) in keys.iter() {
            let run = &mut runs[k as usize];
            run.lods = geometry
                .mesh(batches[run.first as usize].mesh)
                .map_or(1, |m| m.lod_count.clamp(1, crate::mesh::MAX_LODS as u32));
            run.command = command_count;
            command_count += run.lods;
            counts[run.state] += run.lods;
        }
        let mut slot_count = 0u32;
        for run in runs.iter_mut() {
            run.slot = slot_count;
            slot_count += run.lods * run.len;
        }
        let drawn_commands = command_count;
        // The sink: its command follows the drawn ones in every list (no state draws it),
        // its slots follow theirs; culling appends holes there when it keeps them.
        let holes = scene.holes();
        let sink_command = command_count;
        let sink_slot = slot_count;
        command_count += 1;
        slot_count += holes;
        let mut starts = [0u32; STATES];
        let mut next = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = next;
            next += count;
        }
        // The batch table, indexed by batch id: what the instances' `batch` fields name.
        let batch_table = {
            let (out, staged) = ring.alloc::<BatchData>(batches.len())?;
            // The sink's entry (id 0, named by the holes), and dead batches' (named by
            // no instance): blended (no shadows, nothing early), one level, and an
            // inside-out box (negative extents) that fails every frustum plane, so holes
            // are culled without touching the sink's counter. With culling off they are
            // appended to the sink's command, which no state draws.
            let empty = BatchData {
                command: sink_command,
                slot: sink_slot,
                count: holes,
                lods: 1 | 1 << 8,
                center: [0.0; 4],
                extents: [-1e30, -1e30, -1e30, 0.0],
                errors: [0.0; 4],
            };
            for out in out.iter_mut() {
                out.write(empty);
            }
            for run in runs.iter() {
                let mesh = batches[run.first as usize].mesh;
                let (center, extents) = local_box(geometry, mesh);
                let lods = geometry.mesh(mesh).map(|m| m.lods).unwrap_or_default();
                out[run.first as usize].write(BatchData {
                    command: run.command,
                    slot: run.slot,
                    count: run.len,
                    lods: run.lods | u32::from(run.state & BLENDED != 0) << 8,
                    center: center.extend(0.0).to_array(),
                    extents: extents.extend(0.0).to_array(),
                    errors: lods.map(|l| l.error),
                });
            }
            staged.address
        };
        let commands = {
            let per_list = command_count as usize;
            let (out, staged) =
                ring.alloc::<vk::DrawIndexedIndirectCommand>(lists as usize * per_list)?;
            command_scratch.clear();
            for &(_, k) in keys.iter() {
                let run = &runs[k as usize];
                // A removed mesh draws nothing.
                let mesh = geometry.mesh(batches[run.first as usize].mesh);
                for lod in 0..run.lods {
                    let level = mesh.map(|m| m.lods[lod as usize]).unwrap_or_default();
                    command_scratch.push(vk::DrawIndexedIndirectCommand {
                        index_count: level.index_count,
                        instance_count: 0,
                        first_index: level.first_index,
                        vertex_offset: mesh.map_or(0, |m| m.first_vertex as i32),
                        first_instance: run.slot + lod * run.len,
                    });
                }
            }
            command_scratch.push(vk::DrawIndexedIndirectCommand {
                index_count: 0,
                instance_count: 0,
                first_index: 0,
                vertex_offset: 0,
                first_instance: sink_slot,
            });
            for list in 0..lists {
                let slots = list_offset(list, phases, cascade_count) * slot_count;
                let out = &mut out[list as usize * per_list..(list as usize + 1) * per_list];
                for (out, command) in out.iter_mut().zip(command_scratch.iter()) {
                    out.write(vk::DrawIndexedIndirectCommand {
                        first_instance: command.first_instance + slots,
                        ..*command
                    });
                }
            }
            staged
        };
        drop(scratch);
        let (min, max) = scene.bounds();
        Some(Layout {
            instance_count: scene.slot_count(),
            instances: scene.current_address,
            source: Source::Scene {
                current: scene.current,
                previous: scene.previous,
                previous_address: scene.previous_address,
            },
            batch_table,
            commands,
            command_count,
            drawn_commands,
            slot_count,
            batch_count,
            states: States { starts, counts },
            min,
            max,
            subsurface_draws,
        })
    }

    /// Declares the passes of a laid-out frame (both paths).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn declare<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        view: &ForwardView<'g>,
        setup: &Setup,
        layout: &Layout,
        imported: &Imported<'g>,
        capture_shadow: Option<&CaptureShadow>,
    ) -> Option<ForwardPasses> {
        let names = if setup.capture {
            &CAPTURE_NAMES
        } else {
            &VIEW_NAMES
        };
        let (color, depth) = (view.color, view.depth);
        let load = view.clear.map_or(LoadOp::Load, LoadOp::Clear);
        let depth_clear = LoadOp::Clear([1.0, 0.0, 0.0, 0.0]);
        let Setup {
            depth_desc,
            phases,
            cascade_count,
            shadow_draws,
            capture,
        } = *setup;
        let occlusion = view.occlusion.filter(|_| phases == 2);
        // MSAA (ADR 0075): multisampled targets draw with their sample count's pipelines,
        // always after a depth prepass, and what reads the scene by pixel (ambient
        // occlusion, contact shadows, culling, reflections, subsurface scattering) reads
        // single-sampled resolves.
        let samples = depth_desc.samples;
        let ms = if samples == vk::SampleCountFlags::TYPE_1 {
            None
        } else {
            Some(self.msaa_pipelines(samples)?)
        };
        let single = |format| TextureDesc::new_2d(format, depth_desc.width, depth_desc.height);
        let multi = |format| TextureDesc {
            samples,
            ..single(format)
        };
        let resolved_depth =
            ms.map(|_| fg.create_texture("depth resolved", single(self.depth_format)));
        let read_depth = resolved_depth.unwrap_or(depth);
        let [nearest, farthest] = self.depth_resolves;
        let Layout {
            instance_count,
            instances,
            source,
            batch_table,
            commands,
            command_count,
            drawn_commands,
            slot_count,
            batch_count,
            states,
            min,
            max,
            subsurface_draws,
        } = *layout;
        let subsurface = self.subsurface_states();
        // The scene's instances, read by culling and by the vertex shaders.
        let scene_instances = match source {
            Source::Scene { current, .. } => Some(current),
            Source::Ring(_) => None,
        };
        let pyramid_layout = PyramidLayout::new(depth_desc.width, depth_desc.height);
        // Temporal state: this frame's jitter (the projection moved by a sub-pixel
        // offset), last frame's camera, whether last frame is related at all.
        let temporal = view.temporal;
        let (jitter, unjittered, clip_from_world) = jittered(view, &depth_desc);
        let jitter_ndc = [
            2.0 * jitter[0] / depth_desc.width.max(1) as f32,
            2.0 * jitter[1] / depth_desc.height.max(1) as f32,
        ];
        let reset = temporal.is_some_and(|t| t.reset.replace(false));
        let previous_clip_from_world = temporal
            .and_then(|t| t.previous_clip_from_world.get())
            .unwrap_or(unjittered);
        let previous_count = match source {
            Source::Ring(_) => temporal.map_or(0, |t| t.previous_count.get()),
            // Every slot holds last frame's transform (new and teleported instances their
            // current one), unless the view's history was reset.
            Source::Scene { .. } if reset => 0,
            Source::Scene { .. } => instance_count,
        };
        // The environment, imported (and rebuilt if needed) by the caller.
        let world_from_clip = clip_from_world.inverse();
        let environment = imported.environment;
        let atmosphere = environment.and_then(|e| e.atmosphere);
        // Local lights: culled, sorted and binned on the CPU (ADR 0045).
        let axis = ViewAxis::new(unjittered);
        let (light_layout, uploaded) = {
            let mut scratch = self.scratch.borrow_mut();
            let scratch = &mut scratch.lights;
            let layout = match axis {
                Some(axis) if !view.lights.is_empty() => cull_lights(
                    view.lights,
                    unjittered,
                    axis,
                    [depth_desc.width, depth_desc.height],
                    scratch,
                ),
                _ => LightLayout::EMPTY,
            };
            let uploaded = if layout.count > 0 {
                let n = layout.count as usize;
                let (out, lights) = ring.alloc::<LightData>(n)?;
                write_lights(view.lights, scratch, out);
                let (out, rects) = ring.alloc::<[u32; 2]>(n)?;
                write_rects(scratch, out);
                let (out, bins) = ring.alloc::<u32>(Z_BINS)?;
                write_bins(view.lights, &layout, scratch, out);
                Some((lights.address, rects.address, bins.address))
            } else {
                None
            };
            (layout, uploaded)
        };
        // Local lights' shadows (ADR 0071): each kept light's first view, the views, the
        // views drawn this frame, and the atlas (imported once per frame).
        let local = self
            .local_shadows
            .as_ref()
            .filter(|_| self.local_shadows_on && !capture);
        let mut local_frame = LocalFrame::default();
        if let Some(state) = local {
            let plan = state.plan.borrow();
            if light_layout.count > 0 && !plan.views.is_empty() {
                let scratch = self.scratch.borrow();
                let n = light_layout.count as usize;
                let (out, staged) = ring.alloc::<u32>(n)?;
                for (out, index) in out.iter_mut().zip(scratch.lights.kept()) {
                    let first = plan.first_view[index as usize];
                    local_frame.shadowed |= first != NO_SHADOW;
                    out.write(first);
                }
                local_frame.light_shadows = staged.address;
            }
            if local_frame.shadowed {
                local_frame.views = ring.push(&plan.views)?;
            }
            if !plan.drawn.is_empty() {
                let (out, staged) = ring.alloc::<ShadowViewData>(plan.drawn.len())?;
                for (out, view) in out.iter_mut().zip(plan.drawn.iter()) {
                    out.write(view.data);
                }
                local_frame.draws = staged.address;
            }
            if local_frame.shadowed || !plan.drawn.is_empty() {
                let serial = fg.serial();
                let atlas = match state.imported.get() {
                    Some((s, texture)) if s == serial => texture,
                    _ => {
                        let final_state =
                            crate::graph::TextureState::shader_read(ShaderStages::FRAGMENT);
                        let texture = fg.import_texture(
                            "local shadow atlas",
                            crate::graph::ImportedTexture {
                                image: state.atlas.image,
                                view: state.atlas.view,
                                desc: state.atlas.desc,
                                initial: state.atlas.state.get(),
                                final_state: Some(final_state),
                                sampled_index: Some(state.atlas.sampled),
                                storage_index: None,
                            },
                        );
                        state.atlas.state.set(final_state);
                        state.imported.set(Some((serial, texture)));
                        state.rects.borrow_mut().clear();
                        texture
                    }
                };
                local_frame.atlas = Some(atlas);
                let mut rects = state.rects.borrow_mut();
                // Room for every view of every light (more views of one frame may add
                // theirs), so later frames do not grow it.
                rects.reserve(plan.views.len().max(plan.drawn.len()) * 2);
                let first = rects.len();
                rects.extend(plan.drawn.iter().map(|d| d.rect));
                local_frame.rects = first..rects.len();
            }
        }
        // Subsurface profiles, for transmission and the scatter pass (ADR 0059).
        let profiles = if subsurface_draws {
            ring.push(&self.subsurface_profiles)?
        } else {
            0
        };
        let settings = &self.shadow_settings;
        // Reflections (ADR 0073): with an environment (their last fallback), in perspective
        // views, never in a probe's capture. Their constants (and visible probes) go to the
        // ring before the shading.
        let reflection_passes = self
            .reflections
            .as_ref()
            .filter(|_| self.reflections_on && !capture && view.reflections.any());
        let prepared = match (reflection_passes, environment) {
            (Some(r), Some(env)) => r.passes.prepare(
                ring,
                &ViewSetup {
                    environment: env,
                    clip_from_world,
                    camera: view.camera.position,
                    size: [depth_desc.width, depth_desc.height],
                    probes: imported.probes.map(|(p, _)| p),
                    ssr: view.reflections.ssr,
                },
            ),
            _ => None,
        };
        // Whether the view reflects anything: visible probes or screen-space reflections.
        let reflecting = prepared.is_some_and(|p| p.probes > 0 || p.ssr.is_some());
        let fit = match capture_shadow {
            Some(c) => c.fit,
            None => shadow::fit_cascades(
                unjittered,
                view.camera.position,
                view.light.direction,
                min,
                max,
                settings,
            ),
        };
        let frame = FrameData {
            clip_from_world: clip_from_world.to_cols_array(),
            light_from_world: fit.light_from_world.to_cols_array(),
            camera_position: view.camera.position.extend(1.0).to_array(),
            to_light: (-view.light.direction.normalize_or(Vec3::NEG_Y))
                .extend(0.0)
                .to_array(),
            light_color: environment
                .and_then(|e| e.sun_color)
                .unwrap_or(view.light.color)
                .extend(0.0)
                .to_array(),
            ambient: view.light.ambient.extend(0.0).to_array(),
            planes: Frustum::from_clip_from_world(view.camera.clip_from_world)
                .planes
                .map(|p| p.to_array()),
            shadow_sampler: Bindless::sampler(SamplerKind::ShadowCompare),
            cull: u32::from(self.frustum_culling),
            visibility_capacity: occlusion.map_or(0, Occlusion::capacity),
            hiz_levels: pyramid_layout.levels,
            depth_size: [depth_desc.width, depth_desc.height],
            // Orthographic views (no perspective divide) draw full meshes.
            lod_scale: crate::cull::lod_scale(&unjittered, depth_desc.height, self.lod_threshold),
            slot_count,
            hiz_offsets: pyramid_layout.offsets,
            previous_clip_from_world: previous_clip_from_world.to_cols_array(),
            previous_clip_from_clip: (previous_clip_from_world * unjittered.inverse())
                .to_cols_array(),
            jitter: [jitter_ndc[0], jitter_ndc[1], 0.0, 0.0],
            previous_count,
            scatter_budget: self.subsurface_settings.unwrap_or_default().budget(),
            _pad2: [0; 2],
            cascades: cascade_data(&fit, settings),
            cascade_count: fit.count as u32,
            command_count,
            cascade_blend: settings.blend.clamp(0.0, 1.0),
            normal_bias: settings.normal_bias,
            fade_last: u32::from(fit.fade),
            scatter_footprint: {
                let (forward, near_center) =
                    axis.map_or((Vec3::NEG_Z, Vec3::ZERO), |a| (a.forward, a.near_center));
                let f = Footprint::new(unjittered, forward, near_center, depth_desc.height);
                [f.pixels_per_unit, f.w_scale, f.w_offset]
            },
            view_axis: axis.map_or([0.0; 4], |a| {
                a.forward.extend(-a.forward.dot(a.near_center)).to_array()
            }),
            lights: uploaded.map_or(0, |u| u.0),
            zbins: uploaded.map_or(0, |u| u.2),
            light_count: light_layout.count,
            light_words: light_layout.words,
            tiles_x: light_layout.tiles[0],
            bin_scale: light_layout.bin_scale,
            light_culling: u32::from(self.light_culling),
            // New noise every frame only when a temporal resolve averages it.
            scatter_frame: temporal.map_or(0, |t| t.frame.get() as u32),
            _pad4: [0; 2],
            world_from_clip: world_from_clip.to_cols_array(),
            environment: u32::from(environment.is_some()),
            sky_cube: environment.map_or(0, |e| e.source_index),
            prefiltered: environment.map_or(0, |e| e.prefiltered_index),
            dfg_lut: environment.map_or(0, |e| e.dfg_index),
            env_levels: PREFILTERED_LEVELS,
            env_intensity: environment.map_or(1.0, |e| e.intensity),
            linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
            subsurface_count: if subsurface_draws {
                self.subsurface_profiles.len() as u32
            } else {
                0
            },
            sh: environment.map_or(0, |e| e.sh_address),
            subsurface: profiles,
            atmosphere: atmosphere.map_or(0, |a| a.constants),
            local_shadow_atlas: local.map_or(0, |s| s.atlas.sampled),
            _pad5: 0,
            light_shadows: local_frame.light_shadows,
            shadow_views: local_frame.views,
            shadow_draws: local_frame.draws,
            shadow_draw_count: shadow_draws,
            shadow_texel: local.map_or(0.0, |s| 1.0 / s.atlas.desc.width.max(1) as f32),
        };
        let frame = ring.push(&[frame])?;
        let base = Push {
            frame,
            instances,
            batches: batch_table,
            visible: 0,
            commands: 0,
            visibility: occlusion.map_or(0, |o| o.address),
            pyramid: 0,
            positions: geometry.position_address(),
            attributes: geometry.attribute_address(),
            materials: materials.address(),
            previous_instances: match source {
                Source::Ring(_) => temporal.map_or(0, |t| t.address),
                Source::Scene {
                    previous_address, ..
                } => previous_address,
            },
            light_tiles: 0,
            instance_count,
            texture: 0,
            phase: PHASE_ALL,
            ao: NO_AO,
        };

        let command_bytes = u64::from(command_count) * COMMAND_BYTES;
        let culled = fg.create_buffer(
            "draw commands",
            BufferDesc {
                size: u64::from(phases) * command_bytes,
            },
        );
        let visible_bytes = u64::from(slot_count) * 4;
        let visible = fg.create_buffer(
            "visible instances",
            BufferDesc {
                size: u64::from(phases) * visible_bytes,
            },
        );
        let shadow_commands = fg.create_buffer(
            "shadow commands",
            BufferDesc {
                size: u64::from(cascade_count) * command_bytes,
            },
        );
        let casters = fg.create_buffer(
            "shadow casters",
            BufferDesc {
                size: u64::from(cascade_count) * visible_bytes,
            },
        );
        // Local lights' shadow views drawn this frame: their command and caster lists,
        // sized for a power of two of views so the graph reuses its buffers from frame to
        // frame (the count changes every frame).
        let local_lists = (shadow_draws > 0).then(|| {
            let room = u64::from(shadow_draws.next_power_of_two().max(4));
            (
                fg.create_buffer(
                    "local shadow commands",
                    BufferDesc {
                        size: room * command_bytes,
                    },
                ),
                fg.create_buffer(
                    "local shadow casters",
                    BufferDesc {
                        size: room * visible_bytes,
                    },
                ),
            )
        });
        let flags = occlusion.map(|o| {
            // Flags written by the previous frame's late phase, or to be cleared now.
            let stale = o.stale.replace(false);
            let written = BufferState {
                stages: vk::PipelineStageFlags2::COMPUTE_SHADER,
                access: vk::AccessFlags2::SHADER_STORAGE_READ
                    | vk::AccessFlags2::SHADER_STORAGE_WRITE,
            };
            let buffer = fg.import_buffer(
                "visibility flags",
                ImportedBuffer {
                    buffer: o.buffer,
                    size: u64::from(o.capacity.max(1)) * 4,
                    address: o.address,
                    initial: if stale { BufferState::idle() } else { written },
                    final_state: Some(written),
                },
            );
            (buffer, stale)
        });
        let mut setup = fg
            .add_pass(names.cull_setup)
            .write(culled, Access::TransferWrite)
            .write(shadow_commands, Access::TransferWrite);
        if let Some((flags, true)) = flags {
            setup = setup.write(flags, Access::TransferWrite);
        }
        if let Some((local_commands, _)) = local_lists {
            setup = setup.write(local_commands, Access::TransferWrite);
        }
        setup.execute(move |ctx| {
            let region = vk::BufferCopy {
                src_offset: commands.offset,
                dst_offset: 0,
                size: u64::from(phases) * command_bytes,
            };
            let shadow_region = vk::BufferCopy {
                src_offset: commands.offset + u64::from(phases) * command_bytes,
                dst_offset: 0,
                size: u64::from(cascade_count) * command_bytes,
            };
            // SAFETY: recording outside a rendering scope; the ranges are in bounds.
            unsafe {
                ctx.device
                    .cmd_copy_buffer(ctx.cmd, commands.buffer, ctx.buffer(culled), &[region]);
                ctx.device.cmd_copy_buffer(
                    ctx.cmd,
                    commands.buffer,
                    ctx.buffer(shadow_commands),
                    &[shadow_region],
                );
                if let Some((flags, true)) = flags {
                    ctx.device
                        .cmd_fill_buffer(ctx.cmd, ctx.buffer(flags), 0, vk::WHOLE_SIZE, 0);
                }
                if let Some((local_commands, _)) = local_lists {
                    let region = vk::BufferCopy {
                        src_offset: commands.offset
                            + u64::from(phases + cascade_count) * command_bytes,
                        dst_offset: 0,
                        size: u64::from(shadow_draws) * command_bytes,
                    };
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        commands.buffer,
                        ctx.buffer(local_commands),
                        &[region],
                    );
                }
            }
        });
        let flag_buffer = flags.map(|(buffer, _)| buffer);
        // One thread per instance.
        let cull_threads = Dispatch::Threads([instance_count, 1, 1]);
        let cull = &self.cull;
        let first_phase = if occlusion.is_some() {
            PHASE_EARLY
        } else {
            PHASE_ALL
        };
        let name = if occlusion.is_some() {
            "cull early"
        } else {
            names.cull
        };
        cull_pass(
            fg,
            bindless,
            name,
            culled,
            visible,
            flag_buffer,
            first_phase,
            scene_instances,
        )
        .dispatch(cull, cull_threads, move |ctx| {
            Some(Push {
                visible: ctx.address(visible),
                commands: ctx.address(culled),
                phase: first_phase,
                ..base
            })
        });

        // Shadow casters of each cascade, then each cascade drawn into its atlas tile; a
        // probe's later faces read its first face's map.
        let shared_shadow = capture_shadow.and_then(|c| c.map.get());
        let geometry = *geometry;
        let materials = *materials;
        let shadow_map = match shared_shadow {
            Some(map) => map,
            None => {
                fg.add_compute(names.shadow_cull, bindless)
                    .read(
                        shadow_commands,
                        Access::StorageReadWrite(ShaderStages::COMPUTE),
                    )
                    .write(casters, Access::StorageWrite(ShaderStages::COMPUTE))
                    .maybe_read(scene_instances, Access::StorageRead(ShaderStages::COMPUTE))
                    .dispatch(cull, cull_threads, move |ctx| {
                        Some(Push {
                            visible: ctx.address(casters),
                            commands: ctx.address(shadow_commands),
                            phase: PHASE_SHADOW,
                            ..base
                        })
                    });
                let resolution = settings.resolution;
                let (atlas_width, atlas_height) = settings.atlas_size();
                let columns = settings.atlas_grid().0;
                let shadow_map = fg.create_texture(
                    "shadow map",
                    TextureDesc::new_2d(settings.format, atlas_width, atlas_height),
                );
                let shadow_pipelines = &self.shadow;
                let drawn_cascades = fit.count as u32;
                materials
                    .read(geometry.read(fg.add_pass(names.shadow)))
                    .maybe_read(scene_instances, Access::StorageRead(ShaderStages::VERTEX))
                    .read(shadow_commands, Access::Indirect)
                    .read(casters, Access::StorageRead(ShaderStages::VERTEX))
                    .depth(shadow_map, depth_clear, StoreOp::Store)
                    .execute(move |ctx| {
                        for cascade in 0..drawn_cascades {
                            let tile = vk::Rect2D {
                                offset: vk::Offset2D {
                                    x: ((cascade % columns) * resolution) as i32,
                                    y: ((cascade / columns) * resolution) as i32,
                                },
                                extent: vk::Extent2D {
                                    width: resolution,
                                    height: resolution,
                                },
                            };
                            let push = Push {
                                visible: ctx.address(casters),
                                phase: cascade,
                                ..base
                            };
                            // SAFETY: recording inside the pass's rendering scope; the pass reads
                            // the commands (indirect), the caster lists, the pool and the material
                            // table.
                            unsafe {
                                states.draw(
                                    ctx,
                                    bindless,
                                    &geometry,
                                    [&shadow_pipelines[0], &shadow_pipelines[1]],
                                    &push,
                                    true,
                                    DEPTH_STATES,
                                    ctx.buffer(shadow_commands),
                                    u64::from(cascade) * command_bytes,
                                    tile,
                                );
                            }
                        }
                    });
                if let Some(c) = capture_shadow {
                    c.map.set(Some(shadow_map));
                }
                shadow_map
            }
        };
        // Local lights' shadow views (ADR 0071): culled per view, then each drawn into its
        // tile of the atlas (cleared first); the rest of the atlas keeps its contents.
        if let (Some(state), Some((local_commands, local_casters)), Some(atlas)) =
            (local, local_lists, local_frame.atlas)
        {
            let pipelines = &state.pipelines;
            fg.add_compute("local shadow cull", bindless)
                .read(
                    local_commands,
                    Access::StorageReadWrite(ShaderStages::COMPUTE),
                )
                .write(local_casters, Access::StorageWrite(ShaderStages::COMPUTE))
                .maybe_read(scene_instances, Access::StorageRead(ShaderStages::COMPUTE))
                .dispatch(
                    &pipelines.cull,
                    Dispatch::Threads([instance_count, shadow_draws, 1]),
                    move |ctx| {
                        Some(Push {
                            visible: ctx.address(local_casters),
                            commands: ctx.address(local_commands),
                            ..base
                        })
                    },
                );
            let rects = &state.rects;
            let range = local_frame.rects.clone();
            materials
                .read(geometry.read(fg.add_pass("local shadows")))
                .maybe_read(scene_instances, Access::StorageRead(ShaderStages::VERTEX))
                .read(local_commands, Access::Indirect)
                .read(local_casters, Access::StorageRead(ShaderStages::VERTEX))
                .depth(atlas, LoadOp::Load, StoreOp::Store)
                .execute(move |ctx| {
                    let rects = rects.borrow();
                    let push = Push {
                        visible: ctx.address(local_casters),
                        ..base
                    };
                    for (k, rect) in rects[range].iter().enumerate() {
                        let clear = vk::ClearAttachment {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
                            color_attachment: 0,
                            clear_value: vk::ClearValue {
                                depth_stencil: vk::ClearDepthStencilValue {
                                    depth: 1.0,
                                    stencil: 0,
                                },
                            },
                        };
                        // SAFETY: recording inside the pass's rendering scope; the pass
                        // reads the commands (indirect), the caster lists, the pool and the
                        // material table.
                        unsafe {
                            ctx.device.cmd_clear_attachments(
                                ctx.cmd,
                                &[clear],
                                &[vk::ClearRect {
                                    rect: *rect,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                }],
                            );
                            states.draw(
                                ctx,
                                bindless,
                                &geometry,
                                [&pipelines.depth[0], &pipelines.depth[1]],
                                &Push {
                                    phase: k as u32,
                                    ..push
                                },
                                true,
                                DEPTH_STATES,
                                ctx.buffer(local_commands),
                                k as u64 * command_bytes,
                                *rect,
                            );
                        }
                    }
                });
        }
        // Motion vectors: a second target, last frame's transforms read by the vertex
        // shader, and this frame's copied for the next one afterwards.
        let velocity = temporal.map(|_| fg.create_texture("motion vectors", multi(MOTION_FORMAT)));
        const COPIED: BufferState = BufferState {
            stages: vk::PipelineStageFlags2::COPY,
            access: vk::AccessFlags2::TRANSFER_WRITE,
        };
        let previous = temporal.map(|t| {
            // The scene keeps last frame's instances itself.
            if let Source::Scene { previous, .. } = source {
                return previous;
            }
            let buffer = fg.import_buffer(
                "previous instances",
                ImportedBuffer {
                    buffer: t.buffer,
                    size: u64::from(t.capacity.max(1)) * size_of::<InstanceData>() as u64,
                    address: t.address,
                    initial: t.state.get(),
                    final_state: Some(COPIED),
                },
            );
            t.state.set(COPIED);
            buffer
        });
        // The tiles' light bitmasks, from the lights' rectangles.
        let light_tiles = match uploaded {
            Some((_, rects, _)) if self.light_culling => {
                let words = light_layout.mask_words();
                let lights = light_layout.count;
                let buffer = fg.create_buffer("light tiles", BufferDesc { size: words * 4 });
                fg.add_pass(names.light_tiles_clear)
                    .write(buffer, Access::TransferWrite)
                    .execute(move |ctx| {
                        // SAFETY: recording outside a rendering scope.
                        unsafe {
                            ctx.device.cmd_fill_buffer(
                                ctx.cmd,
                                ctx.buffer(buffer),
                                0,
                                vk::WHOLE_SIZE,
                                0,
                            );
                        }
                    });
                // One workgroup per light: it sets the light's bit over its rectangle.
                fg.add_compute(names.light_tiles, bindless)
                    .read(buffer, Access::StorageReadWrite(ShaderStages::COMPUTE))
                    .dispatch(
                        &self.light_tiles,
                        Dispatch::Groups([lights, 1, 1]),
                        move |ctx| {
                            Some(Push {
                                visible: rects,
                                light_tiles: ctx.address(buffer),
                                ..base
                            })
                        },
                    );
                Some(buffer)
            }
            _ => None,
        };
        let mut targets = Targets {
            color,
            color_load: load,
            velocity,
            previous,
            light_tiles,
            environment,
            ao: None,
            local_atlas: local_frame.atlas.filter(|_| local_frame.shadowed),
            contact: None,
            reflect: None,
            resolves: None,
        };

        // Ambient occlusion needs the whole depth before shading: the prepass draws it.
        // Probe captures draw without it.
        let ao = self
            .ao_settings
            .filter(|_| !capture)
            .zip(view.camera.view_space())
            .zip(self.ao.as_ref());
        let ao_frame = if temporal.is_some() {
            let frame = self.ao_frame.get().wrapping_add(1);
            self.ao_frame.set(frame);
            frame
        } else {
            // Without a temporal resolve changing noise would flicker.
            0
        };
        // Subsurface materials need it too: their pass follows the forward pass with the
        // depth tested for equality.
        // The sun's contact shadows (ADR 0071) need it too.
        let contact = self
            .contact_settings
            .filter(|_| !capture)
            .zip(view.camera.view_space())
            .zip(self.contact.as_ref());
        let contact_view = ContactView {
            clip_from_world,
            to_light: -view.light.direction.normalize_or(Vec3::NEG_Y),
        };
        // Reflections need it too (ADR 0073): their inputs are written by the shading
        // after it.
        targets.reflect = reflecting.then(|| {
            (
                fg.create_texture("reflection normals", multi(NORMAL_ROUGHNESS_FORMAT)),
                fg.create_texture("reflection weights", multi(WEIGHT_FORMAT)),
            )
        });
        // With MSAA: the lit image screen-space reflections trace (resolved by the last
        // pass that writes the color before them), and their inputs, resolved.
        let scattering = subsurface.is_some() && subsurface_draws;
        let ms_resolves = ms.map(|_| MsResolves {
            lit: (reflecting && view.reflections.ssr.is_some())
                .then(|| fg.create_texture("lit resolved", single(self.color_format))),
            reflect: targets.reflect.map(|_| {
                [
                    fg.create_texture(
                        "reflection normals resolved",
                        single(NORMAL_ROUGHNESS_FORMAT),
                    ),
                    fg.create_texture("reflection weights resolved", single(WEIGHT_FORMAT)),
                    fg.create_texture("reflection far normals", single(NORMAL_ROUGHNESS_FORMAT)),
                    fg.create_texture("reflection far weights", single(WEIGHT_FORMAT)),
                    fg.create_texture("reflection far depth", single(msaa::FAR_DEPTH_FORMAT)),
                ]
            }),
        });
        targets.resolves = ms_resolves.map(|r| MsResolves {
            // The scatter pass resolves the lit image when subsurface materials draw.
            lit: r.lit.filter(|_| !scattering),
            ..r
        });
        let prepass = ms.is_some()
            || self.depth_prepass
            || ao.is_some()
            || subsurface_draws
            || contact.is_some()
            || reflecting;
        // With a prepass, the prepass writes the motion vectors and the shading after it
        // draws without them (the still variants' registers: on the Radeon its vertex
        // stage takes 49 VGPRs instead of 78).
        let prepass_motion = prepass && velocity.is_some();
        if prepass_motion {
            targets.velocity = None;
            targets.previous = None;
        }
        let prepass_pipelines = match (ms, prepass_motion) {
            (Some(m), false) => [&m.prepass[0], &m.prepass[1]],
            (Some(m), true) => [&m.prepass_motion[0], &m.prepass_motion[1]],
            (None, false) => [&self.prepass[0], &self.prepass[1]],
            (None, true) => [&self.prepass_motion[0], &self.prepass_motion[1]],
        };
        // What the prepass phases attach for motion vectors: the target (cleared by the
        // first phase) and last frame's instances.
        let prepass_velocity = velocity.zip(previous).filter(|_| prepass_motion);
        // After the prepass every state shades with the equal-depth pipeline: masked
        // materials were alpha-tested there, and nothing is left to discard.
        let variant = usize::from(light_layout.count > 0) | usize::from(environment.is_some()) << 1;
        // With shadowed local lights in reach, the shading that looks their shadows up
        // (ADR 0071; only then, so other views carry no code for it).
        let shadowed = local.filter(|_| local_frame.shadowed).map(|s| &s.pipelines);
        let shading = match shadowed {
            Some(p) => &p.shading[shadowed_index(variant)],
            None => &self.shading[variant],
        };
        // With ambient occlusion, the shading that reads it (the prepass is on then).
        let ao_shading = ao.map(|(_, passes)| match shadowed.and_then(|p| p.ao.as_ref()) {
            Some(p) => &p[shadowed_index(variant)],
            None => &passes.shading[variant],
        });
        // With contact shadows (the prepass is on then), the shading that reads their mask.
        let contact_shading = contact.and_then(|(_, c)| {
            let k = shadowed_index(variant);
            match (ao.is_some(), shadowed.is_some()) {
                (false, false) => Some(&c.shading[variant]),
                (true, false) => c.ao.as_ref().map(|p| &p[variant]),
                (false, true) => c.shadowed.as_ref().map(|p| &p[k]),
                (true, true) => c.shadowed_ao.as_ref().map(|p| &p[k]),
            }
        });
        // With reflections, the shading that also writes their inputs.
        let reflect_shading = reflection_passes.filter(|_| reflecting).and_then(|r| {
            r.pipeline(
                variant & 1 != 0,
                ao.is_some(),
                contact.is_some(),
                shadowed.is_some(),
            )
        });
        let main_pipelines = if let Some(m) = ms {
            let k = shadowed_index(variant);
            let ms_shadowed = m.shadowed.as_ref().filter(|_| shadowed.is_some());
            let p: &Pipeline = if reflect_shading.is_some() {
                m.reflect_pipeline(
                    variant & 1 != 0,
                    ao.is_some(),
                    contact.is_some(),
                    shadowed.is_some(),
                )?
            } else if contact.is_some() {
                let c = m.contact.as_ref()?;
                match (ao.is_some(), shadowed.is_some()) {
                    (false, false) => &c.shading[variant],
                    (true, false) => &c.ao.as_ref()?[variant],
                    (false, true) => &c.shadowed.as_ref()?[k],
                    (true, true) => &c.shadowed_ao.as_ref()?[k],
                }
            } else if ao.is_some() {
                match ms_shadowed {
                    Some(s) => &s.ao.as_ref()?[k],
                    None => &m.ao.as_ref()?[variant],
                }
            } else {
                match ms_shadowed {
                    Some(s) => &s.shading[k].shade,
                    None => &m.shading[variant].shade,
                }
            };
            [p, p]
        } else {
            match (contact_shading, prepass, velocity.is_some()) {
                _ if reflect_shading.is_some() => {
                    let p = reflect_shading?;
                    [p, p]
                }
                (Some(p), _, _) => [p, p],
                // After the prepass (which wrote the motion vectors).
                (None, true, _) => match ao_shading {
                    Some(p) => [p, p],
                    None => [&shading.shade, &shading.shade],
                },
                (None, false, false) => [&shading.main[0], &shading.main[1]],
                (None, false, true) => [&shading.main_motion[0], &shading.main_motion[1]],
            }
        };
        // `contact`: the sun's contact shadows, whose mask's index the shading that reads
        // it takes in `phase` (ADR 0071).
        let draw_phase = move |ctx: &mut crate::graph::PassContext<'_>,
                               pipelines: [&Pipeline; 2],
                               phase: u32,
                               mask: u32,
                               ao: Option<Texture>,
                               contact: Option<Texture>| {
            let push = Push {
                visible: ctx.address(visible),
                texture: ctx.sampled_index(shadow_map).unwrap_or(0),
                light_tiles: light_tiles.map_or(0, |b| ctx.address(b)),
                ao: ao.and_then(|t| ctx.sampled_index(t)).unwrap_or(NO_AO),
                phase: contact
                    .and_then(|t| ctx.sampled_index(t))
                    .unwrap_or(base.phase),
                ..base
            };
            let offset = u64::from(phase.saturating_sub(1)) * command_bytes;
            // SAFETY: recording inside the pass's rendering scope; the pass reads the pool
            // and the material table.
            unsafe {
                states.draw(
                    ctx,
                    bindless,
                    &geometry,
                    pipelines,
                    &push,
                    false,
                    mask,
                    ctx.buffer(culled),
                    offset,
                    ctx.render_area(),
                );
            }
        };
        // The depth is needed after the frame's passes for motion vectors and upsampling.
        let depth_store = if velocity.is_some() || environment.is_some() {
            StoreOp::Store
        } else {
            StoreOp::DontCare
        };
        let draw_lists = DrawLists {
            push: base,
            commands: culled,
            visible,
            instances: scene_instances,
            phases,
            command_bytes,
            states,
        };
        let mut passes =
            match flag_buffer {
                None => {
                    let depth_use = if prepass {
                        let mut pass = prepass_pass(
                            fg,
                            names.depth_prepass,
                            &geometry,
                            &materials,
                            culled,
                            visible,
                            scene_instances,
                        )
                        .depth(depth, depth_clear, StoreOp::Store);
                        if let Some(resolved) = resolved_depth {
                            pass = pass.resolve(resolved, nearest);
                        }
                        if let Some((velocity, previous)) = prepass_velocity {
                            pass = pass
                                .color(velocity, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                                .read(previous, Access::StorageRead(ShaderStages::VERTEX));
                        }
                        pass.execute(move |ctx| {
                            draw_phase(ctx, prepass_pipelines, PHASE_ALL, DEPTH_STATES, None, None)
                        });
                        DepthUse::ReadOnly
                    } else {
                        DepthUse::Write(depth_clear, depth_store)
                    };
                    if let Some(((settings, view_space), passes)) = &ao {
                        targets.ao =
                            Some(passes.passes.add_passes(
                                fg, bindless, read_depth, view_space, settings, ao_frame,
                            ));
                    }
                    if let Some(((settings, _), state)) = contact {
                        targets.contact = Some(state.passes.add_passes(
                            fg,
                            bindless,
                            read_depth,
                            &contact_view,
                            &settings,
                        ));
                    }
                    let ao_texture = targets.ao;
                    let contact_texture = targets.contact;
                    shading_pass(
                        fg,
                        names.forward,
                        &geometry,
                        &materials,
                        shadow_map,
                        culled,
                        visible,
                        targets,
                        true,
                        depth,
                        depth_use,
                        scene_instances,
                    )
                    .execute(move |ctx| {
                        draw_phase(
                            ctx,
                            main_pipelines,
                            PHASE_ALL,
                            PLAIN_STATES,
                            ao_texture,
                            contact_texture,
                        )
                    });
                    ForwardPasses {
                        instances: instance_count,
                        batches: batch_count,
                        draw_commands: drawn_commands,
                        list_commands: command_count,
                        occlusion: false,
                        commands: Some(culled),
                        motion: None,
                        ao: targets.ao,
                        atmosphere: atmosphere.and_then(|a| a.textures),
                        contact_shadows: targets.contact,
                        reflections: None,
                        resolved_depth,
                        draws: Some(draw_lists),
                    }
                }
                Some(flags) => {
                    // Two phases: draw what was visible, build the pyramid from its depth, then
                    // test everything against it and draw what the first phase missed. With the
                    // prepass the phases draw depth only, and the forward pass shades both
                    // lists once.
                    if prepass {
                        let mut pass = prepass_pass(
                            fg,
                            "depth prepass early",
                            &geometry,
                            &materials,
                            culled,
                            visible,
                            scene_instances,
                        )
                        .depth(depth, depth_clear, StoreOp::Store);
                        // The pyramid keeps the farthest depth: conservative for culling.
                        if let Some(resolved) = resolved_depth {
                            pass = pass.resolve(resolved, farthest);
                        }
                        if let Some((velocity, previous)) = prepass_velocity {
                            pass = pass
                                .color(velocity, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                                .read(previous, Access::StorageRead(ShaderStages::VERTEX));
                        }
                        pass.execute(move |ctx| {
                            draw_phase(
                                ctx,
                                prepass_pipelines,
                                PHASE_EARLY,
                                DEPTH_STATES,
                                None,
                                None,
                            )
                        });
                    } else {
                        shading_pass(
                            fg,
                            "forward early",
                            &geometry,
                            &materials,
                            shadow_map,
                            culled,
                            visible,
                            targets,
                            true,
                            depth,
                            DepthUse::Write(depth_clear, StoreOp::Store),
                            scene_instances,
                        )
                        .execute(move |ctx| {
                            draw_phase(ctx, main_pipelines, PHASE_EARLY, PLAIN_STATES, None, None)
                        });
                    }
                    let pyramid = fg.create_buffer(
                        "depth pyramid",
                        BufferDesc {
                            size: pyramid_layout.bytes(),
                        },
                    );
                    self.pyramid.add_pass(fg, bindless, read_depth, pyramid);
                    cull_pass(
                        fg,
                        bindless,
                        "cull late",
                        culled,
                        visible,
                        Some(flags),
                        PHASE_LATE,
                        scene_instances,
                    )
                    .read(pyramid, Access::StorageRead(ShaderStages::COMPUTE))
                    .dispatch(cull, cull_threads, move |ctx| {
                        Some(Push {
                            visible: ctx.address(visible) + visible_bytes,
                            commands: ctx.address(culled) + command_bytes,
                            pyramid: ctx.address(pyramid),
                            phase: PHASE_LATE,
                            ..base
                        })
                    });
                    if prepass {
                        let mut pass = prepass_pass(
                            fg,
                            "depth prepass late",
                            &geometry,
                            &materials,
                            culled,
                            visible,
                            scene_instances,
                        )
                        .depth(depth, LoadOp::Load, StoreOp::Store);
                        if let Some(resolved) = resolved_depth {
                            pass = pass.resolve(resolved, nearest);
                        }
                        if let Some((velocity, previous)) = prepass_velocity {
                            pass = pass
                                .color(velocity, LoadOp::Load, StoreOp::Store)
                                .read(previous, Access::StorageRead(ShaderStages::VERTEX));
                        }
                        pass.execute(move |ctx| {
                            draw_phase(ctx, prepass_pipelines, PHASE_LATE, DEPTH_STATES, None, None)
                        });
                        if let Some(((settings, view_space), passes)) = &ao {
                            targets.ao = Some(passes.passes.add_passes(
                                fg, bindless, read_depth, view_space, settings, ao_frame,
                            ));
                        }
                        if let Some(((settings, _), state)) = contact {
                            targets.contact = Some(state.passes.add_passes(
                                fg,
                                bindless,
                                read_depth,
                                &contact_view,
                                &settings,
                            ));
                        }
                        let ao_texture = targets.ao;
                        let contact_texture = targets.contact;
                        shading_pass(
                            fg,
                            names.forward,
                            &geometry,
                            &materials,
                            shadow_map,
                            culled,
                            visible,
                            targets,
                            true,
                            depth,
                            DepthUse::ReadOnly,
                            scene_instances,
                        )
                        .execute(move |ctx| {
                            draw_phase(
                                ctx,
                                main_pipelines,
                                PHASE_EARLY,
                                PLAIN_STATES,
                                ao_texture,
                                contact_texture,
                            );
                            draw_phase(
                                ctx,
                                main_pipelines,
                                PHASE_LATE,
                                PLAIN_STATES,
                                ao_texture,
                                contact_texture,
                            );
                        });
                    } else {
                        shading_pass(
                            fg,
                            "forward late",
                            &geometry,
                            &materials,
                            shadow_map,
                            culled,
                            visible,
                            targets,
                            false,
                            depth,
                            DepthUse::Write(LoadOp::Load, depth_store),
                            scene_instances,
                        )
                        .execute(move |ctx| {
                            draw_phase(ctx, main_pipelines, PHASE_LATE, PLAIN_STATES, None, None)
                        });
                    }
                    ForwardPasses {
                        instances: instance_count,
                        batches: batch_count,
                        draw_commands: drawn_commands,
                        list_commands: command_count,
                        occlusion: true,
                        commands: Some(culled),
                        motion: None,
                        ao: targets.ao,
                        atmosphere: atmosphere.and_then(|a| a.textures),
                        contact_shadows: targets.contact,
                        reflections: None,
                        resolved_depth,
                        draws: Some(draw_lists),
                    }
                }
            };
        // Subsurface materials (ADR 0059): shaded after the others, also writing their
        // diffuse light and albedo, then scattered into the color target.
        if let Some(pipelines) = subsurface.filter(|_| subsurface_draws) {
            let variant =
                usize::from(light_layout.count > 0) | usize::from(environment.is_some()) << 1;
            let irradiance = fg.create_texture("subsurface irradiance", multi(IRRADIANCE_FORMAT));
            let albedo = fg.create_texture("subsurface albedo", multi(ALBEDO_FORMAT));
            // With MSAA the scatter pass reads them resolved (averaged).
            let resolved = ms.map(|_| {
                (
                    fg.create_texture("subsurface irradiance resolved", single(IRRADIANCE_FORMAT)),
                    fg.create_texture("subsurface albedo resolved", single(ALBEDO_FORMAT)),
                )
            });
            // (After the prepass, which wrote their motion vectors.)
            let ms_subsurface = ms.and_then(|m| m.subsurface.as_ref());
            let ms_shadowed = ms
                .and_then(|m| m.shadowed.as_ref())
                .filter(|_| shadowed.is_some())
                .and_then(|p| p.subsurface.as_ref());
            let pipeline = match (ms, shadowed.and_then(|p| p.subsurface.as_ref())) {
                (Some(_), _) => match ms_shadowed {
                    Some(p) => &p[shadowed_index(variant)],
                    None => &ms_subsurface?.0[variant],
                },
                (None, Some(p)) => &p[shadowed_index(variant)],
                (None, None) => &pipelines.shading[variant],
            };
            let two_phases = flag_buffer.is_some();
            // Their diffuse light is occluded by the view's ambient occlusion too.
            let ao_texture = targets.ao;
            // They keep the sky's reflections (their pass writes no reflection inputs).
            let targets = Targets {
                reflect: None,
                resolves: None,
                ..targets
            };
            shading_pass(
                fg,
                names.forward_subsurface,
                &geometry,
                &materials,
                shadow_map,
                culled,
                visible,
                targets,
                false,
                depth,
                DepthUse::ReadOnly,
                scene_instances,
            )
            .color(irradiance, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .color(albedo, LoadOp::DontCare, StoreOp::Store)
            .execute(move |ctx| {
                let pipelines = [pipeline, pipeline];
                if two_phases {
                    draw_phase(
                        ctx,
                        pipelines,
                        PHASE_EARLY,
                        SUBSURFACE_STATES,
                        ao_texture,
                        None,
                    );
                    draw_phase(
                        ctx,
                        pipelines,
                        PHASE_LATE,
                        SUBSURFACE_STATES,
                        ao_texture,
                        None,
                    );
                } else {
                    draw_phase(
                        ctx,
                        pipelines,
                        PHASE_ALL,
                        SUBSURFACE_STATES,
                        ao_texture,
                        None,
                    );
                }
            });
            // With MSAA: their nearest sample's light and albedo (depth and profile in
            // alpha must stay one sample's).
            if let (Some(m), Some((irradiance_1x, albedo_1x))) = (ms_subsurface, resolved) {
                msaa::add_nearest_resolve(
                    fg,
                    bindless,
                    "subsurface resolve",
                    &m.2,
                    [irradiance, albedo],
                    None,
                    &[irradiance_1x, albedo_1x],
                );
            }
            // The scatter pass: the same batches, adding where their depth is.
            let scatter = match ms_subsurface {
                Some((_, scatter, _)) => scatter,
                None => &pipelines.scatter,
            };
            // With MSAA the neighbours' light comes from the resolve, the fragment's own
            // light and albedo from its samples (`scatterFragmentMs`).
            let (light, albedo, own_light) = match resolved {
                Some((light, _)) => (light, albedo, Some(irradiance)),
                None => (irradiance, albedo, None),
            };
            let lit = ms_resolves.and_then(|r| r.lit);
            let mut pass = forward_pass(
                fg,
                names.subsurface_scatter,
                &geometry,
                &materials,
                shadow_map,
                culled,
                visible,
                scene_instances,
            )
            .read(light, Access::Sampled(ShaderStages::FRAGMENT))
            .read(albedo, Access::Sampled(ShaderStages::FRAGMENT))
            .color(color, LoadOp::Load, StoreOp::Store);
            if let Some(own) = own_light {
                pass = pass.read(own, Access::Sampled(ShaderStages::FRAGMENT));
            }
            if let Some(lit) = lit {
                pass = pass.resolve(lit, ResolveMode::Average);
            }
            pass.depth_read_only(depth).execute(move |ctx| {
                let (Some(light), Some(albedo)) =
                    (ctx.sampled_index(light), ctx.sampled_index(albedo))
                else {
                    return;
                };
                let push = Push {
                    visible: ctx.address(visible),
                    texture: light,
                    phase: albedo,
                    ao: own_light
                        .and_then(|t| ctx.sampled_index(t))
                        .unwrap_or(base.ao),
                    ..base
                };
                let phases: &[u32] = if two_phases {
                    &[PHASE_EARLY, PHASE_LATE]
                } else {
                    &[PHASE_ALL]
                };
                for &phase in phases {
                    let offset = u64::from(phase.saturating_sub(1)) * command_bytes;
                    // SAFETY: recording inside the pass's rendering scope; the pass reads
                    // the pool, the material table and the commands.
                    unsafe {
                        states.draw(
                            ctx,
                            bindless,
                            &geometry,
                            [scatter, scatter],
                            &push,
                            false,
                            SUBSURFACE_STATES,
                            ctx.buffer(culled),
                            offset,
                            ctx.render_area(),
                        );
                    }
                }
            });
        }
        // Reflections (ADR 0073): screen-space reflections and probes replace the sky's
        // specular light on the opaque image, before the atmosphere and the transparent
        // surfaces.
        if let (Some(r), Some(prepared), Some(env), Some((normal_roughness, weight))) =
            (reflection_passes, prepared, environment, targets.reflect)
        {
            if let Some((probes, _)) = imported.probes.filter(|_| prepared.probes > 0) {
                probes.mark_used(true, false);
            }
            // With MSAA: the nearest sample's inputs, the multisampled color as the
            // composite's target.
            let (normal_roughness, weight) = match (
                ms_resolves.and_then(|r| r.reflect),
                ms.and_then(|m| m.reflect.as_deref()),
            ) {
                (Some(resolved), Some(r)) => {
                    msaa::add_nearest_resolve(
                        fg,
                        bindless,
                        "reflection inputs resolve",
                        &r.2,
                        [normal_roughness, weight],
                        Some(depth),
                        &resolved,
                    );
                    (resolved[0], resolved[1])
                }
                _ => (normal_roughness, weight),
            };
            let lit = ms_resolves.and_then(|r| r.lit).unwrap_or(color);
            let composite = ms
                .and_then(|m| m.reflect.as_deref())
                .zip(ms_resolves)
                .and_then(|(r, resolves)| {
                    let [_, _, far_nr, far_weight, far_depth] = resolves.reflect?;
                    Some(MsComposite {
                        pipelines: &r.1,
                        depth,
                        samples: samples.as_raw(),
                        far: [far_depth, far_nr, far_weight],
                    })
                });
            let ssr = r.passes.add_view_passes(
                fg,
                bindless,
                &prepared,
                &ViewInputs {
                    color,
                    lit,
                    depth: read_depth,
                    normal_roughness,
                    weight,
                    environment: env,
                    probes: imported.probes.map(|(_, t)| t),
                },
                composite,
            );
            passes.reflections = Some(crate::reflections::ReflectionOutput {
                normal_roughness,
                weight,
                ssr,
            });
        }
        // With an atmosphere or fog: the sky, aerial perspective and fog over the opaque
        // image (ADR 0069). A probe's capture fills its sky in its face copy instead.
        if capture {
        } else if let (Some(env), Some(atmosphere)) = (environment, atmosphere) {
            // With MSAA per sample, from each sample's depth.
            crate::atmosphere::add_composite_pass(
                fg,
                bindless,
                ms.map_or(&self.composite, |m| &m.composite),
                color,
                depth,
                &env,
                &atmosphere,
            );
        } else if let Some(env) = environment {
            // The sky where no geometry was drawn (depth still 1).
            let sky = ms.map_or(&self.sky, |m| &m.sky);
            fg.add_pass("sky")
                .color(color, LoadOp::Load, StoreOp::Store)
                .depth_read_only(depth)
                .read(env.source, Access::Sampled(ShaderStages::FRAGMENT))
                .execute(move |ctx| {
                    let area = ctx.render_area();
                    // SAFETY: recording inside the pass's rendering scope; the frame
                    // constants are in the ring.
                    unsafe {
                        sky.bind(ctx.cmd);
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        set_viewport(ctx.device, ctx.cmd, area);
                        bindless.push_constants(ctx.cmd, push_bytes(&base));
                        ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                    }
                });
        }
        // Transparent surfaces, back to front over the opaque image: depth tested, not
        // written, lit like the rest (ADR 0054). They were culled in the last phase only.
        let blended_commands: u32 = (0..STATES)
            .filter(|s| s & BLENDED != 0)
            .map(|s| states.counts[s])
            .sum();
        let reactive = (blended_commands > 0 && velocity.is_some())
            .then(|| fg.create_texture("reactive mask", multi(REACTIVE_FORMAT)));
        // With MSAA, resolved (averaged) for the temporal resolve.
        let reactive_resolved = reactive.and_then(|_| {
            ms.map(|_| fg.create_texture("reactive resolved", single(REACTIVE_FORMAT)))
        });
        if blended_commands > 0 {
            let reactive_index = usize::from(reactive.is_some());
            let pipelines = match (ms, atmosphere, shadowed) {
                (Some(m), _, _) => {
                    let ms_shadowed = m.shadowed.as_ref().filter(|_| shadowed.is_some());
                    match (atmosphere, ms_shadowed) {
                        (Some(_), Some(p)) => &p.blended_atmosphere[reactive_index],
                        (Some(_), None) => &m.blended_atmosphere[variant & 1][reactive_index],
                        (None, Some(p)) => {
                            &p.shading[shadowed_index(variant)].blended[reactive_index]
                        }
                        (None, None) => &m.shading[variant].blended[reactive_index],
                    }
                }
                // Under an atmosphere or fog (the environment variants).
                (None, Some(_), Some(p)) => &p.blended_atmosphere[reactive_index],
                (None, Some(_), None) => &self.blended_atmosphere[variant & 1][reactive_index],
                (None, None, _) => &shading.blended[reactive_index],
            };
            let mut pass = forward_pass(
                fg,
                names.transparent,
                &geometry,
                &materials,
                shadow_map,
                culled,
                visible,
                scene_instances,
            )
            .color(color, LoadOp::Load, StoreOp::Store);
            if let Some(reactive) = reactive {
                pass = pass.color(reactive, LoadOp::Clear([0.0; 4]), StoreOp::Store);
                if let Some(resolved) = reactive_resolved {
                    pass = pass.resolve(resolved, ResolveMode::Average);
                }
            }
            if let Some(tiles) = light_tiles {
                pass = pass.read(tiles, Access::StorageRead(ShaderStages::FRAGMENT));
            }
            if let Some(env) = environment {
                pass = pass
                    .read(env.prefiltered, Access::Sampled(ShaderStages::FRAGMENT))
                    .read(env.dfg, Access::Sampled(ShaderStages::FRAGMENT))
                    .read(env.sh, Access::StorageRead(ShaderStages::FRAGMENT));
            }
            if let Some(t) = atmosphere.and_then(|a| a.textures) {
                pass = pass.read(t.aerial, Access::Sampled(ShaderStages::FRAGMENT));
            }
            if let Some(atlas) = local_frame.atlas.filter(|_| local_frame.shadowed) {
                pass = pass.read(atlas, Access::Sampled(ShaderStages::FRAGMENT));
            }
            let offset = u64::from(phases - 1) * command_bytes;
            pass.depth_read_only(depth).execute(move |ctx| {
                let push = Push {
                    visible: ctx.address(visible),
                    texture: ctx.sampled_index(shadow_map).unwrap_or(0),
                    light_tiles: light_tiles.map_or(0, |b| ctx.address(b)),
                    ..base
                };
                // SAFETY: recording inside the pass's rendering scope; the pass reads the
                // pool, the material table and the commands.
                unsafe {
                    states.draw(
                        ctx,
                        bindless,
                        &geometry,
                        [pipelines, pipelines],
                        &push,
                        false,
                        BLENDED_STATES,
                        ctx.buffer(culled),
                        offset,
                        ctx.render_area(),
                    );
                }
            });
        }
        let (Some(temporal), Some(velocity), Some(previous)) = (temporal, velocity, previous)
        else {
            return Some(passes);
        };

        // Where no geometry was drawn (depth still 1), the camera's motion.
        let background = ms.map_or(&self.background, |m| &m.background);
        fg.add_pass("motion background")
            .color(velocity, LoadOp::Load, StoreOp::Store)
            .depth_read_only(depth)
            .execute(move |ctx| {
                let area = ctx.render_area();
                // SAFETY: recording inside the pass's rendering scope; the frame constants
                // are in the ring.
                unsafe {
                    background.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_viewport(ctx.device, ctx.cmd, area);
                    bindless.push_constants(ctx.cmd, push_bytes(&base));
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                }
            });
        // This frame's transforms are next frame's previous ones (the scene path keeps
        // them in the scene).
        let instances_staged = match source {
            Source::Ring(staged) => Some(staged),
            Source::Scene { .. } => None,
        };
        let copied = instances_staged.map_or(0, |_| instance_count.min(temporal.capacity));
        if let Some(instances_staged) = instances_staged.filter(|_| copied > 0) {
            let size = u64::from(copied) * size_of::<InstanceData>() as u64;
            fg.add_pass("remember instances")
                .write(previous, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; both ranges are in
                    // bounds (the ring holds every instance, the state `capacity`).
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            instances_staged.buffer,
                            ctx.buffer(previous),
                            &[vk::BufferCopy {
                                src_offset: instances_staged.offset,
                                dst_offset: 0,
                                size,
                            }],
                        );
                    }
                });
        }
        temporal.previous_count.set(copied);
        temporal.previous_clip_from_world.set(Some(unjittered));
        temporal.frame.set(temporal.frame.get() + 1);
        Some(ForwardPasses {
            motion: Some(MotionOutput {
                velocity,
                jitter,
                reset,
                reactive: reactive_resolved.or(reactive),
                clip_from_world,
                previous_clip_from_world,
            }),
            ..passes
        })
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
}

/// What a frame's shading and passes take of the local lights' shadows (ADR 0071).
#[derive(Debug, Default)]
struct LocalFrame {
    /// Some kept light has a shadow: the shading looks them up.
    shadowed: bool,
    /// Addresses of the per-light first views, the views and the drawn views (0: none).
    light_shadows: u64,
    views: u64,
    draws: u64,
    /// The atlas in this frame's graph.
    atlas: Option<Texture>,
    /// This view's drawn tiles among the frame's.
    rects: core::ops::Range<usize>,
}

/// Where a frame's instance changes come from, for the local shadows' cache.
#[derive(Clone, Copy)]
enum Changes<'a, 'g> {
    Draws(&'a [DrawItem]),
    Scene(&'a SceneFrame<'g>),
}

/// Where the instances come from.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// Written into the ring this frame, in draw order (the draw-list path).
    Ring(crate::upload::Staged),
    /// A persistent scene's buffers.
    Scene {
        current: Buffer,
        previous: Buffer,
        previous_address: vk::DeviceAddress,
    },
}

/// One frame's instances, batches and draw commands.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// Instances the culling pass visits (one thread each).
    instance_count: u32,
    instances: vk::DeviceAddress,
    source: Source,
    batch_table: vk::DeviceAddress,
    /// The command lists, in the ring.
    commands: crate::upload::Staged,
    /// Commands per list.
    command_count: u32,
    /// Commands that draw (the scene path's sink follows them).
    drawn_commands: u32,
    /// Visible-list slots per list.
    slot_count: u32,
    batch_count: u32,
    states: States,
    /// World-space bounds of the instances (the shadow cascades' fit).
    min: Vec3,
    max: Vec3,
    subsurface_draws: bool,
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
        reflections: None,
        resolved_depth,
        draws: None,
    }
}

/// The targets of the shading passes.
#[derive(Debug, Clone, Copy)]
struct Targets {
    color: Texture,
    /// What the first shading pass does with the color target.
    color_load: LoadOp,
    /// Motion vectors, with a temporal state.
    velocity: Option<Texture>,
    /// Last frame's instances, read by the vertex shader for motion vectors.
    previous: Option<Buffer>,
    /// The tiles' light bitmasks, with light culling.
    light_tiles: Option<Buffer>,
    /// The environment's lighting.
    environment: Option<EnvironmentFrame>,
    /// The view's ambient occlusion (ADR 0058), computed after the depth prepass.
    ao: Option<Texture>,
    /// The local lights' shadow atlas, when the shading looks it up (ADR 0071).
    local_atlas: Option<Texture>,
    /// The sun's contact shadows (ADR 0071), computed after the depth prepass.
    contact: Option<Texture>,
    /// Reflections' inputs (ADR 0073): normal and roughness, specular weight.
    reflect: Option<(Texture, Texture)>,
    /// With MSAA (ADR 0075): what the shading pass resolves its targets into.
    resolves: Option<MsResolves>,
}

/// The single-sampled textures a multisampled view's shading resolves into (ADR 0075).
#[derive(Debug, Clone, Copy)]
struct MsResolves {
    /// The lit image, for screen-space reflections.
    lit: Option<Texture>,
    /// Reflections' inputs: normal and roughness, specular weight (the nearest
    /// sample's), and the farthest sample's with its depth.
    reflect: Option<[Texture; 5]>,
}

/// How a shading pass uses the depth target.
#[derive(Debug, Clone, Copy)]
enum DepthUse {
    /// Tested and written.
    Write(LoadOp, StoreOp),
    /// Tested for equality with the prepass's depth.
    ReadOnly,
}

/// A shading pass with its targets; `first` starts them (clearing motion vectors), later
/// ones load them.
#[allow(clippy::too_many_arguments)]
fn shading_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    geometry: &GeometryFrame<'_>,
    materials: &MaterialFrame<'_>,
    shadow_map: Texture,
    culled: Buffer,
    visible: Buffer,
    targets: Targets,
    first: bool,
    depth: Texture,
    depth_use: DepthUse,
    instances: Option<Buffer>,
) -> PassBuilder<'a, 'g> {
    let mut pass = forward_pass(
        fg, name, geometry, materials, shadow_map, culled, visible, instances,
    )
    .color(
        targets.color,
        if first {
            targets.color_load
        } else {
            LoadOp::Load
        },
        StoreOp::Store,
    );
    let resolves = targets.resolves;
    if let Some(lit) = resolves.and_then(|r| r.lit) {
        pass = pass.resolve(lit, ResolveMode::Average);
    }
    if let Some(velocity) = targets.velocity {
        let load = if first {
            LoadOp::Clear([0.0; 4])
        } else {
            LoadOp::Load
        };
        pass = pass.color(velocity, load, StoreOp::Store);
    }
    if let Some((normal_roughness, weight)) = targets.reflect {
        // Cleared to zero where nothing writes them (the sky, subsurface materials): no
        // reflections there.
        let load = if first {
            LoadOp::Clear([0.0; 4])
        } else {
            LoadOp::Load
        };
        pass =
            pass.color(normal_roughness, load, StoreOp::Store)
                .color(weight, load, StoreOp::Store);
    }
    if let Some(previous) = targets.previous {
        pass = pass.read(previous, Access::StorageRead(ShaderStages::VERTEX));
    }
    if let Some(tiles) = targets.light_tiles {
        pass = pass.read(tiles, Access::StorageRead(ShaderStages::FRAGMENT));
    }
    if let Some(env) = targets.environment {
        pass = pass
            .read(env.prefiltered, Access::Sampled(ShaderStages::FRAGMENT))
            .read(env.dfg, Access::Sampled(ShaderStages::FRAGMENT))
            .read(env.sh, Access::StorageRead(ShaderStages::FRAGMENT));
    }
    if let Some(ao) = targets.ao {
        pass = pass.read(ao, Access::Sampled(ShaderStages::FRAGMENT));
    }
    if let Some(atlas) = targets.local_atlas {
        pass = pass.read(atlas, Access::Sampled(ShaderStages::FRAGMENT));
    }
    if let Some(contact) = targets.contact {
        pass = pass.read(contact, Access::Sampled(ShaderStages::FRAGMENT));
    }
    match depth_use {
        DepthUse::Write(load, store) => pass.depth(depth, load, store),
        DepthUse::ReadOnly => pass.depth_read_only(depth),
    }
}

/// A culling pass of `phase` with its buffer accesses (`flags`: occlusion culling's
/// visibility flags, read early and rewritten late).
#[allow(clippy::too_many_arguments)]
fn cull_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    bindless: &'g Bindless,
    name: &'static str,
    culled: Buffer,
    visible: Buffer,
    flags: Option<Buffer>,
    phase: u32,
    instances: Option<Buffer>,
) -> ComputePass<'a, 'g> {
    let pass = fg
        .add_compute(name, bindless)
        .read(culled, Access::StorageReadWrite(ShaderStages::COMPUTE))
        .write(visible, Access::StorageWrite(ShaderStages::COMPUTE))
        .maybe_read(instances, Access::StorageRead(ShaderStages::COMPUTE));
    match flags {
        Some(flags) if phase == PHASE_LATE => {
            pass.read(flags, Access::StorageReadWrite(ShaderStages::COMPUTE))
        }
        Some(flags) => pass.read(flags, Access::StorageRead(ShaderStages::COMPUTE)),
        None => pass,
    }
}

/// A depth prepass phase that draws the culled commands, without its attachment.
#[allow(clippy::too_many_arguments)]
fn prepass_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    geometry: &GeometryFrame<'_>,
    materials: &MaterialFrame<'_>,
    culled: Buffer,
    visible: Buffer,
    instances: Option<Buffer>,
) -> PassBuilder<'a, 'g> {
    materials
        .read(geometry.read(fg.add_pass(name)))
        .read(culled, Access::Indirect)
        .read(visible, Access::StorageRead(ShaderStages::VERTEX))
        .maybe_read(instances, Access::StorageRead(ShaderStages::VERTEX))
}

/// A forward pass that draws the culled commands, without its attachments.
#[allow(clippy::too_many_arguments)]
fn forward_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    geometry: &GeometryFrame<'_>,
    materials: &MaterialFrame<'_>,
    shadow_map: Texture,
    culled: Buffer,
    visible: Buffer,
    instances: Option<Buffer>,
) -> PassBuilder<'a, 'g> {
    materials
        .read(geometry.read(fg.add_pass(name)))
        .read(shadow_map, Access::Sampled(ShaderStages::FRAGMENT))
        .read(culled, Access::Indirect)
        .read(visible, Access::StorageRead(ShaderStages::VERTEX))
        .maybe_read(instances, Access::StorageRead(ShaderStages::VERTEX))
}

/// Whether a transform mirrors (negative determinant of its linear part).
pub(crate) fn mirrored(m: &Mat4) -> bool {
    m.x_axis
        .truncate()
        .cross(m.y_axis.truncate())
        .dot(m.z_axis.truncate())
        < 0.0
}

/// A batch's pipeline state; `subsurface`: whether subsurface materials get theirs.
pub(crate) fn state(kind: MaterialKind, mirrored: bool, subsurface: bool) -> usize {
    // Masked and blended exclude each other (both come from the alpha mode), blended and
    // subsurface too (the material table never sets both).
    (usize::from(kind.subsurface && subsurface) * SUBSURFACE)
        | (usize::from(kind.blended) * BLENDED)
        | (usize::from(kind.masked) * MASKED)
        | (usize::from(kind.double_sided) * DOUBLE_SIDED)
        | (usize::from(mirrored) * MIRRORED)
}

/// Fills `slots` with every draw's material slot and `runs` with the runs of consecutive
/// draws that share a mesh and a pipeline state; counts the runs per state. `subsurface`:
/// whether subsurface materials get their own states.
fn batches(
    draws: &[DrawItem],
    materials: &MaterialFrame<'_>,
    slots: &mut Vec<u32>,
    runs: &mut Vec<Run>,
    counts: &mut [u32; STATES],
    subsurface: bool,
) {
    slots.clear();
    runs.clear();
    let mut mesh = None;
    for (i, draw) in draws.iter().enumerate() {
        let (slot, kind) = materials.resolve(draw.material);
        let state = state(kind, mirrored(&draw.world_from_object), subsurface);
        slots.push(slot);
        match runs.last_mut() {
            Some(run) if run.state == state && mesh == Some(draw.mesh) => run.len += 1,
            _ => {
                runs.push(Run {
                    first: i as u32,
                    len: 1,
                    state,
                    order: 0,
                    command: 0,
                    slot: 0,
                    lods: 1,
                });
                counts[state] += 1;
                // A blended draw is a batch of its own (no mesh to continue): one command
                // per instance keeps them in back-to-front order through culling.
                mesh = (state & BLENDED == 0).then_some(draw.mesh);
            }
        }
    }
}

/// Where each pipeline state's batches are among the commands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct States {
    starts: [u32; STATES],
    counts: [u32; STATES],
}

impl States {
    /// Records one indirect draw per pipeline state in use, from the commands at `offset`
    /// in `buffer` (one per batch, in state order), into `area` of the target: `pipelines`
    /// are the opaque and the masked one. Only the states whose bits `mask` sets are drawn
    /// (`*_STATES`). The shadow pass renders back faces of single-sided
    /// geometry (fewer self-shadowing artifacts on closed meshes), except for subsurface
    /// materials (their front faces, where light enters, for thick transmission); the
    /// forward passes cull them.
    ///
    /// # Safety
    ///
    /// `ctx` records inside a rendering scope of a pass that reads the geometry pool and
    /// the material table; `buffer` holds the commands and has indirect-buffer usage.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn draw(
        &self,
        ctx: &crate::graph::PassContext<'_>,
        bindless: &Bindless,
        geometry: &GeometryFrame<'_>,
        pipelines: [&Pipeline; 2],
        push: &Push,
        shadow: bool,
        mask: u32,
        buffer: vk::Buffer,
        offset: u64,
        area: vk::Rect2D,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.draw_slice(
                ctx,
                bindless,
                geometry,
                pipelines,
                push,
                shadow,
                mask,
                buffer,
                offset,
                area,
                (0, 1),
            );
        }
    }

    /// [`Self::draw`] for one slice of the commands: with `(k, n)`, every `n`-th command of
    /// each state from its `k`-th (texture feedback spreads its draws over `n` frames).
    ///
    /// # Safety
    ///
    /// As [`Self::draw`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn draw_slice(
        &self,
        ctx: &crate::graph::PassContext<'_>,
        bindless: &Bindless,
        geometry: &GeometryFrame<'_>,
        pipelines: [&Pipeline; 2],
        push: &Push,
        shadow: bool,
        mask: u32,
        buffer: vk::Buffer,
        offset: u64,
        area: vk::Rect2D,
        (slice, slices): (u32, u32),
    ) {
        let slices = slices.max(1);
        let mut bound = None;
        for state in 0..STATES {
            let count = self.counts[state].saturating_sub(slice).div_ceil(slices);
            if count == 0 || mask & (1 << state) == 0 {
                continue;
            }
            let masked = state & MASKED != 0;
            let cull = match (state & DOUBLE_SIDED != 0, shadow) {
                (true, _) => vk::CullModeFlags::NONE,
                // Subsurface materials cast from the faces towards the light: the shadow
                // map then holds where light enters them, for thick transmission.
                (false, true) if state & SUBSURFACE != 0 => vk::CullModeFlags::BACK,
                (false, true) => vk::CullModeFlags::FRONT,
                (false, false) => vk::CullModeFlags::BACK,
            };
            let front = if state & MIRRORED != 0 {
                vk::FrontFace::CLOCKWISE
            } else {
                vk::FrontFace::COUNTER_CLOCKWISE
            };
            // SAFETY: guaranteed by the caller.
            unsafe {
                if bound != Some(masked) {
                    pipelines[usize::from(masked)].bind(ctx.cmd);
                    if bound.is_none() {
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        set_viewport(ctx.device, ctx.cmd, area);
                        bindless.push_constants(ctx.cmd, push_bytes(push));
                        geometry.bind_indices(ctx);
                    }
                    bound = Some(masked);
                }
                ctx.device.cmd_set_cull_mode(ctx.cmd, cull);
                ctx.device.cmd_set_front_face(ctx.cmd, front);
                ctx.device.cmd_draw_indexed_indirect(
                    ctx.cmd,
                    buffer,
                    offset + u64::from(self.starts[state] + slice) * COMMAND_BYTES,
                    count,
                    COMMAND_BYTES as u32 * slices,
                );
            }
        }
    }
}

/// The shader's view of the frame's cascades: projections as scale and offset, atlas
/// tiles, selection limits and caster bounds.
fn cascade_data(fit: &CascadeFit, settings: &ShadowSettings) -> [CascadeData; MAX_CASCADES] {
    let (columns, rows) = settings.atlas_grid();
    let mut out = [CascadeData::default(); MAX_CASCADES];
    for (k, (data, cascade)) in out.iter_mut().zip(&fit.cascades[..fit.count]).enumerate() {
        let m = cascade.clip_from_light();
        let tile = [(k as u32 % columns) as f32, (k as u32 / columns) as f32];
        let grid = [columns as f32, rows as f32];
        *data = CascadeData {
            scale: [m.x_axis.x, m.y_axis.y, m.z_axis.z, cascade.texel],
            offset: [m.w_axis.x, m.w_axis.y, m.w_axis.z, cascade.limit],
            atlas: [
                0.5 / grid[0],
                0.5 / grid[1],
                (0.5 + tile[0]) / grid[0],
                (0.5 + tile[1]) / grid[1],
            ],
            bounds: [
                cascade.center.x,
                cascade.center.y,
                cascade.radius,
                cascade.far,
            ],
        };
    }
    out
}

/// A mesh's object-space bounding box as center and half extents (empty for removed
/// meshes).
fn local_box(geometry: &GeometryFrame<'_>, mesh: MeshId) -> (Vec3, Vec3) {
    geometry.mesh(mesh).map_or((Vec3::ZERO, Vec3::ZERO), |m| {
        ((m.min + m.max) * 0.5, (m.max - m.min) * 0.5)
    })
}

/// # Safety
///
/// `cmd` is recording.
unsafe fn set_viewport(device: &engine_rhi::ash::Device, cmd: vk::CommandBuffer, area: vk::Rect2D) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_set_viewport(
            cmd,
            0,
            &[vk::Viewport {
                x: area.offset.x as f32,
                y: area.offset.y as f32,
                width: area.extent.width as f32,
                height: area.extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
        device.cmd_set_scissor(cmd, 0, &[area]);
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
