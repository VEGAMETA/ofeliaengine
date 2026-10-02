//! Local reflections (ADR 0073, docs/research/reflections.md): reflection probes and
//! screen-space reflections, replacing the sky's specular light where they know better.
//!
//! - [`ReflectionProbes`]: placed boxes, each a cube captured from a point by the forward
//!   renderer itself (the scene drawn into six faces), GGX-prefiltered on the GPU into one
//!   cube array and kept until something asks for a new capture: an edit of the probe,
//!   [`ReflectionProbes::refresh`], a change of the sky or the sun, or, on the persistent
//!   GPU scene path, an instance moving, appearing or going inside its box. Captures
//!   follow a budget of faces per frame. A pixel blends the probes whose influence box
//!   holds it (smallest first, fading over each one's blend distance), with the reflected
//!   ray corrected for parallax against the probe's box (Lagarde and Zanuttini 2012); the
//!   rest comes from the sky.
//! - [`ScreenSpaceReflections`]: a view's screen-space reflections: rays traced through a
//!   pyramid of nearest depths against the frame's own lit image, at full or half
//!   resolution, stochastic for glossy surfaces with a spatial and a temporal filter (its
//!   history is this object's). Where a ray finds nothing on screen, the probes and the
//!   sky take over.
//!
//! A view asks for them through [`crate::forward::ForwardView::reflections`] after
//! [`crate::forward::ForwardRenderer::set_reflections`]; they need the view's environment
//! (the sky is the last fallback). Its opaque surfaces then shade with entry points that
//! also write their normal, roughness and specular weight, and one full-screen pass after
//! them adds weight × (reflection − sky). Views without reflections run exactly the
//! shading they ran before.
//!
//! ```no_run
//! # use engine_render::forward::{ForwardRenderer, ForwardView, Reflections};
//! # use engine_render::reflections::*;
//! # use engine_core::math::Vec3;
//! # fn demo(device: &engine_rhi::Device, allocator: &mut engine_rhi::Allocator,
//! #         bindless: &engine_rhi::Bindless, renderer: &mut ForwardRenderer,
//! #         view: ForwardView<'_>) -> Result<(), engine_rhi::RhiError> {
//! renderer.set_reflections(device, bindless, true)?;
//! // Once per scene: a probe per room (captured by the next frames that draw it).
//! let mut probes = ReflectionProbes::new(device, allocator, bindless, ProbeSettings::default())?;
//! probes.add(ReflectionProbe::room(Vec3::new(0.0, 2.0, 0.0), Vec3::new(5.0, 2.0, 8.0)));
//! // Once per view: its screen-space reflections at the view's size.
//! let ssr = ScreenSpaceReflections::new(device, allocator, bindless, 1920, 1080,
//!     SsrSettings::default())?;
//! // Every frame:
//! let view = ForwardView {
//!     reflections: Reflections { probes: Some(&probes), ssr: Some(&ssr) },
//!     ..view
//! };
//! # let _ = view; Ok(()) }
//! ```

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::cull::Frustum;
use crate::environment::{EnvironmentFrame, Image, ImageKind, PREFILTERED_LEVELS};
use crate::forward::{Camera, DirectionalLight};
use crate::graph::{
    Access, BufferDesc, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc,
};
use crate::upload::UploadRing;
use core::cell::{Cell, RefCell};
use engine_core::math::{Mat3, Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocator, Bindless, Blend, Device, GraphicsPipelineDesc, Pipeline, PipelineDesc, RhiError,
    SamplerKind, ShaderStageDesc, spirv_words,
};

const PROBE_FACE: &[u8] = include_bytes!("../shaders/reflections.probeFace.spv");
const PROBE_DOWNSAMPLE: &[u8] = include_bytes!("../shaders/reflections.probeDownsample.spv");
const PROBE_PREFILTER: &[u8] = include_bytes!("../shaders/reflections.probePrefilter.spv");
const COMPOSITE_VERTEX: &[u8] = include_bytes!("../shaders/reflections.compositeVertex.spv");
const COMPOSITE_PROBES: &[u8] = include_bytes!("../shaders/reflections.compositeProbes.spv");
const COMPOSITE_SSR: &[u8] = include_bytes!("../shaders/reflections.compositeSsr.spv");
const COMPOSITE_PROBES_SSR: &[u8] = include_bytes!("../shaders/reflections.compositeProbesSsr.spv");
/// The composite into multisampled targets (MSAA, ADR 0075): on the nearest surface's
/// samples only.
const COMPOSITE_MS: [(&[u8], &core::ffi::CStr); 4] = [
    (
        include_bytes!("../shaders/reflections.compositeProbesMs.spv"),
        c"compositeProbesMs",
    ),
    (
        include_bytes!("../shaders/reflections.compositeSsrMs.spv"),
        c"compositeSsrMs",
    ),
    (
        include_bytes!("../shaders/reflections.compositeProbesSsrMs.spv"),
        c"compositeProbesSsrMs",
    ),
    (
        include_bytes!("../shaders/reflections.compositeProbesFarMs.spv"),
        c"compositeProbesFarMs",
    ),
];

/// A multisampled view's composite (MSAA, ADR 0075): the pipelines
/// ([`composite_pipelines`]), the multisampled depth, the sample count, and the farthest
/// sample's depth, normal and roughness and weight (for the probes' second draw, on the
/// samples behind the nearest surface).
#[derive(Debug, Clone, Copy)]
pub(crate) struct MsComposite<'g> {
    pub pipelines: &'g [Pipeline; 4],
    pub depth: Texture,
    pub samples: u32,
    pub far: [Texture; 3],
}
const SSR_DEPTH: &[u8] = include_bytes!("../shaders/reflections.ssrDepth.spv");
const SSR_TRACE: &[u8] = include_bytes!("../shaders/reflections.ssrTrace.spv");
const SSR_SPATIAL: &[u8] = include_bytes!("../shaders/reflections.ssrSpatial.spv");
const SSR_TEMPORAL: &[u8] = include_bytes!("../shaders/reflections.ssrTemporal.spv");

/// Format of the normal (octahedral, 10 bits each) and roughness the reflection shading
/// writes.
pub const NORMAL_ROUGHNESS_FORMAT: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;
/// Format of the weight of the prefiltered radiance (split sum × specular occlusion).
pub const WEIGHT_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// Format of the probes' cubes (and of the screen-space reflections).
pub const PROBE_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// The coarse screen grid probes are listed by (`PROBE_CELLS_*` in
/// `reflections_common.slang`): a probe's bit is set in the cells its screen rectangle
/// covers.
pub const PROBE_CELLS: [u32; 2] = [16, 16];
/// Slices of view depth probes are listed by (`PROBE_BINS` in `reflections_common.slang`),
/// from the camera to the farthest visible probe.
pub const PROBE_BINS: usize = 64;
const PROBE_CELL_COUNT: usize = (PROBE_CELLS[0] * PROBE_CELLS[1]) as usize;
/// Most probes one view blends (the smallest that reach it; the rest are dropped).
pub const MAX_VISIBLE_PROBES: usize = 32;
/// Mips of a probe's cube: perceptual roughness `mip / (levels − 1)`, as the sky's.
pub const PROBE_LEVELS: u32 = PREFILTERED_LEVELS;
const PREFILTER_SAMPLES: u32 = 64;
const SSR_DISTANCE_FORMAT: vk::Format = vk::Format::R16_SFLOAT;
/// The spatial filter's hit distance and view depth per trace texel.
const SSR_DISTANCE_DEPTH_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;
/// Levels the nearest-depth pyramid has room for (targets up to 65536 on a side).
const MAX_PYRAMID_LEVELS: u32 = 16;

