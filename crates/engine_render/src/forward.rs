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
use crate::cull::Frustum;
use crate::environment::{Environment, EnvironmentFrame, PREFILTERED_LEVELS};
use crate::gpu_scene::SceneFrame;
use crate::graph::{
    Access, Buffer, BufferDesc, BufferState, FrameGraph, ImportedBuffer, LoadOp, PassBuilder,
    ShaderStages, StoreOp, Texture, TextureDesc,
};
use crate::hiz::{self, DepthPyramid, PyramidLayout};
use crate::lights::{
    LightData, LightLayout, ViewAxis, Z_BINS, cull_lights, write_bins, write_lights, write_rects,
};
use crate::material::{MaterialFrame, MaterialId, MaterialKind};
use crate::mesh::{GeometryFrame, MeshId};
use crate::reflections::{
    NORMAL_ROUGHNESS_FORMAT, ProbeTextures, ReflectionPasses, ReflectionProbes, ViewInputs,
    ViewSetup, WEIGHT_FORMAT,
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
const FRAGMENT_MOTION_AO: &[u8] = include_bytes!("../shaders/forward.fragmentMainMotionAo.spv");
const FRAGMENT_MASKED_MOTION: &[u8] = include_bytes!("../shaders/forward.fragmentMaskedMotion.spv");
const BACKGROUND_VERTEX: &[u8] = include_bytes!("../shaders/forward.backgroundVertex.spv");
const BACKGROUND_MOTION: &[u8] = include_bytes!("../shaders/forward.backgroundMotion.spv");
const SKY_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.skyFragment.spv");
const FRAGMENT_SUBSURFACE: &[u8] = include_bytes!("../shaders/forward.fragmentSubsurface.spv");
const FRAGMENT_SUBSURFACE_MOTION: &[u8] =
    include_bytes!("../shaders/forward.fragmentSubsurfaceMotion.spv");
const SCATTER_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.scatterFragment.spv");
const FRAGMENT_REFLECT: &[u8] = include_bytes!("../shaders/forward.fragmentReflect.spv");
const FRAGMENT_REFLECT_AO: &[u8] = include_bytes!("../shaders/forward.fragmentReflectAo.spv");
const FRAGMENT_REFLECT_MOTION: &[u8] =
    include_bytes!("../shaders/forward.fragmentReflectMotion.spv");
const FRAGMENT_REFLECT_MOTION_AO: &[u8] =
    include_bytes!("../shaders/forward.fragmentReflectMotionAo.spv");

pub use crate::lights::{LocalLight, Spot};
pub use crate::reflections::Reflections;
pub use crate::shadow::ShadowSettings;

/// Format of the motion vector target (ADR 0043).
pub const MOTION_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;
const DEPTH_VERTEX_MASKED: &[u8] = include_bytes!("../shaders/forward.depthVertexMasked.spv");
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
const BLENDED_STATES: u32 = 0x0000_FF00;
const SUBSURFACE_STATES: u32 = 0x00FF_0000;
/// Everything that writes depth and casts shadows.
const DEPTH_STATES: u32 = PLAIN_STATES | SUBSURFACE_STATES;

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
/// instance transforms, from which the forward pass writes motion vectors, and the
/// sub-pixel jitter sequence of the projection.
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
            reset: Cell::new(true),
            state: Cell::new(BufferState::idle()),
        })
    }

    /// Jitters the projection through `phases` Halton(2, 3) offsets (0: no jitter). TAA
    /// wants at least 8, upscaling by `s` per axis ⌈8 · s²⌉.
    pub fn set_jitter_phases(&self, phases: u32) {
        self.phases.set(phases);
    }

    /// The jitter of the next frame, in pixels.
    pub fn next_jitter(&self) -> [f32; 2] {
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

/// `Frame` in `forward.slang` (1024 bytes).
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
    _pad5: [u32; 2],
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
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    frame: u64,
    instances: u64,
    batches: u64,
    visible: u64,
    commands: u64,
    visibility: u64,
    pyramid: u64,
    positions: u64,
    attributes: u64,
    materials: u64,
    previous_instances: u64,
    light_tiles: u64,
    instance_count: u32,
    texture: u32,
    phase: u32,
    /// Bindless index of the view's ambient occlusion, or [`NO_AO`].
    ao: u32,
}

