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
const PROBE_TILES: &[u8] = include_bytes!("../shaders/reflections.probeTiles.spv");
const COMPOSITE_VERTEX: &[u8] = include_bytes!("../shaders/reflections.compositeVertex.spv");
const COMPOSITE_FRAGMENT: &[u8] = include_bytes!("../shaders/reflections.compositeFragment.spv");
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
/// Side of the screen tiles probes are listed by, in pixels.
pub const PROBE_TILE: u32 = 16;
/// Most probes one view blends (the smallest that reach it; the rest are dropped).
pub const MAX_VISIBLE_PROBES: usize = 64;
/// Mips of a probe's cube: perceptual roughness `mip / (levels − 1)`, as the sky's.
pub const PROBE_LEVELS: u32 = PREFILTERED_LEVELS;
const PREFILTER_SAMPLES: u32 = 64;
const SSR_DISTANCE_FORMAT: vk::Format = vk::Format::R16_SFLOAT;
/// Levels the nearest-depth pyramid has room for (targets up to 65536 on a side).
const MAX_PYRAMID_LEVELS: u32 = 16;
/// `NO_TEXTURE` in `reflections.slang`.
const NO_TEXTURE: u32 = u32::MAX;

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

/// `Probe` in `reflections.slang` (144 bytes).
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
    _unused: u64,
    probe_rects: u64,
    _pad: [u32; 2],
    ssr: [u32; 4],
    pyramid_offsets: [u32; 16],
}