/// The six faces' cameras: forward and up (`face_camera` in `reflections.slang`; the faces
/// in Vulkan's cube order +X, −X, +Y, −Y, +Z, −Z).
pub const FACES: [(Vec3, Vec3); 6] = [
    (Vec3::X, Vec3::Y),
    (Vec3::NEG_X, Vec3::Y),
    (Vec3::Y, Vec3::NEG_Z),
    (Vec3::NEG_Y, Vec3::Z),
    (Vec3::Z, Vec3::Y),
    (Vec3::NEG_Z, Vec3::Y),
];

/// `ReflectPush` in `reflections.slang` (80 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    frame: u64,
    /// The probes' tile masks, or the SSR's depth pyramid.
    buffer: u64,
    t: [u32; 6],
    target: u32,
    read: u32,
    size: u32,
    level: u32,
    levels: u32,
    layer: u32,
    face: u32,
    mode: u32,
    sampler: u32,
    value: f32,
}

/// `Probe` in `reflections_common.slang` (144 bytes).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C)]
pub(crate) struct ProbeData {
    influence: [[f32; 4]; 3],
    extents: [f32; 4],
    parallax: [[f32; 4]; 3],
    parallax_extents: [f32; 4],
    position: [f32; 4],
}

/// `ReflectFrame` in `reflections.slang` (384 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct FrameData {
    clip_from_world: [f32; 16],
    previous_clip_from_world: [f32; 16],
    camera: [f32; 4],
    forward: [f32; 4],
    ray_base: [f32; 4],
    ray_x: [f32; 4],
    ray_y: [f32; 4],
    size: [u32; 4],
    env: [u32; 4],
    params: [f32; 4],
    probe_info: [u32; 4],
    probes: u64,
    _unused: [u64; 2],
    _pad: [u32; 2],
    ssr: [u32; 4],
    pyramid_offsets: [u32; 16],
    probe_cells: [u32; PROBE_CELL_COUNT],
    probe_bins: [u32; PROBE_BINS],
}

const _: () = assert!(size_of::<Push>() == 80);
const _: () = assert!(size_of::<ProbeData>() == 144);
const _: () = assert!(size_of::<FrameData>() == 384 + 4 * (PROBE_CELL_COUNT + PROBE_BINS));

/// What a view reflects: probes, screen-space reflections, both or neither (the default).
#[derive(Debug, Clone, Copy, Default)]
pub struct Reflections<'a> {
    /// Reflection probes of the scene (shared by views; one view per frame captures).
    pub probes: Option<&'a ReflectionProbes>,
    /// This view's screen-space reflections (one per view: they keep its history).
    pub ssr: Option<&'a ScreenSpaceReflections>,
}

impl Reflections<'_> {
    /// Neither probes nor screen-space reflections.
    pub const NONE: Reflections<'static> = Reflections {
        probes: None,
        ssr: None,
    };

    /// Whether the view asks for any reflections.
    pub fn any(&self) -> bool {
        self.probes.is_some() || self.ssr.is_some()
    }
}

/// An oriented box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeBox {
    /// World-space centre.
    pub center: Vec3,
    /// Half its size along its own axes.
    pub half_extents: Vec3,
    /// Its axes' rotation from the world's.
    pub rotation: Quat,
}

impl ProbeBox {
    /// An axis-aligned box.
    pub fn new(center: Vec3, half_extents: Vec3) -> Self {
        Self {
            center,
            half_extents,
            rotation: Quat::IDENTITY,
        }
    }

    /// The world-space bounding box: minimum and maximum corners.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        let m = Mat3::from_quat(self.rotation);
        let e = self.half_extents.abs();
        let extent = Vec3::new(
            m.row(0).abs().dot(e),
            m.row(1).abs().dot(e),
            m.row(2).abs().dot(e),
        );
        (self.center - extent, self.center + extent)
    }

    /// Rows of the world → box transform (rotation and translation), each row scaled by
    /// `scale` per axis.
    fn rows(&self, scale: Vec3) -> [[f32; 4]; 3] {
        let inverse = Mat3::from_quat(self.rotation.inverse());
        let t = -(inverse * self.center);
        core::array::from_fn(|k| {
            let row = inverse.row(k) * scale[k];
            [row.x, row.y, row.z, t[k] * scale[k]]
        })
    }

    /// Where the ray from `origin` along `direction` leaves the box, as the distance along
    /// it; `None` when `origin` is outside (the parallax correction of a probe).
    pub fn exit_distance(&self, origin: Vec3, direction: Vec3) -> Option<f32> {
        let inverse = self.rotation.inverse();
        let o = inverse * (origin - self.center);
        let d = inverse * direction;
        let e = self.half_extents;
        if o.abs().cmpgt(e).any() {
            return None;
        }
        let mut t = f32::MAX;
        for k in 0..3 {
            if d[k].abs() > 1e-8 {
                t = t.min(((e[k] - o[k]) / d[k]).max((-e[k] - o[k]) / d[k]));
            }
        }
        Some(t)
    }
}

/// When a probe is captured again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeRefresh {
    /// After edits of the probe, changes of the sky or the sun, and (on the persistent GPU
    /// scene path) when instances inside its boxes change.
    #[default]
    OnChange,
    /// Only after edits of the probe and [`ReflectionProbes::refresh`].
    Manual,
}

/// A reflection probe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReflectionProbe {
    /// Where its cube is captured from.
    pub position: Vec3,
    /// Where it reflects: pixels inside this box use it, fading out over
    /// `blend_distance` towards its faces.
    pub influence: ProbeBox,
    /// The proxy of its surroundings: reflected rays are intersected with this box and
    /// the cube read towards the intersection (usually the room's walls).
    pub parallax: ProbeBox,
    /// The distance (world units) over which the probe fades in from its influence box's
    /// faces.
    pub blend_distance: f32,
    /// Multiplier of its radiance.
    pub intensity: f32,
    /// How far its capture sees (the face cameras' far plane).
    pub far: f32,
    /// When it is captured again.
    pub refresh: ProbeRefresh,
}

impl ReflectionProbe {
    /// A probe for an axis-aligned room: captured from its centre, correcting parallax
    /// with the room's box, reflecting in that box grown by its blend distance (a tenth of
    /// its smallest half extent), so neighbouring rooms' probes blend across their shared
    /// walls instead of leaving a seam of sky.
    pub fn room(center: Vec3, half_extents: Vec3) -> Self {
        let b = ProbeBox::new(center, half_extents);
        let blend = 0.1 * half_extents.min_element().max(1e-3);
        Self {
            position: center,
            influence: ProbeBox::new(center, half_extents + blend),
            parallax: b,
            blend_distance: blend,
            intensity: 1.0,
            far: 4.0 * half_extents.length().max(1e-3),
            refresh: ProbeRefresh::OnChange,
        }
    }

    /// The world-space box whose changes it sees (both of its boxes).
    pub fn region(&self) -> (Vec3, Vec3) {
        let (a, b) = self.influence.bounds();
        let (c, d) = self.parallax.bounds();
        (a.min(c), b.max(d))
    }

    fn gpu(&self, layer: u32) -> ProbeData {
        let he = self.influence.half_extents.max(Vec3::splat(1e-6));
        let pe = self.parallax.half_extents.max(Vec3::splat(1e-6));
        ProbeData {
            influence: self.influence.rows(he.recip()),
            extents: he.extend(1.0 / self.blend_distance.max(1e-6)).to_array(),
            parallax: self.parallax.rows(Vec3::ONE),
            parallax_extents: pe.extend(layer as f32).to_array(),
            // The capture point in the parallax box's frame.
            position: (self.parallax.rotation.inverse() * (self.position - self.parallax.center))
                .extend(self.intensity.max(0.0))
                .to_array(),
        }
    }
}