/// `NO_TEXTURE` in `forward.slang`: no ambient occlusion.
const NO_AO: u32 = u32::MAX;

const _: () = assert!(core::mem::size_of::<CascadeData>() == 64);
const _: () = assert!(core::mem::size_of::<FrameData>() == 1024);
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
}

/// Which instance-slot region command list `list` uses: the forward phases' lists
/// (`0..phases`) have their own regions, and each cascade's shadow list (`phases + k`) has
/// region `k` (the shadow lists are culled apart from the forward ones).
fn list_offset(list: u32, phases: u32) -> u32 {
    if list < phases { list } else { list - phases }
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
    /// Forward shading that also writes motion vectors: opaque, masked, and after the
    /// prepass.
    main_motion: [Pipeline; 2],
    shade_motion: Pipeline,
    /// Transparent surfaces: premultiplied blending, depth tested but not written; without
    /// and with the reactive mask.
    blended: [Pipeline; 2],
}

/// Ambient occlusion (ADR 0058): its passes, and the shading that reads it after the depth
/// prepass (`fragmentMainAo`, `fragmentMainMotionAo`), per shading variant: without and
/// with motion vectors. The other entry points have no code for it (see `shade` in
/// `forward.slang`).
#[derive(Debug)]
struct AoPasses {
    passes: AmbientOcclusion,
    shading: [[Pipeline; 2]; 4],
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
        let fragment_motion = spirv_words(FRAGMENT_MOTION_AO).ok_or_else(invalid)?;
        let formats = [color_format];
        let motion_formats = [color_format, MOTION_FORMAT];
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
            .flat_map(|v| {
                [
                    shade(&fragment, c"fragmentMainAo", &formats[..], v),
                    shade(
                        &fragment_motion,
                        c"fragmentMainMotionAo",
                        &motion_formats[..],
                        v,
                    ),
                ]
            })
            .map(PipelineDesc::Graphics)
            .collect();
        let mut pipelines =
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "ambient occlusion pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let mut pair = || -> Result<[Pipeline; 2], RhiError> { Ok([next()?, next()?]) };
        let shading = [pair()?, pair()?, pair()?, pair()?];
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
    /// light and albedo): without and with motion vectors.
    shading: [[Pipeline; 2]; 4],
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
        let fragment_motion = spirv_words(FRAGMENT_SUBSURFACE_MOTION).ok_or_else(invalid)?;
        let scatter = spirv_words(SCATTER_FRAGMENT).ok_or_else(invalid)?;
        let formats = [color_format, IRRADIANCE_FORMAT, ALBEDO_FORMAT];
        let motion_formats = [
            color_format,
            MOTION_FORMAT,
            IRRADIANCE_FORMAT,
            ALBEDO_FORMAT,
        ];
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
        let mut descs = Vec::with_capacity(2 * VARIANTS.len() + 1);
        for variant in &VARIANTS {
            descs.push(shade(
                &fragment,
                c"fragmentSubsurface",
                &formats[..],
                variant,
            ));
            descs.push(shade(
                &fragment_motion,
                c"fragmentSubsurfaceMotion",
                &motion_formats[..],
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
        let mut pair = || -> Result<[Pipeline; 2], RhiError> { Ok([next()?, next()?]) };
        let shading = [pair()?, pair()?, pair()?, pair()?];
        Ok(Self {
            shading,
            scatter: next()?,
        })
    }
}

/// Reflections (ADR 0073): their passes, and per shading variant with an environment
/// (without and with local lights) the shading of views with them, after the depth
/// prepass, which also writes the normal, roughness and specular weight:
/// `[lights][motion][ambient occlusion]`.
#[derive(Debug)]
struct ReflectionShading {
    passes: ReflectionPasses,
    shading: [[[Pipeline; 2]; 2]; 2],
}

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
        let words = |spirv| spirv_words(spirv).ok_or_else(invalid);
        // [motion][ambient occlusion].
        let fragments = [
            (words(FRAGMENT_REFLECT)?, c"fragmentReflect"),
            (words(FRAGMENT_REFLECT_AO)?, c"fragmentReflectAo"),
            (words(FRAGMENT_REFLECT_MOTION)?, c"fragmentReflectMotion"),
            (
                words(FRAGMENT_REFLECT_MOTION_AO)?,
                c"fragmentReflectMotionAo",
            ),
        ];
        let formats = [color_format, NORMAL_ROUGHNESS_FORMAT, WEIGHT_FORMAT];
        let motion_formats = [
            color_format,
            MOTION_FORMAT,
            NORMAL_ROUGHNESS_FORMAT,
            WEIGHT_FORMAT,
        ];
        // The environment variants (2, 3): without and with local lights; after the
        // prepass, tested for equality.
        let mut descs = Vec::with_capacity(8);
        for variant in [2, 3] {
            for (k, (fragment, entry)) in fragments.iter().enumerate() {
                let color_formats: &[vk::Format] = if k >= 2 { &motion_formats } else { &formats };
                descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
                    depth_write: false,
                    depth_compare: vk::CompareOp::EQUAL,
                    ..shading_desc(
                        &vertex,
                        fragment,
                        entry,
                        color_formats,
                        depth_format,
                        &VARIANTS[variant],
                    )
                }));
            }
        }
        let mut pipelines =
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "reflection shading pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let mut lights = || -> Result<[[Pipeline; 2]; 2], RhiError> {
            Ok([[next()?, next()?], [next()?, next()?]])
        };
        let shading = [lights()?, lights()?];
        Ok(Self {
            passes: ReflectionPasses::new(device, bindless, color_format)?,
            shading,
        })
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
    /// Reflections' passes and shading pipelines (ADR 0073), made when first turned on.
    reflections: Option<ReflectionShading>,
    reflections_on: bool,
    scratch: RefCell<Scratch>,
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
        let mut descs = Vec::with_capacity(8 * VARIANTS.len() + 11);
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
                main(
                    &fragment_motion,
                    c"fragmentMainMotion",
                    &motion_formats,
                    true,
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
                shade_motion: next()?,
                blended: [next()?, next()?],
            })
        };
        let shading = [shading()?, shading()?, shading()?, shading()?];
        let background = next()?;
        let sky = next()?;
        let prepass = [next()?, next()?];
        let shadow = [next()?, next()?];
        let composite = next()?;
        let blended_atmosphere = [[next()?, next()?], [next()?, next()?]];
        Ok(Self {
            shading,
            prepass,
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
            reflections: None,
            reflections_on: false,
            scratch: RefCell::new(Scratch::default()),
        })
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
        Ok(())
    }

    /// The ambient occlusion settings, when it is on.
    pub fn ambient_occlusion(&self) -> Option<AoSettings> {
        self.ao_settings
    }

    /// Turns local reflections (ADR 0073, [`crate::reflections`]) on or off (the
    /// default): views with [`ForwardView::reflections`] then use their probes and
    /// screen-space reflections. The first time they are turned on their passes and 8
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
            self.reflections = Some(ReflectionShading::new(
                device,
                bindless,
                self.color_format,
                self.depth_format,
            )?);
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
                [
                    r.shading[0][0][0].statistics(device),
                    r.shading[1][0][0].statistics(device),
                    a,
                    b,
                    c,
                ]
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
        Ok(())
    }

    /// The driver's statistics of the opaque forward pipeline of each shading variant (see
    /// `ForwardRenderer::shading`): without and with local lights, then the same with an
    /// environment. Empty unless the device captures statistics
    /// ([`Device::pipeline_statistics_enabled`]).
    pub fn shading_statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        core::array::from_fn(|k| self.shading[k].main[0].statistics(device))
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
        let setup = self.setup(fg, view);
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
        let layout = self.layout_draws(ring, geometry, materials, draws, view, &setup);
        let passes = layout.and_then(|layout| {
            self.declare(
                fg, ring, bindless, geometry, materials, view, &setup, &layout, &imported,
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
        let setup = self.setup(fg, view);
        let imported = self.import(fg, ring, bindless, view, &setup)?;
        if let Some((probes, textures)) = imported.probes {
            // What moved, appeared or went away in the scene asks the probes that see it
            // for a new capture.
            probes.note_changes(scene.changed_boxes());
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
        let layout = self.layout_scene(ring, geometry, scene, view, &setup);
        let passes = layout.and_then(|layout| {
            self.declare(
                fg, ring, bindless, geometry, materials, view, &setup, &layout, &imported,
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
                capture: true,
            };
            if laid_out.is_none() {
                laid_out = Some(layout(self, ring, &capture, &setup)?);
            }
            let laid = laid_out.as_ref()?;
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
            capture: false,
        }
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
            ..
        } = *setup;
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
            let (out, staged) = ring.alloc::<vk::DrawIndexedIndirectCommand>(
                (phases + cascade_count) as usize * per_list,
            )?;
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
            for list in 0..phases + cascade_count {
                let slots = list_offset(list, phases) * slot_count;
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
            ..
        } = *setup;
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
            let (out, staged) = ring.alloc::<vk::DrawIndexedIndirectCommand>(
                (phases + cascade_count) as usize * per_list,
            )?;
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
            for list in 0..phases + cascade_count {
                let slots = list_offset(list, phases) * slot_count;
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
            capture,
        } = *setup;
        let occlusion = view.occlusion.filter(|_| phases == 2);
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
        // Subsurface profiles, for transmission and the scatter pass (ADR 0059).
        let profiles = if subsurface_draws {
            ring.push(&self.subsurface_profiles)?
        } else {
            0
        };
        // Reflections (ADR 0073): with an environment (their last fallback), in perspective
        // views, never in a probe's capture. Their constants (and visible probes) go to the
        // ring before the shading that reads them.
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
        let settings = &self.shadow_settings;
        let fit = shadow::fit_cascades(
            unjittered,
            view.camera.position,
            view.light.direction,
            min,
            max,
            settings,
        );
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
            lod_scale: {
                // Pixels per unit at distance 1: the projection's vertical scale (the length
                // of the clip matrix's y row, whatever the camera's rotation) times half
                // the height. Orthographic views (no perspective divide) draw full meshes.
                let (y_row, w_row) = (unjittered.row(1).truncate(), unjittered.row(3).truncate());
                if self.lod_threshold > 0.0 && w_row.length() > 1e-6 {
                    y_row.length() * depth_desc.height as f32 * 0.5 / self.lod_threshold
                } else {
                    0.0
                }
            },
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
            _pad5: [0; 2],
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

        // Shadow casters of each cascade, then each cascade drawn into its atlas tile.
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
        let geometry = *geometry;
        let materials = *materials;
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
        // Motion vectors: a second target, last frame's transforms read by the vertex
        // shader, and this frame's copied for the next one afterwards.
        let velocity = temporal.map(|_| {
            fg.create_texture(
                "motion vectors",
                TextureDesc::new_2d(MOTION_FORMAT, depth_desc.width, depth_desc.height),
            )
        });
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
            reflect: None,
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
        targets.reflect = reflecting.then(|| {
            let desc = |format| TextureDesc::new_2d(format, depth_desc.width, depth_desc.height);
            (
                fg.create_texture("reflection normals", desc(NORMAL_ROUGHNESS_FORMAT)),
                fg.create_texture("reflection weights", desc(WEIGHT_FORMAT)),
            )
        });
        // Subsurface materials and reflections need it too: their passes follow the
        // forward pass, or read the depth before it.
        let prepass = self.depth_prepass || ao.is_some() || subsurface_draws || reflecting;
        let prepass_pipelines = [&self.prepass[0], &self.prepass[1]];
        // After the prepass every state shades with the equal-depth pipeline: masked
        // materials were alpha-tested there, and nothing is left to discard.
        let variant = usize::from(light_layout.count > 0) | usize::from(environment.is_some()) << 1;
        let shading = &self.shading[variant];
        // With ambient occlusion, the shading that reads it (the prepass is on then).
        let ao_shading = ao.map(|(_, passes)| &passes.shading[variant]);
        // With reflections, the shading that also writes their inputs (the prepass is on
        // then).
        let reflect_shading = reflection_passes.filter(|_| reflecting).map(|r| {
            &r.shading[variant & 1][usize::from(velocity.is_some())][usize::from(ao.is_some())]
        });
        let main_pipelines = match (prepass, velocity.is_some()) {
            (true, _) if reflect_shading.is_some() => {
                let p = reflect_shading?;
                [p, p]
            }
            (true, false) => match ao_shading {
                Some(p) => [&p[0], &p[0]],
                None => [&shading.shade, &shading.shade],
            },
            (true, true) => match ao_shading {
                Some(p) => [&p[1], &p[1]],
                None => [&shading.shade_motion, &shading.shade_motion],
            },
            (false, false) => [&shading.main[0], &shading.main[1]],
            (false, true) => [&shading.main_motion[0], &shading.main_motion[1]],
        };
        let draw_phase = move |ctx: &mut crate::graph::PassContext<'_>,
                               pipelines: [&Pipeline; 2],
                               phase: u32,
                               mask: u32,
                               ao: Option<Texture>| {
            let push = Push {
                visible: ctx.address(visible),
                texture: ctx.sampled_index(shadow_map).unwrap_or(0),
                light_tiles: light_tiles.map_or(0, |b| ctx.address(b)),
                ao: ao.and_then(|t| ctx.sampled_index(t)).unwrap_or(NO_AO),
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
        let passes = match flag_buffer {
            None => {
                let depth_use = if prepass {
                    prepass_pass(
                        fg,
                        names.depth_prepass,
                        &geometry,
                        &materials,
                        culled,
                        visible,
                        scene_instances,
                    )
                    .depth(depth, depth_clear, StoreOp::Store)
                    .execute(move |ctx| {
                        draw_phase(ctx, prepass_pipelines, PHASE_ALL, DEPTH_STATES, None)
                    });
                    DepthUse::ReadOnly
                } else {
                    DepthUse::Write(depth_clear, depth_store)
                };
                if let Some(((settings, view_space), passes)) = &ao {
                    targets.ao = Some(
                        passes
                            .passes
                            .add_passes(fg, bindless, depth, view_space, settings, ao_frame),
                    );
                }
                let ao_texture = targets.ao;
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
                    draw_phase(ctx, main_pipelines, PHASE_ALL, PLAIN_STATES, ao_texture)
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
                }
            }
            Some(flags) => {
                // Two phases: draw what was visible, build the pyramid from its depth, then
                // test everything against it and draw what the first phase missed. With the
                // prepass the phases draw depth only, and the forward pass shades both
                // lists once.
                if prepass {
                    prepass_pass(
                        fg,
                        "depth prepass early",
                        &geometry,
                        &materials,
                        culled,
                        visible,
                        scene_instances,
                    )
                    .depth(depth, depth_clear, StoreOp::Store)
                    .execute(move |ctx| {
                        draw_phase(ctx, prepass_pipelines, PHASE_EARLY, DEPTH_STATES, None)
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
                        draw_phase(ctx, main_pipelines, PHASE_EARLY, PLAIN_STATES, None)
                    });
                }
                let pyramid = fg.create_buffer(
                    "depth pyramid",
                    BufferDesc {
                        size: pyramid_layout.bytes(),
                    },
                );
                self.pyramid.add_pass(fg, bindless, depth, pyramid);
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
                    prepass_pass(
                        fg,
                        "depth prepass late",
                        &geometry,
                        &materials,
                        culled,
                        visible,
                        scene_instances,
                    )
                    .depth(depth, LoadOp::Load, StoreOp::Store)
                    .execute(move |ctx| {
                        draw_phase(ctx, prepass_pipelines, PHASE_LATE, DEPTH_STATES, None)
                    });
                    if let Some(((settings, view_space), passes)) = &ao {
                        targets.ao = Some(
                            passes
                                .passes
                                .add_passes(fg, bindless, depth, view_space, settings, ao_frame),
                        );
                    }
                    let ao_texture = targets.ao;
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
                        draw_phase(ctx, main_pipelines, PHASE_EARLY, PLAIN_STATES, ao_texture);
                        draw_phase(ctx, main_pipelines, PHASE_LATE, PLAIN_STATES, ao_texture);
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
                        draw_phase(ctx, main_pipelines, PHASE_LATE, PLAIN_STATES, None)
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
                }
            }
        };
        // Subsurface materials (ADR 0059): shaded after the others, also writing their
        // diffuse light and albedo, then scattered into the color target.
        if let Some(pipelines) = subsurface.filter(|_| subsurface_draws) {
            let variant =
                usize::from(light_layout.count > 0) | usize::from(environment.is_some()) << 1;
            let irradiance = fg.create_texture(
                "subsurface irradiance",
                TextureDesc::new_2d(IRRADIANCE_FORMAT, depth_desc.width, depth_desc.height),
            );
            let albedo = fg.create_texture(
                "subsurface albedo",
                TextureDesc::new_2d(ALBEDO_FORMAT, depth_desc.width, depth_desc.height),
            );
            let pipeline = &pipelines.shading[variant][usize::from(velocity.is_some())];
            let two_phases = flag_buffer.is_some();
            // Their diffuse light is occluded by the view's ambient occlusion too.
            let ao_texture = targets.ao;
            // They keep the sky's reflections (their pass writes no reflection inputs).
            let targets = Targets {
                reflect: None,
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
                    draw_phase(ctx, pipelines, PHASE_EARLY, SUBSURFACE_STATES, ao_texture);
                    draw_phase(ctx, pipelines, PHASE_LATE, SUBSURFACE_STATES, ao_texture);
                } else {
                    draw_phase(ctx, pipelines, PHASE_ALL, SUBSURFACE_STATES, ao_texture);
                }
            });
            // The scatter pass: the same batches, adding where their depth is.
            let scatter = &pipelines.scatter;
            forward_pass(
                fg,
                names.subsurface_scatter,
                &geometry,
                &materials,
                shadow_map,
                culled,
                visible,
                scene_instances,
            )
            .read(irradiance, Access::Sampled(ShaderStages::FRAGMENT))
            .read(albedo, Access::Sampled(ShaderStages::FRAGMENT))
            .color(color, LoadOp::Load, StoreOp::Store)
            .depth_read_only(depth)
            .execute(move |ctx| {
                let (Some(light), Some(albedo)) =
                    (ctx.sampled_index(irradiance), ctx.sampled_index(albedo))
                else {
                    return;
                };
                let push = Push {
                    visible: ctx.address(visible),
                    texture: light,
                    phase: albedo,
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
        // specular light on the opaque image.
        if let (Some(r), Some(prepared), Some(env), Some((normal_roughness, weight))) =
            (reflection_passes, prepared, environment, targets.reflect)
        {
            if let Some((probes, _)) = imported.probes.filter(|_| prepared.probes > 0) {
                probes.mark_used(true, false);
            }
            r.passes.add_view_passes(
                fg,
                bindless,
                &prepared,
                &ViewInputs {
                    color,
                    depth,
                    normal_roughness,
                    weight,
                    environment: env,
                    probes: imported.probes.map(|(_, t)| t),
                },
            );
        }
        // With an atmosphere or fog: the sky, aerial perspective and fog over the opaque
        // image (ADR 0069). A probe's capture fills its sky in its face copy instead.
        if capture {
        } else if let (Some(env), Some(atmosphere)) = (environment, atmosphere) {
            crate::atmosphere::add_composite_pass(
                fg,
                bindless,
                &self.composite,
                color,
                depth,
                &env,
                &atmosphere,
            );
        } else if let Some(env) = environment {
            // The sky where no geometry was drawn (depth still 1).
            let sky = &self.sky;
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
        let reactive = (blended_commands > 0 && velocity.is_some()).then(|| {
            fg.create_texture(
                "reactive mask",
                TextureDesc::new_2d(REACTIVE_FORMAT, depth_desc.width, depth_desc.height),
            )
        });
        if blended_commands > 0 {
            let reactive_index = usize::from(reactive.is_some());
            let pipelines = match atmosphere {
                // Under an atmosphere or fog (the environment variants).
                Some(_) => &self.blended_atmosphere[variant & 1][reactive_index],
                None => &shading.blended[reactive_index],
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
        let background = &self.background;
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
                reactive,
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
    /// A reflection probe's face (ADR 0073): no ambient occlusion, reflections or sky
    /// pass; passes named apart.
    capture: bool,
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
    let velocity = temporal.map(|_| {
        let desc = fg.texture_desc(depth);
        fg.create_texture(
            "motion vectors",
            TextureDesc::new_2d(MOTION_FORMAT, desc.width, desc.height),
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
    pass.depth(depth, depth_clear, depth_store).execute(|_| {});
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
    /// Reflections' inputs (ADR 0073): normal and roughness, specular weight.
    reflect: Option<(Texture, Texture)>,
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
#[derive(Debug, Clone, Copy)]
struct States {
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
    unsafe fn draw(
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
        let mut bound = None;
        for state in 0..STATES {
            let count = self.counts[state];
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
                    offset + u64::from(self.starts[state]) * COMMAND_BYTES,
                    count,
                    COMMAND_BYTES as u32,
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