const _: () = assert!(size_of::<Push>() == 80);
const _: () = assert!(size_of::<ProbeData>() == 144);
const _: () = assert!(size_of::<FrameData>() == 384);

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
    /// A probe for an axis-aligned room: captured from its centre, reflecting and
    /// correcting parallax with the room's box, fading over a tenth of its smallest
    /// half extent.
    pub fn room(center: Vec3, half_extents: Vec3) -> Self {
        let b = ProbeBox::new(center, half_extents);
        Self {
            position: center,
            influence: b,
            parallax: b,
            blend_distance: 0.1 * half_extents.min_element().max(1e-3),
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
            position: self.position.extend(self.intensity.max(0.0)).to_array(),
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
        ProbeTextures {
            array: self.array.import(fg, "reflection probes"),
            source: self.source.import(fg, "reflection probe capture"),
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

    /// The captured probes a view with `clip_from_world` (of a `size` target) can see,
    /// smallest first (at most [`MAX_VISIBLE_PROBES`]), written into `ring` with their
    /// screen rectangles in tiles. `None` when `ring` is full.
    pub(crate) fn view_frame(
        &self,
        ring: &mut UploadRing,
        clip_from_world: Mat4,
        size: [u32; 2],
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
        let tiles = [size[0].div_ceil(PROBE_TILE), size[1].div_ceil(PROBE_TILE)];
        if count == 0 {
            return Some(VisibleProbes {
                count: 0,
                words: 0,
                tiles,
                probes: 0,
                rects: 0,
            });
        }
        let (out, data) = ring.alloc::<ProbeData>(count)?;
        for (out, &(_, slot)) in out.iter_mut().zip(&state.visible) {
            let probe = state.slots[slot as usize]
                .probe
                .unwrap_or_else(|| ReflectionProbe::room(Vec3::ZERO, Vec3::ONE));
            out.write(probe.gpu(slot));
        }
        let (out, rects) = ring.alloc::<[u32; 2]>(count)?;
        for (out, &(_, slot)) in out.iter_mut().zip(&state.visible) {
            let rect = state.slots[slot as usize]
                .probe
                .map_or([0, 0, tiles[0] - 1, tiles[1] - 1], |p| {
                    screen_rect(&p.influence, clip_from_world, size)
                });
            out.write([rect[0] | rect[1] << 16, rect[2] | rect[3] << 16]);
        }
        Some(VisibleProbes {
            count: count as u32,
            words: count.div_ceil(32) as u32,
            tiles,
            probes: data.address,
            rects: rects.address,
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

/// The tiles (inclusive: min x, min y, max x, max y) a box covers on screen; the whole
/// screen when it reaches behind the camera.
fn screen_rect(b: &ProbeBox, clip_from_world: Mat4, size: [u32; 2]) -> [u32; 4] {
    let tiles = [
        size[0].div_ceil(PROBE_TILE).max(1),
        size[1].div_ceil(PROBE_TILE).max(1),
    ];
    let whole = [0, 0, tiles[0] - 1, tiles[1] - 1];
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
    let to_tile = |ndc: f32, pixels: u32, last: u32| -> u32 {
        let pixel = (ndc * 0.5 + 0.5) * pixels as f32;
        ((pixel.max(0.0) as u32) / PROBE_TILE).min(last)
    };
    [
        to_tile(lo.x, size[0], tiles[0] - 1),
        to_tile(lo.y, size[1], tiles[1] - 1),
        to_tile(hi.x, size[0], tiles[0] - 1),
        to_tile(hi.y, size[1], tiles[1] - 1),
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
    words: u32,
    tiles: [u32; 2],
    probes: u64,
    rects: u64,
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
    composite: Pipeline,
    tiles: ComputeShader<Push>,
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

/// What a view's reflection passes read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewInputs<'g> {
    pub color: Texture,
    pub depth: Texture,
    pub normal_roughness: Texture,
    pub weight: Texture,
    pub environment: EnvironmentFrame,
    /// As the depth was drawn (with any jitter).
    pub clip_from_world: Mat4,
    pub camera: Vec3,
    pub size: [u32; 2],
    pub probes: Option<(&'g ReflectionProbes, ProbeTextures)>,
    pub ssr: Option<&'g ScreenSpaceReflections>,
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

impl ReflectionPasses {
    /// Creates the composite pipeline (into `color_format`) and the compute passes.
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
        let fragment = spirv_words(COMPOSITE_FRAGMENT).ok_or_else(invalid)?;
        let formats = [color_format];
        let sources = [
            ComputeSource::<Push>::new(PROBE_TILES, c"probeTiles", &[])?,
            ComputeSource::<Push>::new(SSR_DEPTH, c"ssrDepth", &[])?,
            ComputeSource::<Push>::new(SSR_TRACE, c"ssrTrace", &[])?,
            ComputeSource::<Push>::new(SSR_SPATIAL, c"ssrSpatial", &[])?,
            ComputeSource::<Push>::new(SSR_TEMPORAL, c"ssrTemporal", &[])?,
        ];
        let mut descs = vec![PipelineDesc::Graphics(GraphicsPipelineDesc {
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
                    spirv: &fragment,
                    entry: c"compositeFragment",
                }),
            )
        })];
        descs.extend(sources.iter().map(ComputeSource::desc));
        let mut pipelines =
            Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "reflection pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let composite = next()?;
        let [tiles, depth, trace, spatial, temporal] = &sources;
        Ok(Self {
            composite,
            tiles: tiles.finish(next()?),
            depth: depth.finish(next()?),
            trace: trace.finish(next()?),
            spatial: spatial.finish(next()?),
            temporal: temporal.finish(next()?),
        })
    }

    /// The driver's statistics of the composite pipeline.
    pub(crate) fn composite_statistics(
        &self,
        device: &Device,
    ) -> Vec<engine_rhi::PipelineStatistic> {
        self.composite.statistics(device)
    }

    /// Declares a view's reflection passes: the visible probes' tiles, screen-space
    /// reflections, and the composite into `inputs.color`. `None` when `ring` is full.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn add_view_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        inputs: &ViewInputs<'g>,
    ) -> Option<()> {
        engine_profiler::zone!("render::reflections::add_view_passes");
        let camera = Camera {
            clip_from_world: inputs.clip_from_world,
            position: inputs.camera,
        };
        let view = camera.view_space()?;
        let world_from_view = view.view_from_world.inverse();
        let to_world = |v: Vec3| world_from_view.transform_vector3(v);
        let [width, height] = inputs.size;
        let visible = match inputs.probes {
            Some((probes, _)) => {
                Some(probes.view_frame(ring, inputs.clip_from_world, inputs.size)?)
            }
            None => None,
        };
        let ssr = inputs.ssr.filter(|s| s.size == inputs.size);
        let settings = ssr
            .map(ScreenSpaceReflections::settings)
            .unwrap_or_default();
        let scale = if settings.half_resolution { 2 } else { 1 };
        let trace_size = [width.div_ceil(scale), height.div_ceil(scale)];
        let (levels, offsets, floats) = pyramid_layout(width.max(1), height.max(1));
        let frame_index = ssr.map_or(0, |s| s.frame.get());
        let previous = ssr
            .and_then(|s| s.previous_clip_from_world.get())
            .unwrap_or(inputs.clip_from_world);
        let history_valid = ssr.is_some_and(|s| s.valid.get());
        let frame = FrameData {
            clip_from_world: inputs.clip_from_world.to_cols_array(),
            previous_clip_from_world: previous.to_cols_array(),
            camera: inputs.camera.extend(1.0).to_array(),
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
                inputs.environment.prefiltered_index,
                PREFILTERED_LEVELS,
                Bindless::sampler(SamplerKind::LinearClamp),
                inputs.probes.map_or(0, |(p, _)| p.array_index()),
            ],
            params: [
                inputs.environment.intensity,
                settings.max_roughness.clamp(0.0, 1.0),
                settings.thickness.max(1e-4),
                settings.temporal.clamp(0.01, 1.0),
            ],
            probe_info: [
                visible.map_or(0, |v| v.count),
                visible.map_or(0, |v| v.words),
                visible.map_or(0, |v| v.tiles[0]),
                frame_index,
            ],
            probes: visible.map_or(0, |v| v.probes),
            _unused: 0,
            probe_rects: visible.map_or(0, |v| v.rects),
            _pad: [0; 2],
            ssr: [
                settings.steps.clamp(1, 512),
                scale,
                u32::from(history_valid),
                levels,
            ],
            pyramid_offsets: offsets,
        };
        // The tiles' masks are in a graph buffer, whose address the passes get through
        // their push constants.
        let tile_words = visible.map_or(0, |v| u64::from(v.tiles[0] * v.tiles[1] * v.words));
        let address = ring.push(&[frame])?;
        let tiles = visible.filter(|v| v.count > 0).map(|v| {
            let buffer = fg.create_buffer(
                "reflection probe tiles",
                BufferDesc {
                    size: tile_words * 4,
                },
            );
            (buffer, v)
        });
        let frame_address = address;
        if let Some((buffer, v)) = tiles {
            let shader = &self.tiles;
            fg.add_compute("reflection probe tiles", bindless)
                .write(buffer, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(
                    shader,
                    Dispatch::Threads([v.tiles[0], v.tiles[1], 1]),
                    move |ctx| {
                        Some(Push {
                            frame: frame_address,
                            buffer: ctx.address(buffer),
                            size: v.tiles[1],
                            ..Push::default()
                        })
                    },
                );
        }
        let mut ssr_result = None;
        if let Some(state) = ssr {
            let pyramid = fg.create_buffer(
                "ssr depth pyramid",
                BufferDesc {
                    size: u64::from(floats.max(1)) * 4,
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
            let (nr, weight, color) = (inputs.normal_roughness, inputs.weight, inputs.color);
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
                TextureDesc::new_2d(PROBE_FORMAT, width, height),
            );
            let distance = fg.create_texture(
                "ssr distance",
                TextureDesc::new_2d(SSR_DISTANCE_FORMAT, width, height),
            );
            fg.add_compute("ssr spatial", bindless)
                .read(depth, Access::Sampled(ShaderStages::COMPUTE))
                .read(nr, Access::Sampled(ShaderStages::COMPUTE))
                .read(weight, Access::Sampled(ShaderStages::COMPUTE))
                .read(trace, Access::Sampled(ShaderStages::COMPUTE))
                .read(trace_distance, Access::Sampled(ShaderStages::COMPUTE))
                .write(spatial, Access::StorageWrite(ShaderStages::COMPUTE))
                .write(distance, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(
                    &self.spatial,
                    Dispatch::Threads([width, height, 1]),
                    move |ctx| {
                        Some(Push {
                            frame: frame_address,
                            t: [
                                ctx.sampled_index(depth)?,
                                ctx.sampled_index(nr)?,
                                ctx.sampled_index(weight)?,
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
                                0,
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
                .set(Some(inputs.clip_from_world));
            ssr_result = Some(result);
        }
        // The composite: weight × (reflection − sky) added to the color.
        let (depth, nr, weight) = (inputs.depth, inputs.normal_roughness, inputs.weight);
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
        if let Some((_, textures)) = inputs.probes {
            pass = pass.read(textures.array, Access::Sampled(ShaderStages::FRAGMENT));
        }
        if let Some((buffer, _)) = tiles {
            pass = pass.read(buffer, Access::StorageRead(ShaderStages::FRAGMENT));
        }
        if let Some(result) = ssr_result {
            pass = pass.read(result, Access::Sampled(ShaderStages::FRAGMENT));
        }
        let pipeline = &self.composite;
        pass.execute(move |ctx| {
            let (Some(t0), Some(t1), Some(t2)) = (
                ctx.sampled_index(depth),
                ctx.sampled_index(nr),
                ctx.sampled_index(weight),
            ) else {
                return;
            };
            let push = Push {
                frame: frame_address,
                buffer: tiles.map_or(0, |(b, _)| ctx.address(b)),
                t: [
                    t0,
                    t1,
                    t2,
                    ssr_result
                        .and_then(|r| ctx.sampled_index(r))
                        .unwrap_or(NO_TEXTURE),
                    0,
                    0,
                ],
                ..Push::default()
            };
            let area = ctx.render_area();
            // SAFETY: recording inside the pass's rendering scope; the constants are in
            // the ring, the textures and buffers are read by the pass.
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
        });
        Some(())
    }
}