/// Reflection probes' settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeSettings {
    /// Most probes (their cubes are allocated up front: about 1 MB each at 128²).
    pub capacity: u32,
    /// Face size of a probe's cube (its sharpest level), a power of two from 32.
    pub resolution: u32,
    /// Faces captured per frame at most (6: a whole probe per frame).
    pub faces_per_frame: u32,
    /// The face cameras' near plane.
    pub near: f32,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            capacity: 16,
            resolution: 128,
            faces_per_frame: 6,
            near: 0.05,
        }
    }
}

/// A probe in a [`ReflectionProbes`]: its slot and the slot's generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProbeId {
    slot: u32,
    generation: u32,
}

/// What the probes did in the last frame that drew them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeStats {
    /// Probes in the set.
    pub probes: u32,
    /// Probes with a capture (the others are not used yet).
    pub captured: u32,
    /// Probes waiting for a capture.
    pub pending: u32,
    /// Faces drawn in the last frame.
    pub faces: u32,
    /// Captures finished (prefiltered) in the last frame.
    pub completed: u32,
    /// Probes the last view blended.
    pub visible: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    probe: Option<ReflectionProbe>,
    generation: u32,
    /// Waiting for a capture.
    dirty: bool,
    /// Its cube holds a capture.
    captured: bool,
}

/// A capture in progress: the probe's slot and the next face.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Progress {
    slot: u32,
    next_face: u32,
}

#[derive(Debug, Default)]
struct State {
    slots: Vec<Slot>,
    capturing: Option<Progress>,
    /// The light the captures were made under.
    light: Option<[u32; 6]>,
    /// Visible probes this frame, by volume: (volume, slot).
    visible: Vec<(f32, u32)>,
    stats: ProbeStats,
}

/// The faces of a probe to draw this frame ([`ReflectionProbes::plan`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CapturePlan {
    pub slot: u32,
    pub first_face: u32,
    pub faces: u32,
    pub position: Vec3,
    pub near: f32,
    pub far: f32,
}

impl CapturePlan {
    /// The camera of face `face`.
    pub(crate) fn camera(&self, face: u32) -> Camera {
        face_camera(self.position, face, self.near, self.far)
    }

    /// Whether the plan draws the last face (then the cube is prefiltered).
    pub(crate) fn finishes(&self) -> bool {
        self.first_face + self.faces >= 6
    }
}

/// The camera of a probe's face `face` at `position`: 90°, square.
pub fn face_camera(position: Vec3, face: u32, near: f32, far: f32) -> Camera {
    let (forward, up) = FACES[face as usize % 6];
    let near = near.max(1e-4);
    Camera {
        clip_from_world: vulkan_perspective(
            core::f32::consts::FRAC_PI_2,
            1.0,
            near,
            far.max(near * 2.0),
        ) * look_at(position, position + forward, up),
        position,
    }
}

/// Reflection probes (see the module documentation).
pub struct ReflectionProbes {
    device: engine_rhi::ash::Device,
    face: ComputeShader<Push>,
    downsample: ComputeShader<Push>,
    prefilter: ComputeShader<Push>,
    /// Every probe's prefiltered cube.
    array: Image,
    /// The cube being captured, with its mips.
    source: Image,
    settings: ProbeSettings,
    state: RefCell<State>,
    /// The images' states before this frame's import, and whether the frame used them.
    unused_states: Cell<Option<(crate::graph::TextureState, crate::graph::TextureState)>>,
    used: Cell<[bool; 2]>,
}

impl core::fmt::Debug for ReflectionProbes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReflectionProbes")
            .field("settings", &self.settings)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl ReflectionProbes {
    /// Creates the cube array for `settings.capacity` probes and the capture's cube and
    /// pipelines.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        settings: ProbeSettings,
    ) -> Result<Self, RhiError> {
        let resolution = settings.resolution.clamp(32, 1024).next_power_of_two();
        let settings = ProbeSettings {
            capacity: settings.capacity.clamp(1, 256),
            resolution,
            faces_per_frame: settings.faces_per_frame.clamp(1, 6),
            near: settings.near.max(1e-4),
        };
        let [face, downsample, prefilter] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(PROBE_FACE, c"probeFace", &[])?,
                ComputeSource::new(PROBE_DOWNSAMPLE, c"probeDownsample", &[])?,
                ComputeSource::new(PROBE_PREFILTER, c"probePrefilter", &[])?,
            ],
        )?;
        let array = Image::new(
            device,
            allocator,
            bindless,
            PROBE_FORMAT,
            resolution,
            resolution,
            PROBE_LEVELS,
            ImageKind::CubeArray(settings.capacity),
        )?;
        let source = Image::new(
            device,
            allocator,
            bindless,
            PROBE_FORMAT,
            resolution,
            resolution,
            resolution.ilog2() + 1,
            ImageKind::Cube,
        );
        let mut array = array;
        let source = match source {
            Ok(s) => s,
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { array.destroy(device.raw(), allocator, bindless, 0) };
                return Err(e);
            }
        };
        let state = State {
            slots: vec![Slot::default(); settings.capacity as usize],
            visible: Vec::with_capacity(settings.capacity as usize),
            ..State::default()
        };
        Ok(Self {
            device: device.raw().clone(),
            face,
            downsample,
            prefilter,
            array,
            source,
            settings,
            state: RefCell::new(state),
            unused_states: Cell::new(None),
            used: Cell::new([false; 2]),
        })
    }

    /// The settings (resolution rounded to a power of two, limits applied).
    pub fn settings(&self) -> ProbeSettings {
        self.settings
    }

    /// Faces captured per frame at most (1-6).
    pub fn set_faces_per_frame(&mut self, faces: u32) {
        self.settings.faces_per_frame = faces.clamp(1, 6);
    }

    /// Adds a probe; it reflects once its first capture is done. `None` when the set is
    /// full ([`ProbeSettings::capacity`]).
    pub fn add(&mut self, probe: ReflectionProbe) -> Option<ProbeId> {
        let state = self.state.get_mut();
        let slot = state.slots.iter().position(|s| s.probe.is_none())?;
        let s = &mut state.slots[slot];
        s.probe = Some(probe);
        s.dirty = true;
        s.captured = false;
        Some(ProbeId {
            slot: slot as u32,
            generation: s.generation,
        })
    }

    fn slot_mut(&mut self, id: ProbeId) -> Option<&mut Slot> {
        self.state
            .get_mut()
            .slots
            .get_mut(id.slot as usize)
            .filter(|s| s.probe.is_some() && s.generation == id.generation)
    }

    /// Changes a probe; it is captured again (its old capture is used meanwhile).
    pub fn set(&mut self, id: ProbeId, probe: ReflectionProbe) {
        if let Some(slot) = self.slot_mut(id)
            && slot.probe != Some(probe)
        {
            slot.probe = Some(probe);
            slot.dirty = true;
        }
    }

    /// Removes a probe (stale ids are ignored).
    pub fn remove(&mut self, id: ProbeId) {
        if let Some(slot) = self.slot_mut(id) {
            slot.probe = None;
            slot.generation = slot.generation.wrapping_add(1);
            slot.dirty = false;
            slot.captured = false;
        }
    }

    /// The probe, if `id` is live.
    pub fn get(&self, id: ProbeId) -> Option<ReflectionProbe> {
        self.state
            .borrow()
            .slots
            .get(id.slot as usize)
            .filter(|s| s.generation == id.generation)
            .and_then(|s| s.probe)
    }

    /// Asks for a new capture of a probe.
    pub fn refresh(&self, id: ProbeId) {
        let mut state = self.state.borrow_mut();
        if let Some(slot) = state
            .slots
            .get_mut(id.slot as usize)
            .filter(|s| s.probe.is_some() && s.generation == id.generation)
        {
            slot.dirty = true;
        }
    }

    /// Asks for a new capture of every probe.
    pub fn refresh_all(&self) {
        for slot in &mut self.state.borrow_mut().slots {
            slot.dirty |= slot.probe.is_some();
        }
    }

    /// Something changed in the world-space box `min`..`max`: the probes that see it
    /// ([`ReflectionProbe::region`], [`ProbeRefresh::OnChange`]) are captured again. The
    /// persistent GPU scene path calls it with the scene's changes; call it yourself for
    /// changes the renderer does not see (a draw list's moved objects, local lights).
    pub fn invalidate_region(&self, min: Vec3, max: Vec3) {
        let mut state = self.state.borrow_mut();
        invalidate(&mut state.slots, &[[min, max]]);
    }

    pub(crate) fn note_changes(&self, boxes: &[[Vec3; 2]]) {
        if !boxes.is_empty() {
            invalidate(&mut self.state.borrow_mut().slots, boxes);
        }
    }

    /// What the probes did in the last frame.
    pub fn stats(&self) -> ProbeStats {
        self.state.borrow().stats
    }

    /// Starts a frame's bookkeeping: a changed light or sky (`rebuilt`) asks for new
    /// captures; then picks the faces to draw (the capture in progress, else the dirty
    /// probe nearest to `camera`).
    pub(crate) fn plan(
        &self,
        light: &DirectionalLight,
        sun: Vec3,
        rebuilt: bool,
        camera: Vec3,
    ) -> Option<CapturePlan> {
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let key = {
            let d = light.direction;
            [
                d.x.to_bits(),
                d.y.to_bits(),
                d.z.to_bits(),
                sun.x.to_bits(),
                sun.y.to_bits(),
                sun.z.to_bits(),
            ]
        };
        let light_changed = state.light.is_some_and(|l| l != key);
        state.light = Some(key);
        if light_changed || rebuilt {
            for slot in &mut state.slots {
                if slot
                    .probe
                    .is_some_and(|p| p.refresh == ProbeRefresh::OnChange)
                {
                    slot.dirty = true;
                }
            }
        }
        state.stats.faces = 0;
        state.stats.completed = 0;
        // A capture whose probe went away stops.
        if let Some(p) = state.capturing
            && state.slots[p.slot as usize].probe.is_none()
        {
            state.capturing = None;
        }
        if state.capturing.is_none() {
            let nearest = state
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.dirty)
                .filter_map(|(k, s)| s.probe.map(|p| (k, p.position.distance_squared(camera))))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((k, _)) = nearest {
                state.slots[k].dirty = false;
                state.capturing = Some(Progress {
                    slot: k as u32,
                    next_face: 0,
                });
            }
        }
        let progress = state.capturing?;
        let probe = state.slots[progress.slot as usize].probe?;
        Some(CapturePlan {
            slot: progress.slot,
            first_face: progress.next_face,
            faces: self.settings.faces_per_frame.min(6 - progress.next_face),
            position: probe.position,
            near: self.settings.near,
            far: probe.far,
        })
    }

    /// Records that `plan`'s faces were drawn (and the probe prefiltered when it was the
    /// last).
    pub(crate) fn commit(&self, plan: &CapturePlan) {
        let mut state = self.state.borrow_mut();
        state.stats.faces += plan.faces;
        let next_face = plan.first_face + plan.faces;
        if next_face >= 6 {
            state.capturing = None;
            state.slots[plan.slot as usize].captured = true;
            state.stats.completed += 1;
        } else {
            state.capturing = Some(Progress {
                slot: plan.slot,
                next_face,
            });
        }
    }

    /// Imports the probes' images into `fg` (once per frame): the cube array, then the
    /// capture cube.
    pub(crate) fn import<'g>(&self, fg: &mut FrameGraph<'g>) -> ProbeTextures {
        // An import the frame does not use leaves the image as it was (`finish`).
        self.unused_states
            .set(Some((self.array.state(), self.source.state())));
        self.used.set([false; 2]);
        ProbeTextures {
            array: self.array.import(fg, "reflection probes"),
            source: self.source.import(fg, "reflection probe capture"),
        }
    }

    /// Records that this frame's passes read or write the cube array (`array`) or the
    /// capture cube.
    pub(crate) fn mark_used(&self, array: bool, source: bool) {
        let [a, s] = self.used.get();
        self.used.set([a || array, s || source]);
    }

    /// Ends the frame's import: an image no pass used stays in its state before it.
    pub(crate) fn finish(&self) {
        if let Some((array, source)) = self.unused_states.take() {
            let [a, s] = self.used.get();
            if !a {
                self.array.set_state(array);
            }
            if !s {
                self.source.set_state(source);
            }
        }
    }

    /// Copies face `face`, drawn into `color` and `depth`, into the capture cube; where
    /// nothing was drawn, the sky's radiance (from `environment`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_face_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        textures: ProbeTextures,
        environment: &EnvironmentFrame,
        color: Texture,
        depth: Texture,
        face: u32,
    ) {
        self.mark_used(false, true);
        let size = self.settings.resolution;
        let target = self.source.storage[0];
        let (sky, intensity) = (environment.source_index, environment.intensity);
        fg.add_compute("probe face", bindless)
            .read(color, Access::Sampled(ShaderStages::COMPUTE))
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(environment.source, Access::Sampled(ShaderStages::COMPUTE))
            .write(
                textures.source,
                Access::StorageReadWrite(ShaderStages::COMPUTE),
            )
            .dispatch(&self.face, Dispatch::Threads([size, size, 1]), move |ctx| {
                Some(Push {
                    t: [
                        ctx.sampled_index(color)?,
                        ctx.sampled_index(depth)?,
                        sky,
                        0,
                        0,
                        0,
                    ],
                    target,
                    size,
                    face,
                    sampler: Bindless::sampler(SamplerKind::LinearClamp),
                    value: intensity,
                    ..Push::default()
                })
            });
    }

    /// Fills the capture cube's mips and prefilters it into probe `slot`'s cube.
    pub(crate) fn add_prefilter_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        textures: ProbeTextures,
        slot: u32,
    ) {
        self.mark_used(true, true);
        let size = self.settings.resolution;
        let levels = size.ilog2() + 1;
        let storage: &'g [u32] = &self.source.storage;
        let downsample = &self.downsample;
        fg.add_compute("probe mips", bindless)
            .async_compute()
            .write(
                textures.source,
                Access::StorageReadWrite(ShaderStages::COMPUTE),
            )
            .record(move |rec| {
                for level in 1..levels {
                    rec.barrier();
                    let push = Push {
                        target: storage[level as usize],
                        read: storage[level as usize - 1],
                        size: size >> level,
                        ..Push::default()
                    };
                    let s = size >> level;
                    rec.dispatch(downsample, Dispatch::Threads([s, s, 6]), &push);
                }
            });
        let targets: &'g [u32] = &self.array.storage;
        let source = self.source.sampled;
        let prefilter = &self.prefilter;
        fg.add_compute("probe prefilter", bindless)
            .async_compute()
            .read(textures.source, Access::Sampled(ShaderStages::COMPUTE))
            .write(
                textures.array,
                Access::StorageReadWrite(ShaderStages::COMPUTE),
            )
            .record(move |rec| {
                for level in 0..PROBE_LEVELS {
                    let s = size >> level;
                    let push = Push {
                        t: [source, 0, 0, 0, 0, 0],
                        target: targets[level as usize],
                        read: size,
                        size: s,
                        level,
                        levels: PROBE_LEVELS,
                        layer: slot,
                        mode: PREFILTER_SAMPLES,
                        sampler: Bindless::sampler(SamplerKind::LinearClamp),
                        ..Push::default()
                    };
                    rec.dispatch(prefilter, Dispatch::Threads([s, s, 6]), &push);
                }
            });
    }

    /// The captured probes a view with `clip_from_world` can see, smallest first (at most
    /// [`MAX_VISIBLE_PROBES`]), written into `ring` with the screen grid's cells listing
    /// them. `None` when `ring` is full.
    pub(crate) fn view_frame(
        &self,
        ring: &mut UploadRing,
        clip_from_world: Mat4,
        camera: Vec3,
    ) -> Option<VisibleProbes> {
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let frustum = Frustum::from_clip_from_world(clip_from_world);
        state.visible.clear();
        let (mut probes, mut pending, mut captured) = (0, 0, 0);
        for (k, slot) in state.slots.iter().enumerate() {
            let Some(probe) = slot.probe else { continue };
            probes += 1;
            pending += u32::from(slot.dirty || !slot.captured);
            if !slot.captured {
                continue;
            }
            captured += 1;
            let (min, max) = probe.influence.bounds();
            let (center, extents) = ((min + max) * 0.5, (max - min) * 0.5);
            let inside = frustum
                .planes
                .iter()
                .all(|p| p.truncate().dot(center) + p.truncate().abs().dot(extents) + p.w >= 0.0);
            if inside {
                let e = probe.influence.half_extents;
                state.visible.push((e.x * e.y * e.z, k as u32));
            }
        }
        state
            .visible
            .sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        state.visible.truncate(MAX_VISIBLE_PROBES);
        let count = state.visible.len();
        state.stats.probes = probes;
        state.stats.pending = pending;
        state.stats.captured = captured;
        state.stats.visible = count as u32;
        if count == 0 {
            return Some(VisibleProbes {
                count: 0,
                probes: 0,
                cells: [0; PROBE_CELL_COUNT],
                bins: [0; PROBE_BINS],
                bin_scale: 0.0,
            });
        }
        let (out, data) = ring.alloc::<ProbeData>(count)?;
        for (out, &(_, slot)) in out.iter_mut().zip(&state.visible) {
            let probe = state.slots[slot as usize]
                .probe
                .unwrap_or_else(|| ReflectionProbe::room(Vec3::ZERO, Vec3::ONE));
            out.write(probe.gpu(slot));
        }
        // Each probe's bit in the cells its screen rectangle covers.
        let mut cells = [0u32; PROBE_CELL_COUNT];
        for (k, &(_, slot)) in state.visible.iter().enumerate() {
            let Some(probe) = state.slots[slot as usize].probe else {
                continue;
            };
            let [x0, y0, x1, y1] = screen_cells(&probe.influence, clip_from_world);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    cells[(y * PROBE_CELLS[0] + x) as usize] |= 1 << k;
                }
            }
        }
        // Each probe's bit in the z-bins its influence box's view-depth range covers.
        let view = Camera {
            clip_from_world,
            position: camera,
        }
        .view_space();
        let mut ranges = [(0.0f32, f32::MAX); MAX_VISIBLE_PROBES];
        let mut far = 0.0f32;
        if let Some(view) = view {
            // View depth: minus the view-space z.
            let row = -view.view_from_world.row(2);
            let axis = row.truncate();
            for (range, &(_, slot)) in ranges.iter_mut().zip(&state.visible) {
                let Some(probe) = state.slots[slot as usize].probe else {
                    continue;
                };
                let b = &probe.influence;
                let local_axis = Mat3::from_quat(b.rotation).transpose() * axis;
                let center = axis.dot(b.center) + row.w;
                let extent = local_axis.abs().dot(b.half_extents.abs());
                *range = ((center - extent).max(0.0), center + extent);
                far = far.max(center + extent);
            }
        }
        let bin_scale = if far > 0.0 {
            PROBE_BINS as f32 / far
        } else {
            0.0
        };
        let mut bins = [0u32; PROBE_BINS];
        for (k, &(lo, hi)) in ranges.iter().take(count).enumerate() {
            let first = ((lo * bin_scale) as usize).min(PROBE_BINS - 1);
            let last = if bin_scale > 0.0 {
                ((hi * bin_scale) as usize).min(PROBE_BINS - 1)
            } else {
                PROBE_BINS - 1
            };
            for bin in &mut bins[first..=last] {
                *bin |= 1 << k;
            }
        }
        Some(VisibleProbes {
            count: count as u32,
            probes: data.address,
            cells,
            bins,
            bin_scale,
        })
    }

    /// The probes' cube array's bindless index.
    pub(crate) fn array_index(&self) -> u32 {
        self.array.sampled
    }

    /// Frees the images.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.array.destroy(&self.device, allocator, bindless, 0);
            self.source.destroy(&self.device, allocator, bindless, 0);
        }
    }
}

/// Marks the probes that see any of `boxes` (min, max) for a new capture.
fn invalidate(slots: &mut [Slot], boxes: &[[Vec3; 2]]) {
    for slot in slots.iter_mut() {
        let Some(probe) = slot.probe else { continue };
        if probe.refresh != ProbeRefresh::OnChange || slot.dirty {
            continue;
        }
        let (lo, hi) = probe.region();
        if boxes
            .iter()
            .any(|b| b[0].cmple(hi).all() && b[1].cmpge(lo).all())
        {
            slot.dirty = true;
        }
    }
}

/// The cells of the probes' screen grid (inclusive: min x, min y, max x, max y) a box
/// covers; every cell when it reaches behind the camera.
fn screen_cells(b: &ProbeBox, clip_from_world: Mat4) -> [u32; 4] {
    let last = [PROBE_CELLS[0] - 1, PROBE_CELLS[1] - 1];
    let whole = [0, 0, last[0], last[1]];
    let m = Mat3::from_quat(b.rotation);
    let (mut lo, mut hi) = (
        Vec3::splat(f32::MAX).truncate(),
        Vec3::splat(f32::MIN).truncate(),
    );
    for corner in 0..8 {
        let sign = Vec3::new(
            if corner & 1 != 0 { 1.0 } else { -1.0 },
            if corner & 2 != 0 { 1.0 } else { -1.0 },
            if corner & 4 != 0 { 1.0 } else { -1.0 },
        );
        let p = b.center + m * (b.half_extents * sign);
        let clip = clip_from_world * p.extend(1.0);
        if clip.w <= 1e-5 {
            return whole;
        }
        let ndc = clip.truncate().truncate() / clip.w;
        lo = lo.min(ndc);
        hi = hi.max(ndc);
    }
    let to_cell = |ndc: f32, cells: u32| -> u32 {
        (((ndc * 0.5 + 0.5) * cells as f32).max(0.0) as u32).min(cells - 1)
    };
    [
        to_cell(lo.x, PROBE_CELLS[0]),
        to_cell(lo.y, PROBE_CELLS[1]),
        to_cell(hi.x, PROBE_CELLS[0]),
        to_cell(hi.y, PROBE_CELLS[1]),
    ]
}

/// The probes' images in one frame's graph.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProbeTextures {
    pub array: Texture,
    pub source: Texture,
}

/// A view's visible probes in the ring.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VisibleProbes {
    count: u32,
    probes: u64,
    cells: [u32; PROBE_CELL_COUNT],
    bins: [u32; PROBE_BINS],
    /// Z-bins per unit of view depth.
    bin_scale: f32,
}

/// Screen-space reflections' settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SsrSettings {
    /// Trace one ray per 2 × 2 pixels (one of them, turning every frame) instead of one
    /// per pixel: about a quarter of the cost; the filters fill in.
    pub half_resolution: bool,
    /// Surfaces at least this rough (perceptual) trace nothing (probes and the sky
    /// reflect there); reflections fade out towards it.
    pub max_roughness: f32,
    /// Steps of the hierarchical traversal at most.
    pub steps: u32,
    /// How far (world units) behind the depth buffer's surface a hit still counts: the
    /// assumed thickness of objects.
    pub thickness: f32,
    /// The share of the current frame in the temporal filter for glossy surfaces (mirrors
    /// take more): lower is smoother, with more lag.
    pub temporal: f32,
}

impl Default for SsrSettings {
    fn default() -> Self {
        Self {
            half_resolution: true,
            max_roughness: 0.6,
            steps: 64,
            thickness: 0.3,
            temporal: 0.15,
        }
    }
}

/// A view's screen-space reflections (see the module documentation): settings and the
/// temporal filter's history, for targets of one size.
pub struct ScreenSpaceReflections {
    device: engine_rhi::ash::Device,
    history: [Image; 2],
    size: [u32; 2],
    settings: Cell<SsrSettings>,
    /// The history written last frame.
    current: Cell<usize>,
    valid: Cell<bool>,
    frame: Cell<u32>,
    previous_clip_from_world: Cell<Option<Mat4>>,
}

impl core::fmt::Debug for ScreenSpaceReflections {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ScreenSpaceReflections")
            .field("size", &self.size)
            .field("settings", &self.settings.get())
            .finish_non_exhaustive()
    }
}

impl ScreenSpaceReflections {
    /// Screen-space reflections for a view drawing `width` × `height` targets (two
    /// history images of that size). A view of another size draws without them: make a
    /// new one when it is resized.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        width: u32,
        height: u32,
        settings: SsrSettings,
    ) -> Result<Self, RhiError> {
        let (width, height) = (width.max(1), height.max(1));
        let image = |allocator: &mut Allocator| {
            Image::new(
                device,
                allocator,
                bindless,
                PROBE_FORMAT,
                width,
                height,
                1,
                ImageKind::Flat,
            )
        };
        let mut first = image(allocator)?;
        let second = match image(allocator) {
            Ok(s) => s,
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { first.destroy(device.raw(), allocator, bindless, 0) };
                return Err(e);
            }
        };
        Ok(Self {
            device: device.raw().clone(),
            history: [first, second],
            size: [width, height],
            settings: Cell::new(settings),
            current: Cell::new(0),
            valid: Cell::new(false),
            frame: Cell::new(0),
            previous_clip_from_world: Cell::new(None),
        })
    }

    /// The settings.
    pub fn settings(&self) -> SsrSettings {
        self.settings.get()
    }

    /// Changes the settings.
    pub fn set_settings(&self, settings: SsrSettings) {
        self.settings.set(settings);
    }

    /// The target size it draws.
    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    /// Forgets the history (after a camera cut).
    pub fn reset(&self) {
        self.valid.set(false);
    }

    /// Frees the history.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        for image in &mut self.history {
            // SAFETY: guaranteed by the caller.
            unsafe { image.destroy(&self.device, allocator, bindless, 0) };
        }
    }
}

/// The forward renderer's reflection passes (made by
/// [`crate::forward::ForwardRenderer::set_reflections`]).
pub(crate) struct ReflectionPasses {
    /// The composite with probes, with screen-space reflections, with both.
    composite: [Pipeline; 3],
    depth: ComputeShader<Push>,
    trace: ComputeShader<Push>,
    spatial: ComputeShader<Push>,
    temporal: ComputeShader<Push>,
}

impl core::fmt::Debug for ReflectionPasses {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReflectionPasses").finish_non_exhaustive()
    }
}

/// What a view's reflections depend on, before its passes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewSetup<'g> {
    pub environment: EnvironmentFrame,
    /// As the depth is drawn (with any jitter).
    pub clip_from_world: Mat4,
    pub camera: Vec3,
    pub size: [u32; 2],
    pub probes: Option<&'g ReflectionProbes>,
    pub ssr: Option<&'g ScreenSpaceReflections>,
}

/// A view's reflection constants in the ring ([`ReflectionPasses::prepare`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prepared<'g> {
    /// `ReflectFrame` in the ring (the forward shading reads it through its frame).
    pub address: u64,
    /// Visible probes (0: the shading needs no probe code).
    pub probes: u32,
    /// The view's screen-space reflections, when they run (the target's size matches).
    pub ssr: Option<&'g ScreenSpaceReflections>,
    size: [u32; 2],
    trace_size: [u32; 2],
    levels: u32,
    pyramid_floats: u32,
    clip_from_world: Mat4,
}

/// A view's reflections this frame ([`crate::forward::ForwardPasses::reflections`]), for
/// effects of your own after the scene passes (the frame renderer's hook, debug views).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReflectionOutput {
    /// The opaque surfaces' octahedral normal and perceptual roughness
    /// ([`NORMAL_ROUGHNESS_FORMAT`]; zero where nothing reflects: the sky, subsurface
    /// materials).
    pub normal_roughness: Texture,
    /// The weight of their prefiltered radiance, split sum × specular occlusion
    /// ([`WEIGHT_FORMAT`]).
    pub weight: Texture,
    /// The screen-space reflections, premultiplied by their confidence (alpha), at the
    /// render size, when they ran.
    pub ssr: Option<Texture>,
}

/// What a view's reflection passes read and write.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewInputs {
    /// The composite's target (multisampled with MSAA).
    pub color: Texture,
    /// The lit image screen-space reflections trace (`color`, or its resolve with MSAA).
    pub lit: Texture,
    pub depth: Texture,
    pub normal_roughness: Texture,
    pub weight: Texture,
    pub environment: EnvironmentFrame,
    pub probes: Option<ProbeTextures>,
}

/// Levels of a nearest-depth pyramid of a `width × height` target (level `k` is
/// `⌈size / 2^k⌉`, down to 1 × 1) and each stored level's offset (levels 1.. in floats).
fn pyramid_layout(width: u32, height: u32) -> (u32, [u32; 16], u32) {
    let mut offsets = [0u32; 16];
    let mut floats = 0;
    let mut levels = 0;
    for level in 1..=MAX_PYRAMID_LEVELS {
        let (w, h) = (((width - 1) >> level) + 1, ((height - 1) >> level) + 1);
        offsets[level as usize - 1] = floats;
        floats += w * h;
        levels = level;
        if w == 1 && h == 1 {
            break;
        }
    }
    (levels, offsets, floats)
}

/// The composite's pipelines (probes, screen-space reflections, both; then probes on the
/// samples behind the nearest surface) into a multisampled `color_format` target of
/// `samples` (MSAA, ADR 0075): run per pixel from the nearest (or farthest) sample's
/// inputs, adding to that surface's samples only (`SV_Coverage`).
pub(crate) fn composite_pipelines(
    device: &Device,
    bindless: &Bindless,
    color_format: vk::Format,
    samples: vk::SampleCountFlags,
) -> Result<[Pipeline; 4], RhiError> {
    let invalid = || RhiError::Vulkan {
        call: "embedded SPIR-V",
        result: vk::Result::ERROR_INVALID_SHADER_NV,
    };
    let vertex = spirv_words(COMPOSITE_VERTEX).ok_or_else(invalid)?;
    let fragments = COMPOSITE_MS
        .iter()
        .map(|&(bytes, entry)| Ok((spirv_words(bytes).ok_or_else(invalid)?, entry)))
        .collect::<Result<Vec<_>, RhiError>>()?;
    let formats = [color_format];
    let descs: Vec<PipelineDesc<'_>> = fragments
        .iter()
        .map(|(fragment, entry)| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats: &formats,
                cull_mode: vk::CullModeFlags::NONE,
                depth_test: false,
                depth_write: false,
                blend: Blend::Additive,
                samples,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"compositeVertex",
                    },
                    Some(ShaderStageDesc {
                        spirv: fragment,
                        entry,
                    }),
                )
            })
        })
        .collect();
    let pipelines = Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?;
    pipelines.try_into().map_err(|_| RhiError::Vulkan {
        call: "reflection composite pipelines (fewer than described)",
        result: vk::Result::ERROR_UNKNOWN,
    })
}

impl ReflectionPasses {
    /// Creates the composite pipelines (into `color_format`) and the compute passes.
    pub(crate) fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(COMPOSITE_VERTEX).ok_or_else(invalid)?;
        let fragments = [
            (
                spirv_words(COMPOSITE_PROBES).ok_or_else(invalid)?,
                c"compositeProbes",
            ),
            (
                spirv_words(COMPOSITE_SSR).ok_or_else(invalid)?,
                c"compositeSsr",
            ),
            (
                spirv_words(COMPOSITE_PROBES_SSR).ok_or_else(invalid)?,
                c"compositeProbesSsr",
            ),
        ];
        let formats = [color_format];
        let sources = [
            ComputeSource::<Push>::new(SSR_DEPTH, c"ssrDepth", &[])?,
            ComputeSource::<Push>::new(SSR_TRACE, c"ssrTrace", &[])?,
            ComputeSource::<Push>::new(SSR_SPATIAL, c"ssrSpatial", &[])?,
            ComputeSource::<Push>::new(SSR_TEMPORAL, c"ssrTemporal", &[])?,
        ];
        let mut descs: Vec<PipelineDesc<'_>> = fragments
            .iter()
            .map(|(fragment, entry)| {
                PipelineDesc::Graphics(GraphicsPipelineDesc {
                    color_formats: &formats,
                    cull_mode: vk::CullModeFlags::NONE,
                    depth_test: false,
                    depth_write: false,
                    blend: Blend::Additive,
                    ..GraphicsPipelineDesc::new(
                        ShaderStageDesc {
                            spirv: &vertex,
                            entry: c"compositeVertex",
                        },
                        Some(ShaderStageDesc {
                            spirv: fragment,
                            entry,
                        }),
                    )
                })
            })
            .collect();
        descs.extend(sources.iter().map(ComputeSource::desc));
        let mut pipelines =
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "reflection pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let composite = [next()?, next()?, next()?];
        let [depth, trace, spatial, temporal] = &sources;
        Ok(Self {
            composite,
            depth: depth.finish(next()?),
            trace: trace.finish(next()?),
            spatial: spatial.finish(next()?),
            temporal: temporal.finish(next()?),
        })
    }

    /// The driver's statistics of the composite pipelines: with probes, with screen-space
    /// reflections, with both.
    pub(crate) fn composite_statistics(
        &self,
        device: &Device,
    ) -> [Vec<engine_rhi::PipelineStatistic>; 3] {
        core::array::from_fn(|k| self.composite[k].statistics(device))
    }

    /// Writes a view's reflection constants and visible probes into `ring`, before its
    /// shading reads them. `None` when `ring` is full or the view is not a perspective one.
    pub(crate) fn prepare<'g>(
        &self,
        ring: &mut UploadRing,
        setup: &ViewSetup<'g>,
    ) -> Option<Prepared<'g>> {
        engine_profiler::zone!("render::reflections::prepare");
        let camera = Camera {
            clip_from_world: setup.clip_from_world,
            position: setup.camera,
        };
        let view = camera.view_space()?;
        let world_from_view = view.view_from_world.inverse();
        let to_world = |v: Vec3| world_from_view.transform_vector3(v);
        let [width, height] = setup.size;
        let visible = match setup.probes {
            Some(probes) => Some(probes.view_frame(ring, setup.clip_from_world, setup.camera)?),
            None => None,
        };
        let ssr = setup.ssr.filter(|s| s.size == setup.size);
        let settings = ssr
            .map(ScreenSpaceReflections::settings)
            .unwrap_or_default();
        let scale = if settings.half_resolution { 2 } else { 1 };
        let trace_size = [width.div_ceil(scale), height.div_ceil(scale)];
        let (levels, offsets, floats) = pyramid_layout(width.max(1), height.max(1));
        let frame_index = ssr.map_or(0, |s| s.frame.get());
        let previous = ssr
            .and_then(|s| s.previous_clip_from_world.get())
            .unwrap_or(setup.clip_from_world);
        let history_valid = ssr.is_some_and(|s| s.valid.get());
        let frame = FrameData {
            clip_from_world: setup.clip_from_world.to_cols_array(),
            previous_clip_from_world: previous.to_cols_array(),
            camera: setup.camera.extend(1.0).to_array(),
            forward: to_world(Vec3::NEG_Z)
                .normalize()
                .extend(view.p32 / view.p22)
                .to_array(),
            ray_base: to_world(Vec3::new(view.p20 / view.p00, view.p21 / view.p11, -1.0))
                .extend(view.p22)
                .to_array(),
            ray_x: to_world(Vec3::new(1.0 / view.p00, 0.0, 0.0))
                .extend(view.p32)
                .to_array(),
            ray_y: to_world(Vec3::new(0.0, 1.0 / view.p11, 0.0))
                .extend(1.0)
                .to_array(),
            size: [width, height, trace_size[0], trace_size[1]],
            env: [
                setup.environment.prefiltered_index,
                PREFILTERED_LEVELS,
                Bindless::sampler(SamplerKind::LinearClamp),
                setup.probes.map_or(0, ReflectionProbes::array_index),
            ],
            params: [
                setup.environment.intensity,
                settings.max_roughness.clamp(0.0, 1.0),
                settings.thickness.max(1e-4),
                settings.temporal.clamp(0.01, 1.0),
            ],
            probe_info: [
                visible.map_or(0, |v| v.count),
                visible.map_or(0.0, |v| v.bin_scale).to_bits(),
                0,
                frame_index,
            ],
            probes: visible.map_or(0, |v| v.probes),
            _unused: [0; 2],
            _pad: [0; 2],
            ssr: [
                settings.steps.clamp(1, 512),
                scale,
                u32::from(history_valid),
                levels,
            ],
            pyramid_offsets: offsets,
            probe_cells: visible.map_or([0; PROBE_CELL_COUNT], |v| v.cells),
            probe_bins: visible.map_or([0; PROBE_BINS], |v| v.bins),
        };
        Some(Prepared {
            address: ring.push(&[frame])?,
            probes: visible.map_or(0, |v| v.count),
            ssr,
            size: setup.size,
            trace_size,
            levels,
            pyramid_floats: floats,
            clip_from_world: setup.clip_from_world,
        })
    }

    /// Declares a view's reflection passes after its opaque passes: screen-space
    /// reflections (the nearest-depth pyramid, the trace, the spatial and temporal
    /// filters) when they run, and the composite into `inputs.color`.
    /// `msaa`: for a multisampled target (MSAA, ADR 0075), the composite's pipelines
    /// ([`composite_pipelines`]), the multisampled depth and the sample count; `None` for
    /// this one's.
    pub(crate) fn add_view_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        prepared: &Prepared<'g>,
        inputs: &ViewInputs,
        msaa: Option<MsComposite<'g>>,
    ) -> Option<Texture> {
        engine_profiler::zone!("render::reflections::add_view_passes");
        let ssr = prepared
            .ssr
            .map(|state| self.add_ssr_passes(fg, bindless, prepared, inputs, state));
        let probes = inputs.probes.filter(|_| prepared.probes > 0);
        if probes.is_none() && ssr.is_none() {
            return None;
        }
        let frame_address = prepared.address;
        let (depth, nr, weight) = (inputs.depth, inputs.normal_roughness, inputs.weight);
        // The composite: weight × (reflection − sky) added to the color.
        let mut pass = fg
            .add_pass("reflections")
            .color(inputs.color, LoadOp::Load, StoreOp::Store)
            .read(depth, Access::Sampled(ShaderStages::FRAGMENT))
            .read(nr, Access::Sampled(ShaderStages::FRAGMENT))
            .read(weight, Access::Sampled(ShaderStages::FRAGMENT))
            .read(
                inputs.environment.prefiltered,
                Access::Sampled(ShaderStages::FRAGMENT),
            );
        if let Some(result) = ssr {
            pass = pass.read(result, Access::Sampled(ShaderStages::FRAGMENT));
        }
        if let Some(textures) = probes {
            pass = pass.read(textures.array, Access::Sampled(ShaderStages::FRAGMENT));
        }
        let composite = msaa.map_or(&self.composite[..], |m| &m.pipelines[..3]);
        let samples = msaa.map(|m| (m.depth, m.samples));
        if let Some(m) = msaa {
            pass = pass.read(m.depth, Access::Sampled(ShaderStages::FRAGMENT));
            for t in m.far {
                pass = pass.read(t, Access::Sampled(ShaderStages::FRAGMENT));
            }
        }
        // The samples behind the nearest surface, with probes.
        let far = msaa
            .filter(|_| probes.is_some())
            .map(|m| (&m.pipelines[3], m.far));
        let pipeline = match (probes.is_some(), ssr.is_some()) {
            (true, false) => &composite[0],
            (false, true) => &composite[1],
            _ => &composite[2],
        };
        pass.execute(move |ctx| {
            let (Some(t0), Some(t1), Some(t2)) = (
                ctx.sampled_index(depth),
                ctx.sampled_index(nr),
                ctx.sampled_index(weight),
            ) else {
                return;
            };
            let t3 = ssr.and_then(|r| ctx.sampled_index(r)).unwrap_or(0);
            let (t4, t5) = samples.map_or((0, 0), |(depth, n)| {
                (ctx.sampled_index(depth).unwrap_or(0), n)
            });
            let push = Push {
                frame: frame_address,
                t: [t0, t1, t2, t3, t4, t5],
                ..Push::default()
            };
            let area = ctx.render_area();
            // SAFETY: recording inside the pass's rendering scope; the constants are in
            // the ring, the textures are read by the pass.
            unsafe {
                pipeline.bind(ctx.cmd);
                bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                ctx.device.cmd_set_viewport(
                    ctx.cmd,
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
                ctx.device.cmd_set_scissor(ctx.cmd, 0, &[area]);
                bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
            }
            if let Some((pipeline, [far_depth, far_nr, far_weight])) = far {
                let (Some(f0), Some(f1), Some(f2)) = (
                    ctx.sampled_index(far_depth),
                    ctx.sampled_index(far_nr),
                    ctx.sampled_index(far_weight),
                ) else {
                    return;
                };
                let push = Push {
                    t: [f0, f1, f2, 0, t4, t5],
                    read: t0,
                    ..push
                };
                // SAFETY: as above.
                unsafe {
                    pipeline.bind(ctx.cmd);
                    bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                }
            }
        });
        ssr
    }

    /// Screen-space reflections' passes: the nearest-depth pyramid, the trace, the spatial
    /// and temporal filters. Returns this frame's result (its history).
    #[allow(clippy::too_many_lines)]
    fn add_ssr_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        prepared: &Prepared<'g>,
        inputs: &ViewInputs,
        state: &'g ScreenSpaceReflections,
    ) -> Texture {
        let frame_address = prepared.address;
        let [width, height] = prepared.size;
        let trace_size = prepared.trace_size;
        let levels = prepared.levels;
        let pyramid = fg.create_buffer(
            "ssr depth pyramid",
            BufferDesc {
                size: u64::from(prepared.pyramid_floats.max(1)) * 4,
            },
        );
        let depth = inputs.depth;
        let shader = &self.depth;
        fg.add_compute("ssr depth", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .write(pyramid, Access::StorageReadWrite(ShaderStages::COMPUTE))
            .record(move |rec| {
                let ctx = rec.context();
                let (Some(t0), p) = (ctx.sampled_index(depth), ctx.address(pyramid)) else {
                    return;
                };
                for level in 1..=levels {
                    if level > 1 {
                        rec.barrier();
                    }
                    let w = ((width - 1) >> level) + 1;
                    let h = ((height - 1) >> level) + 1;
                    let push = Push {
                        frame: frame_address,
                        buffer: p,
                        t: [t0, 0, 0, 0, 0, 0],
                        level,
                        ..Push::default()
                    };
                    rec.dispatch(shader, Dispatch::Threads([w, h, 1]), &push);
                }
            });
        let trace = fg.create_texture(
            "ssr trace",
            TextureDesc::new_2d(PROBE_FORMAT, trace_size[0], trace_size[1]),
        );
        let trace_distance = fg.create_texture(
            "ssr trace distance",
            TextureDesc::new_2d(SSR_DISTANCE_FORMAT, trace_size[0], trace_size[1]),
        );
        let (nr, weight, color) = (inputs.normal_roughness, inputs.weight, inputs.lit);
        fg.add_compute("ssr trace", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(nr, Access::Sampled(ShaderStages::COMPUTE))
            .read(weight, Access::Sampled(ShaderStages::COMPUTE))
            .read(color, Access::Sampled(ShaderStages::COMPUTE))
            .read(pyramid, Access::StorageRead(ShaderStages::COMPUTE))
            .write(trace, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(trace_distance, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.trace,
                Dispatch::Threads([trace_size[0], trace_size[1], 1]),
                move |ctx| {
                    Some(Push {
                        frame: frame_address,
                        buffer: ctx.address(pyramid),
                        t: [
                            ctx.sampled_index(depth)?,
                            ctx.sampled_index(nr)?,
                            ctx.sampled_index(weight)?,
                            ctx.sampled_index(color)?,
                            0,
                            0,
                        ],
                        target: ctx.storage_index(trace)?,
                        read: ctx.storage_index(trace_distance)?,
                        ..Push::default()
                    })
                },
            );
        let spatial = fg.create_texture(
            "ssr spatial",
            TextureDesc::new_2d(PROBE_FORMAT, trace_size[0], trace_size[1]),
        );
        let distance = fg.create_texture(
            "ssr distance",
            TextureDesc::new_2d(SSR_DISTANCE_DEPTH_FORMAT, trace_size[0], trace_size[1]),
        );
        fg.add_compute("ssr spatial", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(nr, Access::Sampled(ShaderStages::COMPUTE))
            .read(trace, Access::Sampled(ShaderStages::COMPUTE))
            .read(trace_distance, Access::Sampled(ShaderStages::COMPUTE))
            .write(spatial, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(distance, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.spatial,
                Dispatch::Threads([trace_size[0], trace_size[1], 1]),
                move |ctx| {
                    Some(Push {
                        frame: frame_address,
                        t: [
                            ctx.sampled_index(depth)?,
                            ctx.sampled_index(nr)?,
                            0,
                            ctx.sampled_index(trace)?,
                            ctx.sampled_index(trace_distance)?,
                            0,
                        ],
                        target: ctx.storage_index(spatial)?,
                        read: ctx.storage_index(distance)?,
                        ..Push::default()
                    })
                },
            );
        // The history: last frame's (sampled) and this frame's (written).
        let last = state.current.get();
        let next = 1 - last;
        let previous = state.history[last].import(fg, "ssr history");
        let result = state.history[next].import(fg, "ssr history next");
        let target = state.history[next].storage[0];
        let previous_index = state.history[last].sampled;
        fg.add_compute("ssr temporal", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(nr, Access::Sampled(ShaderStages::COMPUTE))
            .read(weight, Access::Sampled(ShaderStages::COMPUTE))
            .read(spatial, Access::Sampled(ShaderStages::COMPUTE))
            .read(distance, Access::Sampled(ShaderStages::COMPUTE))
            .read(previous, Access::Sampled(ShaderStages::COMPUTE))
            .write(result, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.temporal,
                Dispatch::Threads([width, height, 1]),
                move |ctx| {
                    Some(Push {
                        frame: frame_address,
                        t: [
                            ctx.sampled_index(depth)?,
                            ctx.sampled_index(nr)?,
                            ctx.sampled_index(weight)?,
                            ctx.sampled_index(spatial)?,
                            ctx.sampled_index(distance)?,
                            previous_index,
                        ],
                        target,
                        ..Push::default()
                    })
                },
            );
        state.current.set(next);
        state.valid.set(true);
        state.frame.set(state.frame.get().wrapping_add(1));
        state
            .previous_clip_from_world
            .set(Some(prepared.clip_from_world));
        result
    }
}
