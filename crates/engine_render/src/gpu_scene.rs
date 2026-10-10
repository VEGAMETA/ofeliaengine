//! A persistent GPU scene (ADR 0064): the instances to draw live in device buffers across
//! frames, each in a stable slot, and a frame uploads only what changed.
//!
//! - **Slots.** [`GpuScene::insert`] gives an instance a slot and an [`InstanceId`]
//!   (slot index and generation); the slot stays the instance's until
//!   [`GpuScene::remove`]. Freed slots are reused lowest first, so the slots in use stay
//!   dense. Per-view state kept across frames keys off the slot: occlusion culling's
//!   visibility flags ([`crate::forward::Occlusion`]) and last frame's transforms for
//!   motion vectors, so neither depends on any draw order.
//! - **Two device buffers** in the forward renderer's `Instance` layout (64 bytes):
//!   this frame's instances and last frame's (for motion vectors). They swap roles every
//!   frame, so last frame's values become this frame's previous ones without a copy.
//! - **Uploads.** Changes are recorded on the CPU ([`GpuScene::set_transform`] ignores a
//!   transform equal to the current one) and applied once per frame by
//!   [`GpuScene::add_passes`]. Usually one 16-byte update per touched slot plus 64 bytes
//!   of instance data go through the [`UploadRing`], and a compute pass (`scene.slang`)
//!   scatters them; a slot changed last frame and not this one gets a 16-byte update that
//!   carries its values into this frame's buffer (which held the frame before's). When at
//!   least half of the slots changed, the whole array is copied instead, which moves no
//!   more memory than a draw list does. A frame where nothing changed uploads nothing and
//!   records no pass.
//! - **Batches.** Instances that share a mesh and a pipeline state form a batch (a
//!   blended instance is a batch of its own, as in the draw-list path). Membership is
//!   kept incrementally: a batch is created or freed only when the set of
//!   (mesh, state) pairs in the scene changes. The forward renderer lays the batches out
//!   into indirect commands every frame, at a cost that follows the number of batches,
//!   not of instances ([`crate::forward::ForwardRenderer::add_scene_passes`]).
//! - **Holes.** Removed instances leave holes below the highest slot in use; they are
//!   parked in a *sink* batch (id 0) with an inside-out box that frustum culling always
//!   rejects (with culling off they are appended to its command, which nothing draws).
//! - **Motion between ticks** ([`GpuScene::step_motion`], ADR 0064 addendum): an instance
//!   that moved in a simulation tick gets its new transform once per tick (48 bytes and a
//!   slot word: its transform of the tick before is already on the GPU), and every frame a
//!   compute pass (`scene_motion.slang`) places each moving instance between its two ticks
//!   at the frame's interpolation factor ([`GpuScene::set_alpha`]); last frame's placement
//!   is in the previous buffer. The CPU's work per frame does not grow with the moving
//!   instances; their boxes over the whole move ([`SceneFrame::moving_boxes`]) tell cached
//!   shadows and probes what moves.
//!
//! Materials and meshes are resolved when an instance is added or its mesh or material
//! changes: the material's slot and kind, the mesh's bounding box; moves reuse them.
//! Editing or removing a material resolves every instance again, once
//! ([`crate::material::MaterialTable::revision`]); a removed mesh draws nothing, as in
//! the draw-list path.

use crate::compute::{ComputeShader, Dispatch};
use crate::cull::transform_box;
use crate::forward::{BLENDED, InstanceData, MIRRORED, mirrored, state};
use crate::graph::{Access, Buffer, BufferState, FrameGraph, ImportedBuffer, ShaderStages};
use crate::material::{MaterialFrame, MaterialId};
use crate::mesh::{GeometryFrame, MeshId};
use crate::upload::UploadRing;
use core::cmp::Reverse;
use core::hash::{BuildHasherDefault, Hasher};
use core::mem::MaybeUninit;
use engine_core::math::{Affine3A, Mat4, Vec3, Vec4};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};
use std::collections::{BinaryHeap, HashMap};

const SCATTER: &[u8] = include_bytes!("../shaders/scene.sceneScatter.spv");
const MOTION: &[u8] = include_bytes!("../shaders/scene_motion.sceneMotion.spv");
const STEP: &[u8] = include_bytes!("../shaders/scene_motion.sceneStep.spv");
const INTERPOLATE: &[u8] = include_bytes!("../shaders/scene_motion.sceneInterpolate.spv");

/// `KEEP` and `SETTLE` in `scene.slang`.
const KEEP: u32 = u32::MAX;
const SETTLE: u32 = u32::MAX - 1;

/// The batch of holes (removed instances below the highest slot in use).
pub(crate) const SINK: u32 = 0;

/// Bytes per instance on the GPU.
const INSTANCE_BYTES: u64 = size_of::<InstanceData>() as u64;
const UPDATE_BYTES: u64 = size_of::<Update>() as u64;

/// Fewer slots than this always upload by scatter (a full copy saves nothing there).
const FULL_MIN_SLOTS: usize = 4096;

/// What the scene's buffers are left in after a frame: read and written by compute and
/// vertex shaders and copies.
const SCENE_ACCESS: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::from_raw(
        vk::PipelineStageFlags2::COMPUTE_SHADER.as_raw()
            | vk::PipelineStageFlags2::VERTEX_SHADER.as_raw()
            | vk::PipelineStageFlags2::COPY.as_raw(),
    ),
    access: vk::AccessFlags2::from_raw(
        vk::AccessFlags2::SHADER_STORAGE_READ.as_raw()
            | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw()
            | vk::AccessFlags2::TRANSFER_READ.as_raw()
            | vk::AccessFlags2::TRANSFER_WRITE.as_raw(),
    ),
};

/// An instance in a [`GpuScene`]: its slot and the slot's generation. Ids of removed
/// instances are stale: every call ignores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstanceId {
    index: u32,
    generation: u32,
}

impl InstanceId {
    /// The instance's slot: its index in the scene's buffers, stable while it lives.
    pub fn slot(self) -> u32 {
        self.index
    }
}

/// What to draw: a mesh with a material, placed in the world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneInstance {
    /// Its mesh in the geometry pool.
    pub mesh: MeshId,
    /// Its material in the material table.
    pub material: MaterialId,
    /// Object to world transform (affine: the bottom row is ignored). A negative
    /// determinant mirrors the object.
    pub world_from_object: Mat4,
}

/// Pending changes of a slot, applied by the next upload.
mod flag {
    /// In the dirty list.
    pub(super) const QUEUED: u8 = 1;
    /// Inserted: no previous transform.
    pub(super) const NEW: u8 = 2;
    /// Mesh or material changed (or must be resolved again).
    pub(super) const DRAW: u8 = 4;
    /// Transform changed.
    pub(super) const MOVED: u8 = 8;
    /// Moved without motion (teleported).
    pub(super) const TELEPORT: u8 = 16;
    /// Removed: becomes a hole unless inserted into again before the upload.
    pub(super) const REMOVED: u8 = 32;
    /// Held an uploaded instance before this upload (whose box may bound the scene).
    pub(super) const WAS_LIVE: u8 = 64;
    /// Moves between ticks: new transforms of the tick before and the last tick.
    pub(super) const MOTION: u8 = 128;
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    generation: u32,
    alive: bool,
    flags: u8,
    /// Pipeline state bits of the material (`forward.rs`, subsurface included), without
    /// the mirroring bit, which the transform decides.
    kind_state: u8,
    /// The full pipeline state bits.
    state: u8,
    mesh: MeshId,
    material: MaterialId,
    /// The material's slot in the table.
    material_slot: u32,
    batch: u32,
    /// Upload (frame serial) that last wrote it.
    written: u64,
    /// The mesh's object-space box.
    local_center: Vec3,
    local_extents: Vec3,
    /// The world-space box's center.
    center: Vec3,
    /// Its index in [`GpuScene::moving`], or [`NOT_MOVING`].
    moving: u32,
    /// Its pending motion starts from `SlotMotion::from`, not the last upload's `to`.
    explicit_from: bool,
}

/// A batch: instances sharing a mesh and a pipeline state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Batch {
    pub(crate) mesh: MeshId,
    /// Pipeline state bits (`forward.rs`), subsurface included.
    pub(crate) state: u8,
    /// Live instances.
    pub(crate) count: u32,
    /// Sum of the instances' box centers, for the batch's distance from the camera.
    sum: [f64; 3],
    /// A blended batch's one instance.
    blended_slot: u32,
    pub(crate) alive: bool,
}

impl Batch {
    /// Where the batch is, for drawing front to back (blended: its instance's center).
    fn center(&self, scene: &GpuScene) -> Vec3 {
        if self.state as usize & BLENDED != 0 {
            return scene.slots[self.blended_slot as usize].center;
        }
        let n = f64::from(self.count.max(1));
        Vec3::new(
            (self.sum[0] / n) as f32,
            (self.sum[1] / n) as f32,
            (self.sum[2] / n) as f32,
        )
    }
}

/// FxHash (Firefox, rustc): batch keys are two small integers; the default SipHash
/// would cost more than the rest of an insert.
#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }
    fn write_u8(&mut self, i: u8) {
        self.write_u64(u64::from(i));
    }
    fn write_u32(&mut self, i: u32) {
        self.write_u64(u64::from(i));
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

type BatchKey = (MeshId, u8);

/// One slot update, `Update` in `scene.slang` (16 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Update {
    slot: u32,
    current: u32,
    previous: u32,
    _pad: u32,
}

/// `ScatterPush` in `scene.slang` (48 bytes).
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ScatterPush {
    instances: u64,
    previous: u64,
    updates: u64,
    data: u64,
    count: u32,
    _pad: [u32; 3],
}

const _: () = assert!(size_of::<Update>() == 16);
const _: () = assert!(size_of::<ScatterPush>() == 48);

/// The rows of an instance's transform (`InstanceData::rows`).
type Rows = [[f32; 4]; 3];

/// A moving slot's state on the GPU (`scene_motion.slang`): its two transforms as rows in
/// six planes of 16-byte rows by slot, then a word per slot saying which of the two is the
/// last tick's.
const MOTION_PLANES: u64 = 6;
/// Bytes per slot.
const MOTION_BYTES: u64 = MOTION_PLANES * 16 + 4;

/// Not in [`GpuScene::moving`].
const NOT_MOVING: u32 = u32::MAX;

/// In a motion record's slot word: its `to` goes to the slot's second transform (the first
/// holds `from`).
const TO_SECOND: u32 = 1 << 31;
/// In an update's slot word: the instance is new or teleported (no motion in this frame).
const STILL: u32 = 1 << 30;

/// Rows of a motion update (`from`'s, then `to`'s) and of a step (`to`'s).
const UPDATE_ROWS: usize = 6;
const STEP_ROWS: usize = 3;

/// `MotionPush` in `scene_motion.slang` (64 bytes).
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct MotionPush {
    instances: u64,
    previous: u64,
    motion: u64,
    second: u64,
    words: u64,
    rows: u64,
    capacity: u32,
    count: u32,
    alpha: f32,
    _pad: u32,
}

const _: () = assert!(size_of::<MotionPush>() == 64);

/// A moving slot's state on the CPU between ticks.
#[derive(Debug, Clone, Copy)]
struct SlotMotion {
    /// Its transform at the tick before, when the GPU cannot take it from the last upload's
    /// `to` (a motion that starts, or two ticks between uploads: `Slot::explicit_from`).
    from: Rows,
    /// Its box at the last uploaded tick, the next move's starting box.
    to_box: [Vec3; 2],
    /// Whether the GPU holds the last uploaded `to` in the slot's second transform.
    second: bool,
}

/// One of the scene's device buffers.
#[derive(Debug)]
struct SceneBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
}

impl SceneBuffer {
    fn new(device: &Device, allocator: &mut Allocator, slots: u32) -> Result<Self, RhiError> {
        Self::with_bytes(device, allocator, u64::from(slots.max(1)) * INSTANCE_BYTES)
    }

    fn with_bytes(
        device: &Device,
        allocator: &mut Allocator,
        bytes: u64,
    ) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(bytes).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::TRANSFER_SRC
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
        })
    }

    /// # Safety
    ///
    /// The GPU no longer uses it.
    unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// This frame's and last frame's instances.
#[derive(Debug)]
struct Buffers {
    current: SceneBuffer,
    previous: SceneBuffer,
    /// In slots.
    capacity: u32,
}

/// What motion between ticks needs on the GPU, made with the first moving instance: each
/// slot's two transforms ([`MOTION_BYTES`]), the moving slots, and the three passes.
struct MotionGpu {
    motion: SceneBuffer,
    moving: SceneBuffer,
    /// In slots.
    capacity: u32,
    update: ComputeShader<MotionPush>,
    step: ComputeShader<MotionPush>,
    interpolate: ComputeShader<MotionPush>,
}

impl core::fmt::Debug for MotionGpu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MotionGpu")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

/// What the last [`GpuScene::add_passes`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SceneStats {
    /// Live instances.
    pub instances: u32,
    /// Slots up to the highest in use (live instances and holes): what culling visits.
    pub slots: u32,
    /// Live batches.
    pub batches: u32,
    /// Slots whose instance data changed (added, moved, edited, removed).
    pub uploaded: u32,
    /// Slots whose previous transform was settled (moved last frame, still now).
    pub settled: u32,
    /// Bytes written into the upload ring.
    pub upload_bytes: u64,
    /// Whether the whole array was copied (most slots changed) instead of scattered.
    pub full: bool,
    /// Whether the buffers grew this frame.
    pub grew: bool,
    /// Instances moving between ticks, placed on the GPU this frame.
    pub moving: u32,
    /// Moving instances whose ticks' transforms were uploaded this frame.
    pub motion_updates: u32,
}

/// An empty box (dead slots): ignored by the bounds' minimum and maximum.
const NO_BOX: [Vec3; 2] = [Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)];

/// World-space boxes by slot as six arrays (minimum and maximum per axis), so the bounds
/// are six vectorized reductions; dead slots hold [`NO_BOX`].
#[derive(Debug, Default)]
struct Boxes {
    lo: [Vec<f32>; 3],
    hi: [Vec<f32>; 3],
}

impl Boxes {
    fn with_capacity(n: usize) -> Self {
        Self {
            lo: core::array::from_fn(|_| Vec::with_capacity(n)),
            hi: core::array::from_fn(|_| Vec::with_capacity(n)),
        }
    }

    fn push(&mut self, b: [Vec3; 2]) {
        for (k, (lo, hi)) in self.lo.iter_mut().zip(&mut self.hi).enumerate() {
            lo.push(b[0][k]);
            hi.push(b[1][k]);
        }
    }

    #[inline]
    fn get(&self, i: usize) -> [Vec3; 2] {
        [
            Vec3::new(self.lo[0][i], self.lo[1][i], self.lo[2][i]),
            Vec3::new(self.hi[0][i], self.hi[1][i], self.hi[2][i]),
        ]
    }

    #[inline]
    fn set(&mut self, i: usize, b: [Vec3; 2]) {
        for (k, (lo, hi)) in self.lo.iter_mut().zip(&mut self.hi).enumerate() {
            lo[i] = b[0][k];
            hi[i] = b[1][k];
        }
    }

    /// The union of the first `n` boxes.
    fn union(&self, n: usize) -> (Vec3, Vec3) {
        let lo = core::array::from_fn(|k| {
            reduce(&self.lo[k][..n], f32::MAX, |a, b| if b < a { b } else { a })
        });
        let hi = core::array::from_fn(|k| {
            reduce(&self.hi[k][..n], f32::MIN, |a, b| if b > a { b } else { a })
        });
        (Vec3::from_array(lo), Vec3::from_array(hi))
    }
}

/// Folds `values` with `f` in 16 independent lanes (vectorized), then the lanes.
#[inline]
fn reduce(values: &[f32], init: f32, f: impl Fn(f32, f32) -> f32 + Copy) -> f32 {
    let mut lanes = [init; 16];
    let mut chunks = values.chunks_exact(16);
    for chunk in &mut chunks {
        for (lane, &v) in lanes.iter_mut().zip(chunk) {
            *lane = f(*lane, v);
        }
    }
    let tail = chunks.remainder().iter().fold(init, |a, &v| f(a, v));
    lanes.iter().fold(tail, |a, &v| f(a, v))
}

/// The persistent GPU scene (see the module documentation).
pub struct GpuScene {
    slots: Vec<Slot>,
    /// The instance data the GPU holds once the pending changes are uploaded.
    data: Vec<InstanceData>,
    /// World-space boxes by slot, apart from the slots so the bounds scan reads only
    /// them.
    boxes: Boxes,
    /// Free slots below `slots.len()`, lowest first.
    free: BinaryHeap<Reverse<u32>>,
    /// Slots below this are live or holes; culling visits them.
    high_water: u32,
    live: u32,
    /// Slots with pending changes, each once.
    dirty: Vec<u32>,
    /// Slots moved by the last upload and by this one (settled by the next).
    written_last: Vec<u32>,
    written: Vec<u32>,
    serial: u64,
    pub(crate) batches: Vec<Batch>,
    batch_keys: HashMap<BatchKey, u32, BuildHasherDefault<FxHasher>>,
    free_batches: Vec<u32>,
    live_batches: u32,
    /// World-space bounds of the live instances, and whether they must be recomputed
    /// (an instance on the boundary moved inward or went away).
    bounds: (Vec3, Vec3),
    bounds_stale: bool,
    /// The material table's edit count the instances were resolved against.
    material_revision: Option<u64>,
    /// Scatter updates of this upload.
    updates: Vec<Update>,
    /// World boxes that changed in the last upload: of every instance added, moved,
    /// edited or removed, its box before and after (what cached shadows of local lights
    /// check, ADR 0071).
    changes: Vec<[Vec3; 2]>,
    buffers: Option<Buffers>,
    /// Slots the first buffers hold.
    initial_capacity: u32,
    retired: Vec<(u64, Buffers)>,
    frames_in_flight: u64,
    /// The state the graph left the buffers in.
    state: BufferState,
    scatter: ComputeShader<ScatterPush>,
    stats: SceneStats,
    /// Per slot, what moving between ticks needs on the CPU ([`Self::set_motion`]).
    motion: Vec<SlotMotion>,
    /// Slots placed between ticks on the GPU every frame, and their boxes over the whole
    /// move (the union of the two ticks' boxes), in the same order.
    moving: Vec<u32>,
    moving_boxes: Vec<[Vec3; 2]>,
    /// Whether `moving` changed since its last upload.
    moving_changed: bool,
    /// The interpolation factor of the next upload ([`Self::set_alpha`]).
    alpha: f32,
    /// This upload's motion records' slot words (the updates', then the steps').
    motion_words: Vec<u32>,
    motion_gpu: Option<MotionGpu>,
    /// Motion buffers replaced by growth, freed once their frames are done.
    retired_motion: Vec<(u64, SceneBuffer)>,
    /// The state the graph left the motion buffers in.
    motion_state: BufferState,
}

impl core::fmt::Debug for GpuScene {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuScene")
            .field("live", &self.live)
            .field("slots", &self.high_water)
            .field("batches", &self.live_batches)
            .finish_non_exhaustive()
    }
}

/// A hole: an empty box, parked in the sink batch.
/// A hole: parked in the sink batch, whose inside-out box (negative extents, see
/// `ForwardRenderer::add_scene_passes`) fails every frustum plane, so culling drops it
/// without an atomic. The identity basis keeps those extents as they are.
const HOLE: InstanceData = InstanceData {
    rows: [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ],
    material: 0,
    batch: SINK,
    _pad: [0; 2],
};

fn rows_of_affine(a: &Affine3A) -> Rows {
    let m = a.matrix3;
    let t = a.translation;
    [
        [m.x_axis.x, m.y_axis.x, m.z_axis.x, t.x],
        [m.x_axis.y, m.y_axis.y, m.z_axis.y, t.y],
        [m.x_axis.z, m.y_axis.z, m.z_axis.z, t.z],
    ]
}

fn rows_of(m: &Mat4) -> [[f32; 4]; 3] {
    let t = m.transpose();
    [
        t.x_axis.to_array(),
        t.y_axis.to_array(),
        t.z_axis.to_array(),
    ]
}

fn matrix_of(r: &[[f32; 4]; 3]) -> Mat4 {
    Mat4::from_cols(
        Vec4::new(r[0][0], r[1][0], r[2][0], 0.0),
        Vec4::new(r[0][1], r[1][1], r[2][1], 0.0),
        Vec4::new(r[0][2], r[1][2], r[2][2], 0.0),
        Vec4::new(r[0][3], r[1][3], r[2][3], 1.0),
    )
}

fn same_rows(a: &[[f32; 4]; 3], b: &[[f32; 4]; 3]) -> bool {
    a.iter()
        .flatten()
        .zip(b.iter().flatten())
        .all(|(x, y)| x.to_bits() == y.to_bits())
}

/// This upload's room in the ring, in 16-byte units: the scattered instance data (four
/// units each; none when the whole array is copied), then the motion records' rows by row
/// (row `r` of update `i` at `r × updates + i`, `from`'s three rows then `to`'s; after
/// them row `k` of step `j` at `k × steps + j`), then the records' slot words, four per
/// unit (the updates', then the steps'). The GPU reads each in whole cache lines.
struct Room<'a> {
    units: &'a mut [MaybeUninit<[u32; 4]>],
    /// Whether the whole array is copied (the instance data stays in the CPU array).
    full: bool,
    scattered: usize,
    updates: usize,
    steps: usize,
    next_data: usize,
    next_update: usize,
    next_step: usize,
}

impl Room<'_> {
    /// Units for `scattered` instances, `updates` and `steps`.
    fn units(scattered: usize, updates: usize, steps: usize) -> usize {
        4 * scattered + UPDATE_ROWS * updates + STEP_ROWS * steps + (updates + steps).div_ceil(4)
    }

    fn rows_base(&self) -> usize {
        4 * self.scattered
    }

    fn words_base(&self) -> usize {
        self.rows_base() + UPDATE_ROWS * self.updates + STEP_ROWS * self.steps
    }

    /// Scattered instance `at`.
    fn data(&mut self, at: usize, data: &InstanceData) {
        for (k, row) in data.rows.iter().enumerate() {
            self.units[4 * at + k].write(row.map(f32::to_bits));
        }
        self.units[4 * at + 3].write([data.material, data.batch, data._pad[0], data._pad[1]]);
    }

    /// Update `i`'s rows.
    fn update(&mut self, i: usize, from: &Rows, to: &Rows) {
        let base = self.rows_base();
        for (r, row) in from.iter().chain(to).enumerate() {
            self.units[base + r * self.updates + i].write(row.map(f32::to_bits));
        }
    }

    /// Step `j`'s rows.
    fn step(&mut self, j: usize, to: &Rows) {
        let base = self.rows_base() + UPDATE_ROWS * self.updates;
        for (k, row) in to.iter().enumerate() {
            self.units[base + k * self.steps + j].write(row.map(f32::to_bits));
        }
    }

    /// The records' slot words.
    fn words(&mut self, words: &[u32]) {
        let base = self.words_base();
        for (k, chunk) in words.chunks(4).enumerate() {
            let mut unit = [0; 4];
            unit[..chunk.len()].copy_from_slice(chunk);
            self.units[base + k].write(unit);
        }
    }
}

/// What a written slot's previous transform becomes.
#[derive(Debug, Clone, Copy)]
enum Previous {
    /// Last frame's value (the buffers swapped): it moved.
    Keep,
    /// The new value too: added, teleported or removed (no motion in this frame, and the
    /// next swap cannot bring a removed instance back).
    Same,
}

impl GpuScene {
    /// An empty scene with room for `capacity` instances on the GPU (it grows as
    /// needed); `frames_in_flight` bounds how long replaced buffers are kept.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        capacity: u32,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let scatter =
            ComputeShader::<ScatterPush>::new(device, bindless, SCATTER, c"sceneScatter")?;
        let reserve = capacity.max(1) as usize;
        let mut scene = Self {
            slots: Vec::with_capacity(reserve),
            data: Vec::with_capacity(reserve),
            boxes: Boxes::with_capacity(reserve),
            free: BinaryHeap::new(),
            high_water: 0,
            live: 0,
            dirty: Vec::with_capacity(reserve),
            written_last: Vec::with_capacity(reserve),
            written: Vec::with_capacity(reserve),
            serial: 0,
            batches: Vec::new(),
            batch_keys: HashMap::default(),
            free_batches: Vec::new(),
            live_batches: 0,
            bounds: (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
            bounds_stale: false,
            material_revision: None,
            updates: Vec::with_capacity(reserve),
            changes: Vec::with_capacity(2 * reserve),
            buffers: None,
            initial_capacity: capacity.max(1),
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            state: BufferState::idle(),
            scatter,
            stats: SceneStats::default(),
            motion: Vec::with_capacity(reserve),
            moving: Vec::new(),
            moving_boxes: Vec::new(),
            moving_changed: false,
            alpha: 1.0,
            motion_words: Vec::new(),
            motion_gpu: None,
            retired_motion: Vec::new(),
            motion_state: BufferState::idle(),
        };
        // The sink: holes cull into it, nothing draws it.
        scene.batches.push(Batch {
            mesh: bytemuck::Zeroable::zeroed(),
            state: BLENDED as u8,
            count: 0,
            sum: [0.0; 3],
            blended_slot: 0,
            alive: false,
        });
        Ok(scene)
    }

    /// Live instances.
    pub fn len(&self) -> usize {
        self.live as usize
    }

    /// Whether the scene has no instances.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Slots the scene has used so far (live, holes and free): per-view states known by
    /// slot ([`crate::forward::Occlusion`]) remember every instance when they are at
    /// least this large.
    pub fn slot_count(&self) -> u32 {
        self.slots.len() as u32
    }

    /// What the last [`Self::add_passes`] did.
    pub fn stats(&self) -> SceneStats {
        self.stats
    }

    fn slot_mut(&mut self, id: InstanceId) -> Option<&mut Slot> {
        self.slots
            .get_mut(id.index as usize)
            .filter(|s| s.alive && s.generation == id.generation)
    }

    fn queue(&mut self, index: u32, flags: u8) {
        let slot = &mut self.slots[index as usize];
        if slot.flags & flag::QUEUED == 0 {
            self.dirty.push(index);
        }
        slot.flags |= flags | flag::QUEUED;
    }

    /// Adds an instance; it is drawn from the next [`Self::add_passes`] on, without
    /// motion in its first frame. Allocates only when the scene outgrows its capacity.
    pub fn insert(&mut self, instance: SceneInstance) -> InstanceId {
        let index = match self.free.pop() {
            Some(Reverse(index)) => index,
            None => {
                let index = self.slots.len() as u32;
                self.slots.push(Slot {
                    generation: 0,
                    alive: false,
                    flags: 0,
                    kind_state: 0,
                    state: 0,
                    mesh: instance.mesh,
                    material: instance.material,
                    material_slot: 0,
                    batch: SINK,
                    written: 0,
                    local_center: Vec3::ZERO,
                    local_extents: Vec3::ZERO,
                    center: Vec3::ZERO,
                    moving: NOT_MOVING,
                    explicit_from: false,
                });
                self.data.push(HOLE);
                self.boxes.push(NO_BOX);
                self.motion.push(SlotMotion {
                    from: HOLE.rows,
                    to_box: NO_BOX,
                    second: false,
                });
                index
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.alive = true;
        slot.mesh = instance.mesh;
        slot.material = instance.material;
        let generation = slot.generation;
        self.data[index as usize].rows = rows_of(&instance.world_from_object);
        self.live += 1;
        self.queue(index, flag::NEW | flag::DRAW);
        InstanceId { index, generation }
    }

    /// Removes an instance (stale ids are ignored). Its slot is free at once: an insert
    /// before the next upload takes it over (lowest free slot first), so removing and
    /// adding as many instances leaves no holes. Returns whether it was live.
    pub fn remove(&mut self, id: InstanceId) -> bool {
        if self.slot_mut(id).is_none() {
            return false;
        }
        self.stop_motion(id.index);
        let slot = &mut self.slots[id.index as usize];
        slot.alive = false;
        slot.generation = slot.generation.wrapping_add(1);
        let was_live = if slot.flags & flag::NEW == 0 {
            flag::WAS_LIVE
        } else {
            0
        };
        self.live -= 1;
        self.queue(id.index, flag::REMOVED | was_live);
        self.free.push(Reverse(id.index));
        true
    }

    /// Moves an instance. A transform equal (bit for bit) to the current one changes
    /// nothing; otherwise the instance's motion vectors show the move. Returns whether
    /// the transform changed.
    pub fn set_transform(&mut self, id: InstanceId, world_from_object: &Mat4) -> bool {
        self.set_rows(id, rows_of(world_from_object))
    }

    /// [`Self::set_transform`] from an affine transform (an ECS world transform), without
    /// going through a 4 × 4 matrix.
    pub fn set_affine(&mut self, id: InstanceId, world_from_object: &Affine3A) -> bool {
        self.set_rows(id, rows_of_affine(world_from_object))
    }

    fn set_rows(&mut self, id: InstanceId, rows: [[f32; 4]; 3]) -> bool {
        if self.slot_mut(id).is_none() {
            return false;
        }
        // A moving instance stops at the transform (whatever it held).
        let moving = self.slots[id.index as usize].moving != NOT_MOVING;
        if !moving && same_rows(&self.data[id.index as usize].rows, &rows) {
            return false;
        }
        self.stop_motion(id.index);
        self.data[id.index as usize].rows = rows;
        self.queue(id.index, flag::MOVED);
        true
    }

    /// Moves an instance without motion in this frame (a teleport, a respawn): its
    /// previous transform is the new one too.
    pub fn teleport(&mut self, id: InstanceId, world_from_object: &Mat4) {
        if self.slot_mut(id).is_none() {
            return;
        }
        self.stop_motion(id.index);
        self.data[id.index as usize].rows = rows_of(world_from_object);
        self.queue(id.index, flag::MOVED | flag::TELEPORT);
    }

    /// Moves an instance between two simulation ticks (ADR 0064 addendum): from `from`
    /// (its transform at the tick before) to `to` (at the last tick). Every frame the GPU
    /// places it between them at [`Self::set_alpha`]'s factor, and its motion vectors
    /// follow; the CPU's work per frame does not grow with the moving instances. Call it
    /// once per tick for each instance that moved in it (the last call before an upload
    /// counts); [`Self::set_transform`], [`Self::set_affine`], [`Self::teleport`] or
    /// [`Self::remove`] stop the motion. Returns whether anything changed.
    pub fn set_motion(&mut self, id: InstanceId, from: &Affine3A, to: &Affine3A) -> bool {
        if self.slot_mut(id).is_none() {
            return false;
        }
        let (from, to) = (rows_of_affine(from), rows_of_affine(to));
        let i = id.index as usize;
        let slot = self.slots[i];
        let moving = slot.moving != NOT_MOVING;
        // The GPU starts the move from the last upload's `to` when the slot moved then
        // and nothing changed it since (else, from `from`).
        let continues =
            moving && slot.flags & flag::MOTION == 0 && same_rows(&self.data[i].rows, &from);
        if !continues {
            self.motion[i].from = from;
            self.slots[i].explicit_from = true;
        }
        self.data[i].rows = to;
        if !moving {
            let slot = &mut self.slots[i];
            slot.moving = self.moving.len() as u32;
            self.moving.push(id.index);
            self.moving_boxes.push(NO_BOX);
            self.moving_changed = true;
        }
        self.queue(id.index, flag::MOTION);
        true
    }

    /// [`Self::set_motion`] from the transform the instance was last given (its last
    /// tick's): `to` is the new tick's. What an extractor that mirrors every move calls:
    /// it converts and compares one transform instead of two.
    pub fn step_motion(&mut self, id: InstanceId, to: &Affine3A) -> bool {
        if self.slot_mut(id).is_none() {
            return false;
        }
        let i = id.index as usize;
        let slot = self.slots[i];
        let moving = slot.moving != NOT_MOVING;
        if !moving || slot.flags & flag::MOTION != 0 {
            // It starts moving, or ticks again before an upload: from what it holds.
            self.motion[i].from = self.data[i].rows;
            self.slots[i].explicit_from = true;
        }
        self.data[i].rows = rows_of_affine(to);
        if !moving {
            let slot = &mut self.slots[i];
            slot.moving = self.moving.len() as u32;
            self.moving.push(id.index);
            self.moving_boxes.push(NO_BOX);
            self.moving_changed = true;
        }
        self.queue(id.index, flag::MOTION);
        true
    }

    /// The interpolation factor between the last two ticks that the next
    /// [`Self::add_passes`] places moving instances at (`FixedStep::alpha`: 0 at the tick
    /// before, 1 at the last tick).
    pub fn set_alpha(&mut self, alpha: f32) {
        self.alpha = alpha;
    }

    /// Instances moving between ticks ([`Self::set_motion`]).
    pub fn moving(&self) -> usize {
        self.moving.len()
    }

    /// Takes slot `index` out of the moving instances (if it is one).
    fn stop_motion(&mut self, index: u32) {
        let k = self.slots[index as usize].moving;
        if k == NOT_MOVING {
            return;
        }
        self.moving.swap_remove(k as usize);
        self.moving_boxes.swap_remove(k as usize);
        if let Some(&moved) = self.moving.get(k as usize) {
            self.slots[moved as usize].moving = k;
        }
        let slot = &mut self.slots[index as usize];
        slot.moving = NOT_MOVING;
        slot.explicit_from = false;
        self.moving_changed = true;
    }

    /// Changes an instance's mesh.
    pub fn set_mesh(&mut self, id: InstanceId, mesh: MeshId) {
        let Some(slot) = self.slot_mut(id) else {
            return;
        };
        if slot.mesh != mesh {
            slot.mesh = mesh;
            self.queue(id.index, flag::DRAW);
        }
    }

    /// Takes an instance's mesh box from the geometry pool again in the next upload: after
    /// `GeometryPool::set_bounds` on its mesh (a skinned mesh, `crate::skinning`), so the
    /// scene's bounds and its batch order follow the new box.
    pub fn refresh_bounds(&mut self, id: InstanceId) {
        if self.slot_mut(id).is_some() {
            self.queue(id.index, flag::DRAW);
        }
    }

    /// Changes an instance's material.
    pub fn set_material(&mut self, id: InstanceId, material: MaterialId) {
        let Some(slot) = self.slot_mut(id) else {
            return;
        };
        if slot.material != material {
            slot.material = material;
            self.queue(id.index, flag::DRAW);
        }
    }

    /// The instance, if `id` is live.
    pub fn get(&self, id: InstanceId) -> Option<SceneInstance> {
        let slot = self
            .slots
            .get(id.index as usize)
            .filter(|s| s.alive && s.generation == id.generation)?;
        Some(SceneInstance {
            mesh: slot.mesh,
            material: slot.material,
            world_from_object: matrix_of(&self.data[id.index as usize].rows),
        })
    }

    /// Removes every instance.
    pub fn clear(&mut self) {
        for index in 0..self.slots.len() as u32 {
            if self.slots[index as usize].alive {
                let generation = self.slots[index as usize].generation;
                self.remove(InstanceId { index, generation });
            }
        }
    }

    /// World-space bounds of the live instances (as of the last upload).
    pub fn bounds(&self) -> (Vec3, Vec3) {
        self.bounds
    }

    /// Leaves `slot`'s batch.
    fn leave(&mut self, index: u32) {
        let slot = self.slots[index as usize];
        if slot.batch == SINK {
            return;
        }
        let batch = &mut self.batches[slot.batch as usize];
        batch.count -= 1;
        for k in 0..3 {
            batch.sum[k] -= f64::from(slot.center[k]);
        }
        if batch.count == 0 {
            batch.alive = false;
            if batch.state as usize & BLENDED == 0 {
                self.batch_keys.remove(&(batch.mesh, batch.state));
            }
            self.free_batches.push(slot.batch);
            self.live_batches -= 1;
        }
        self.slots[index as usize].batch = SINK;
    }

    /// The batch for (`mesh`, `state`) of slot `index`, created if needed.
    fn batch_for(&mut self, index: u32, mesh: MeshId, state_bits: u8) -> u32 {
        let blended = state_bits as usize & BLENDED != 0;
        if !blended && let Some(&batch) = self.batch_keys.get(&(mesh, state_bits)) {
            return batch;
        }
        let batch = Batch {
            mesh,
            state: state_bits,
            count: 0,
            sum: [0.0; 3],
            blended_slot: index,
            alive: true,
        };
        let id = match self.free_batches.pop() {
            Some(id) => {
                self.batches[id as usize] = batch;
                id
            }
            None => {
                self.batches.push(batch);
                self.batches.len() as u32 - 1
            }
        };
        if !blended {
            self.batch_keys.insert((mesh, state_bits), id);
        }
        self.live_batches += 1;
        id
    }

    /// Whether a box lies on the bounds (moving it inward or removing it can shrink
    /// them).
    fn on_boundary(&self, b: [Vec3; 2]) -> bool {
        let (lo, hi) = self.bounds;
        b[0].cmple(lo).any() || b[1].cmpge(hi).any()
    }

    /// Applies a slot's pending changes to the CPU state, and writes its update, its data
    /// (in scatter mode) and its motion record into the ring.
    fn apply(
        &mut self,
        index: u32,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
        room: &mut Room<'_>,
        incremental_bounds: bool,
    ) {
        let flags = self.slots[index as usize].flags;
        self.slots[index as usize].flags = 0;
        // Only its motion between ticks changed: cached shadows and probes learn of it from
        // the moving boxes every frame, and the batch keeps the center it had (the order
        // of batches by distance need not follow every tick).
        let motion_only = flags & !(flag::QUEUED | flag::MOTION) == 0;
        // The box it had: whatever it covered may see a shadow change.
        if !motion_only && flags & (flag::WAS_LIVE | flag::NEW) != flag::NEW {
            let old = self.boxes.get(index as usize);
            if old[0].x <= old[1].x {
                self.changes.push(old);
            }
        }
        // An instance removed from it: its box may have bounded the scene.
        if flags & flag::WAS_LIVE != 0
            && incremental_bounds
            && self.on_boundary(self.boxes.get(index as usize))
        {
            self.bounds_stale = true;
        }
        if flags & flag::REMOVED != 0 && !self.slots[index as usize].alive {
            // A hole (the slot is already free).
            self.boxes.set(index as usize, NO_BOX);
            self.leave(index);
            self.data[index as usize] = HOLE;
            self.write(index, HOLE, Previous::Same, room);
            self.slots[index as usize].written = self.serial;
            return;
        }
        // Otherwise live: added, moved or edited; removed and taken over by a new instance
        // counts as added (`NEW`), keeping the batch if it draws the same.
        let i = index as usize;
        let world_from_object = matrix_of(&self.data[i].rows);
        if flags & flag::DRAW != 0 {
            let slot = self.slots[index as usize];
            let (material_slot, kind) = materials.resolve(slot.material);
            let (local_center, local_extents) = geometry
                .mesh(slot.mesh)
                .map_or((Vec3::ZERO, Vec3::ZERO), |m| {
                    ((m.min + m.max) * 0.5, (m.max - m.min) * 0.5)
                });
            let s = &mut self.slots[index as usize];
            s.material_slot = material_slot;
            s.kind_state = state(kind, false, true) as u8;
            s.local_center = local_center;
            s.local_extents = local_extents;
        }
        let slot = &self.slots[i];
        let (kind_state, mesh, batch_id, old_center) =
            (slot.kind_state, slot.mesh, slot.batch, slot.center);
        let state_bits = kind_state
            | if mirrored(&world_from_object) {
                MIRRORED as u8
            } else {
                0
            };
        // The world box, as the draw-list path computes it; a moving instance's covers its
        // whole move between the two ticks.
        let (center, extents) =
            transform_box(&world_from_object, slot.local_center, slot.local_extents);
        let mut new_box = [center - extents, center + extents];
        let moving = slot.moving;
        if moving != NOT_MOVING {
            // From the last tick's box, or the explicit `from`'s.
            let from_box = if slot.explicit_from || flags & flag::MOTION == 0 {
                let (c, e) = transform_box(
                    &matrix_of(&self.motion[i].from),
                    slot.local_center,
                    slot.local_extents,
                );
                [c - e, c + e]
            } else {
                self.motion[i].to_box
            };
            self.motion[i].to_box = new_box;
            new_box[0] = new_box[0].min(from_box[0]);
            new_box[1] = new_box[1].max(from_box[1]);
            self.moving_boxes[moving as usize] = new_box;
        }
        if incremental_bounds {
            if flags & (flag::NEW | flag::WAS_LIVE) == 0 && self.on_boundary(self.boxes.get(i)) {
                self.bounds_stale = true;
            }
            self.bounds = (self.bounds.0.min(new_box[0]), self.bounds.1.max(new_box[1]));
        }
        self.boxes.set(i, new_box);
        if !motion_only {
            self.changes.push(new_box);
        }
        // Batch membership: kept unless the mesh or the state changed.
        let batch = &mut self.batches[batch_id as usize];
        // (Not a blended one's: transparent surfaces are drawn back to front by it.)
        let keep_center = motion_only && state_bits as usize & BLENDED == 0;
        if batch_id != SINK && batch.mesh == mesh && batch.state == state_bits {
            if !keep_center {
                for k in 0..3 {
                    batch.sum[k] += f64::from(center[k] - old_center[k]);
                }
            }
        } else {
            self.leave(index);
            let id = self.batch_for(index, mesh, state_bits);
            let batch = &mut self.batches[id as usize];
            batch.count += 1;
            for k in 0..3 {
                batch.sum[k] += f64::from(center[k]);
            }
            self.slots[i].batch = id;
        }
        let s = &mut self.slots[i];
        s.state = state_bits;
        // (A batch it stayed in counts its old center while it moves: keep that one.)
        if !keep_center || s.batch != batch_id {
            s.center = center;
        }
        let explicit_from = core::mem::replace(&mut s.explicit_from, false);
        let d = &mut self.data[i];
        d.material = s.material_slot;
        d.batch = s.batch;
        let to = d.rows;
        let still = flags & (flag::NEW | flag::TELEPORT) != 0;
        if moving != NOT_MOVING && flags & flag::MOTION != 0 {
            // Each record writes its `to` over the older of the slot's two transforms.
            let second = !self.motion[i].second;
            self.motion[i].second = second;
            let word = index | if second { TO_SECOND } else { 0 };
            if explicit_from || still {
                let k = room.next_update;
                room.next_update += 1;
                self.motion_words[k] = word | if still { STILL } else { 0 };
                room.update(k, &self.motion[i].from, &to);
            } else {
                let k = room.next_step;
                room.next_step += 1;
                self.motion_words[room.updates + k] = word;
                room.step(k, &to);
            }
        }
        // Only the motion changed: the interpolation pass writes the slot's transforms (a
        // slot changed last frame still settles).
        if motion_only {
            return;
        }
        self.slots[i].written = self.serial;
        if !still {
            // Its previous transform differs now: settled by the next upload unless it
            // changes again.
            self.written.push(index);
        }
        let previous = if still {
            Previous::Same
        } else {
            Previous::Keep
        };
        self.write(index, self.data[i], previous, room);
    }

    /// Slot `index`'s new `data`: into the ring with its update (scatter), or only the
    /// update of its previous value when the whole array is copied (a new, teleported or
    /// removed slot's is its new value, from the copied array).
    fn write(&mut self, index: u32, data: InstanceData, previous: Previous, room: &mut Room<'_>) {
        let (current, at) = if room.full {
            (KEEP, index)
        } else {
            let at = room.next_data;
            room.data(at, &data);
            room.next_data += 1;
            (at as u32, at as u32)
        };
        let previous = match previous {
            Previous::Keep => KEEP,
            Previous::Same => at,
        };
        if current != KEEP || previous != KEEP {
            self.updates.push(Update {
                slot: index,
                current,
                previous,
                _pad: 0,
            });
        }
    }

    /// Applies the changes since the last call and declares their upload in `fg`, after
    /// growing the scene's buffers if it outgrew them. Call it once per frame, after the
    /// frame's changes and before the views draw the scene (with the returned
    /// [`SceneFrame`]). Returns `None`, changing nothing, when `ring` lacks the room.
    ///
    /// Allocates only when the scene grows (or a new batch needs room in its tables).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn add_passes<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        allocator: &mut Allocator,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
        frame_number: u64,
    ) -> Result<Option<SceneFrame<'g>>, RhiError> {
        engine_profiler::zone!("render::GpuScene::add_passes");
        self.collect_retired(allocator, frame_number);
        // Material edits: every instance is resolved again (rare: level loads, editing).
        let revision = materials.revision();
        if self.material_revision != Some(revision) {
            self.material_revision = Some(revision);
            for index in 0..self.slots.len() as u32 {
                if self.slots[index as usize].alive {
                    self.queue(index, flag::DRAW);
                }
            }
        }
        // Scatter (one update per touched slot) or the whole array, when most slots
        // changed. Room in the ring first: nothing changes when it is missing.
        let slot_total = self.slots.len();
        let touched = self.dirty.len();
        // Slots whose instance data changes (one that only moves between ticks writes
        // none: the interpolation pass places it), and the motion records: an update for a
        // slot that starts moving, ticked twice since the last upload, or is new or
        // teleported; a step for one that keeps moving.
        let (mut data_touched, mut motion_updates, mut motion_steps) = (0, 0, 0);
        for &index in &self.dirty {
            let slot = &self.slots[index as usize];
            if slot.flags & !(flag::QUEUED | flag::MOTION) != 0 {
                data_touched += 1;
            }
            if slot.moving != NOT_MOVING && slot.flags & flag::MOTION != 0 {
                if slot.explicit_from || slot.flags & (flag::NEW | flag::TELEPORT) != 0 {
                    motion_updates += 1;
                } else {
                    motion_steps += 1;
                }
            }
        }
        let full = slot_total >= FULL_MIN_SLOTS && 2 * data_touched >= slot_total;
        let scattered = if full { 0 } else { data_touched };
        let room_units = Room::units(scattered, motion_updates, motion_steps);
        // The buffers grow to hold every slot (the holes above the highest live slot too:
        // their hole data may still be in flight). The motion buffers are made with the
        // first moving instance and grow with them; the moving slots' list goes up whole
        // when it changed or they were made again.
        let capacity = self.buffers.as_ref().map_or(0, |b| b.capacity);
        let needed_slots = (slot_total as u32).max(1);
        let scene_capacity = if needed_slots > capacity {
            needed_slots
                .max(capacity.saturating_mul(2))
                .max(self.initial_capacity)
        } else {
            capacity
        };
        let motion_capacity = self.motion_gpu.as_ref().map_or(0, |gpu| gpu.capacity);
        let motion_remade = !self.moving.is_empty() && motion_capacity < scene_capacity;
        let list = !self.moving.is_empty() && (self.moving_changed || motion_remade);
        let needed = room_units as u64 * 16
            + if list {
                self.moving.len() as u64 * 4
            } else {
                0
            }
            + if full {
                slot_total as u64 * INSTANCE_BYTES + data_touched as u64 * UPDATE_BYTES
            } else {
                (data_touched + self.written_last.len()) as u64 * UPDATE_BYTES
            };
        if needed + 256 > ring.available() {
            return Ok(None);
        }
        let moving_staged = if list {
            let Some(staged) = ring.stage(&self.moving) else {
                return Ok(None);
            };
            Some(staged)
        } else {
            None
        };
        let Some((units, room_staged)) = ring.alloc::<[u32; 4]>(room_units) else {
            return Ok(None);
        };
        let mut room = Room {
            units,
            full,
            scattered,
            updates: motion_updates,
            steps: motion_steps,
            next_data: 0,
            next_update: 0,
            next_step: 0,
        };
        self.moving_changed = false;
        self.serial += 1;
        core::mem::swap(&mut self.written, &mut self.written_last);
        self.written.clear();
        self.updates.clear();
        self.changes.clear();
        self.motion_words.clear();
        self.motion_words.resize(motion_updates + motion_steps, 0);
        // Bounds: kept incrementally for a few changes, recomputed for many.
        let incremental_bounds = 8 * touched < slot_total;
        if !incremental_bounds {
            self.bounds_stale = true;
        }
        for k in 0..touched {
            let index = self.dirty[k];
            self.apply(index, geometry, materials, &mut room, incremental_bounds);
        }
        debug_assert_eq!(
            (room.next_data, room.next_update, room.next_step),
            (scattered, motion_updates, motion_steps)
        );
        room.words(&self.motion_words);
        let uploaded = data_touched as u32;
        // Slots changed last frame and not now: this frame's buffer held the frame
        // before's values, so theirs come forward (a full copy writes every slot).
        let mut settled = 0;
        if !full {
            for k in 0..self.written_last.len() {
                let index = self.written_last[k];
                let slot = self.slots[index as usize];
                if slot.alive && slot.written != self.serial {
                    self.updates.push(Update {
                        slot: index,
                        current: SETTLE,
                        previous: KEEP,
                        _pad: 0,
                    });
                    settled += 1;
                }
            }
        }
        // Culling stops at the highest slot in use.
        self.dirty.clear();
        self.high_water = self.slots.len() as u32;
        while self.high_water > 0 && !self.slots[self.high_water as usize - 1].alive {
            self.high_water -= 1;
        }
        if self.bounds_stale {
            self.bounds_stale = false;
            self.bounds = self.boxes.union(self.high_water as usize);
        }
        let full_data = if full {
            ring.stage(&self.data)
                .map(|s| (s, self.data.len() as u64 * INSTANCE_BYTES))
        } else {
            None
        };
        let updates_staged = if self.updates.is_empty() {
            None
        } else {
            ring.stage(&self.updates)
        };

        let mut grew = None;
        if needed_slots > capacity {
            let fresh = Buffers {
                current: SceneBuffer::new(device, allocator, scene_capacity)?,
                previous: SceneBuffer::new(device, allocator, scene_capacity)?,
                capacity: scene_capacity,
            };
            grew = self.buffers.replace(fresh);
        }
        let Some(buffers) = self.buffers.as_mut() else {
            return Ok(None);
        };
        if capacity > 0 && grew.is_none() {
            // Last frame's values become this frame's previous ones, and this frame's go
            // to the buffer that held the frame before's.
            core::mem::swap(&mut buffers.current, &mut buffers.previous);
        }
        let size = u64::from(buffers.capacity) * INSTANCE_BYTES;
        let initial = if grew.is_some() || capacity == 0 {
            BufferState::idle()
        } else {
            self.state
        };
        let import = |fg: &mut FrameGraph<'g>, name, b: &SceneBuffer| {
            fg.import_buffer(
                name,
                ImportedBuffer {
                    buffer: b.buffer,
                    size,
                    address: b.address,
                    initial,
                    final_state: Some(SCENE_ACCESS),
                },
            )
        };
        let current = import(fg, "scene instances", &buffers.current);
        let previous = import(fg, "scene previous instances", &buffers.previous);
        let (current_address, previous_address) =
            (buffers.current.address, buffers.previous.address);
        if let Some(old) = grew {
            let bytes = u64::from(old.capacity) * INSTANCE_BYTES;
            let old_current = fg.import_buffer(
                "scene instances (old)",
                ImportedBuffer {
                    buffer: old.current.buffer,
                    size: bytes,
                    address: old.current.address,
                    initial: self.state,
                    final_state: None,
                },
            );
            // Last frame's values: this frame's previous ones, and what this frame's
            // changes apply to.
            fg.add_pass("scene grow")
                .read(old_current, Access::TransferRead)
                .write(current, Access::TransferWrite)
                .write(previous, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = [vk::BufferCopy {
                        src_offset: 0,
                        dst_offset: 0,
                        size: bytes,
                    }];
                    // SAFETY: recording outside a rendering scope; the new buffers are
                    // larger than the old one.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(old_current),
                            ctx.buffer(current),
                            &region,
                        );
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(old_current),
                            ctx.buffer(previous),
                            &region,
                        );
                    }
                });
            self.retired.push((frame_number, old));
        }
        // Motion between ticks: its buffers and passes, made with the first moving
        // instance and grown with the scene's.
        let motion_initial = self.motion_state;
        let mut motion_grew = None;
        if motion_remade {
            let motion = SceneBuffer::with_bytes(
                device,
                allocator,
                u64::from(scene_capacity) * MOTION_BYTES,
            )?;
            let moving = SceneBuffer::with_bytes(device, allocator, u64::from(scene_capacity) * 4)?;
            if let Some(gpu) = self.motion_gpu.as_mut() {
                let old_motion = core::mem::replace(&mut gpu.motion, motion);
                let old_moving = core::mem::replace(&mut gpu.moving, moving);
                motion_grew = Some((old_motion, gpu.capacity));
                self.retired_motion.push((frame_number, old_moving));
                gpu.capacity = scene_capacity;
            } else {
                self.motion_gpu = Some(MotionGpu {
                    motion,
                    moving,
                    capacity: scene_capacity,
                    update: ComputeShader::new(device, bindless, MOTION, c"sceneMotion")?,
                    step: ComputeShader::new(device, bindless, STEP, c"sceneStep")?,
                    interpolate: ComputeShader::new(
                        device,
                        bindless,
                        INTERPOLATE,
                        c"sceneInterpolate",
                    )?,
                });
            }
        }
        let motion_buffers = self.motion_gpu.as_ref().map(|gpu| {
            let import = |fg: &mut FrameGraph<'g>, name, b: &SceneBuffer, bytes: u64| {
                fg.import_buffer(
                    name,
                    ImportedBuffer {
                        buffer: b.buffer,
                        size: bytes,
                        address: b.address,
                        initial: if motion_remade {
                            BufferState::idle()
                        } else {
                            motion_initial
                        },
                        final_state: Some(SCENE_ACCESS),
                    },
                )
            };
            let slots = u64::from(gpu.capacity);
            (
                import(fg, "scene motion", &gpu.motion, slots * MOTION_BYTES),
                import(fg, "scene moving", &gpu.moving, slots * 4),
                gpu.motion.address,
                gpu.moving.address,
            )
        });
        if let (Some((old, old_capacity)), Some((motion, ..))) = (motion_grew, motion_buffers) {
            let (old_plane, new_plane) =
                (u64::from(old_capacity) * 16, u64::from(scene_capacity) * 16);
            let old_buffer = fg.import_buffer(
                "scene motion (old)",
                ImportedBuffer {
                    buffer: old.buffer,
                    size: u64::from(old_capacity) * MOTION_BYTES,
                    address: old.address,
                    initial: motion_initial,
                    final_state: None,
                },
            );
            fg.add_pass("scene motion grow")
                .read(old_buffer, Access::TransferRead)
                .write(motion, Access::TransferWrite)
                .execute(move |ctx| {
                    // Each plane, then the words, at the new capacity's offsets.
                    let mut regions = [vk::BufferCopy::default(); MOTION_PLANES as usize + 1];
                    for (p, region) in (0u64..).zip(regions.iter_mut()) {
                        *region = vk::BufferCopy {
                            src_offset: p * old_plane,
                            dst_offset: p * new_plane,
                            size: if p < MOTION_PLANES {
                                old_plane
                            } else {
                                old_plane / 4
                            },
                        };
                    }
                    // SAFETY: recording outside a rendering scope; the new buffer is larger.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(old_buffer),
                            ctx.buffer(motion),
                            &regions,
                        );
                    }
                });
            self.retired_motion.push((frame_number, old));
        }
        if motion_buffers.is_some() {
            self.motion_state = SCENE_ACCESS;
        }
        self.state = SCENE_ACCESS;
        let count = self.updates.len() as u32;
        let upload_bytes = u64::from(count) * UPDATE_BYTES
            + full_data.map_or(0, |(_, bytes)| bytes)
            + room_units as u64 * 16
            + moving_staged.map_or(0, |_| self.moving.len() as u64 * 4);
        self.stats = SceneStats {
            instances: self.live,
            slots: self.high_water,
            batches: self.live_batches,
            uploaded,
            settled,
            upload_bytes,
            full,
            grew: needed_slots > capacity,
            moving: self.moving.len() as u32,
            motion_updates: (motion_updates + motion_steps) as u32,
        };
        let moving_count = self.moving.len() as u32;
        let alpha = self.alpha;
        let this: &'g Self = self;
        if let Some((staged, bytes)) = full_data {
            fg.add_pass("scene upload (full)")
                .write(current, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the buffer holds every
                    // slot.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            staged.buffer,
                            ctx.buffer(current),
                            &[vk::BufferCopy {
                                src_offset: staged.offset,
                                dst_offset: 0,
                                size: bytes,
                            }],
                        );
                    }
                });
        }
        if let Some(updates) = updates_staged {
            let data = full_data.map_or(room_staged.address, |(staged, _)| staged.address);
            let push = ScatterPush {
                instances: current_address,
                previous: previous_address,
                updates: updates.address,
                data,
                count,
                _pad: [0; 3],
            };
            fg.add_compute("scene upload", bindless)
                .read(current, Access::StorageReadWrite(ShaderStages::COMPUTE))
                .read(previous, Access::StorageReadWrite(ShaderStages::COMPUTE))
                .dispatch(&this.scatter, Dispatch::Threads([count, 1, 1]), move |_| {
                    Some(push)
                });
        }
        // Motion between ticks, after the frame's other writes: the ticks' transforms,
        // then the moving instances placed between them.
        if let (Some((motion, moving, motion_address, moving_address)), Some(gpu)) =
            (motion_buffers, this.motion_gpu.as_ref())
        {
            let rows = room_staged.address + 64 * scattered as u64;
            let step_rows = rows + 16 * (UPDATE_ROWS * motion_updates) as u64;
            let words = step_rows + 16 * (STEP_ROWS * motion_steps) as u64;
            let push = |words, rows, count| MotionPush {
                instances: current_address,
                previous: previous_address,
                motion: motion_address,
                second: motion_address + u64::from(gpu.capacity) * MOTION_PLANES * 16,
                words,
                rows,
                capacity: gpu.capacity,
                count,
                alpha,
                _pad: 0,
            };
            if motion_updates > 0 {
                let push = push(words, rows, motion_updates as u32);
                fg.add_compute("scene motion", bindless)
                    .read(motion, Access::StorageReadWrite(ShaderStages::COMPUTE))
                    .read(previous, Access::StorageReadWrite(ShaderStages::COMPUTE))
                    .dispatch(
                        &gpu.update,
                        Dispatch::Threads([push.count, 1, 1]),
                        move |_| Some(push),
                    );
            }
            if motion_steps > 0 {
                let push = push(
                    words + 4 * motion_updates as u64,
                    step_rows,
                    motion_steps as u32,
                );
                fg.add_compute("scene motion step", bindless)
                    .read(motion, Access::StorageReadWrite(ShaderStages::COMPUTE))
                    .dispatch(
                        &gpu.step,
                        Dispatch::Threads([push.count, 1, 1]),
                        move |_| Some(push),
                    );
            }
            if moving_count > 0 {
                if let Some(list) = moving_staged {
                    let bytes = u64::from(moving_count) * 4;
                    fg.add_pass("scene moving list")
                        .write(moving, Access::TransferWrite)
                        .execute(move |ctx| {
                            // SAFETY: recording outside a rendering scope; the list fits the
                            // buffer (one entry per slot at most).
                            unsafe {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    list.buffer,
                                    ctx.buffer(moving),
                                    &[vk::BufferCopy {
                                        src_offset: list.offset,
                                        dst_offset: 0,
                                        size: bytes,
                                    }],
                                );
                            }
                        });
                }
                let push = push(moving_address, 0, moving_count);
                fg.add_compute("scene interpolate", bindless)
                    .read(moving, Access::StorageRead(ShaderStages::COMPUTE))
                    .read(motion, Access::StorageRead(ShaderStages::COMPUTE))
                    .read(current, Access::StorageReadWrite(ShaderStages::COMPUTE))
                    .dispatch(
                        &gpu.interpolate,
                        Dispatch::Threads([moving_count, 1, 1]),
                        move |_| Some(push),
                    );
            }
        }
        Ok(Some(SceneFrame {
            scene: this,
            current,
            previous,
            current_address,
            previous_address,
        }))
    }

    fn collect_retired(&mut self, allocator: &mut Allocator, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut k = 0;
        while k < self.retired_motion.len() {
            if self.retired_motion[k].0 > completed {
                k += 1;
                continue;
            }
            let (_, mut old) = self.retired_motion.swap_remove(k);
            // SAFETY: the frames that used it have completed.
            unsafe { old.destroy(allocator) };
        }
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, mut old) = self.retired.swap_remove(i);
            // SAFETY: the frames that used them have completed.
            unsafe {
                old.current.destroy(allocator);
                old.previous.destroy(allocator);
            }
        }
    }

    /// Frees the buffers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        for (_, mut old) in self.retired.drain(..) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                old.current.destroy(allocator);
                old.previous.destroy(allocator);
            }
        }
        if let Some(mut buffers) = self.buffers.take() {
            // SAFETY: guaranteed by the caller.
            unsafe {
                buffers.current.destroy(allocator);
                buffers.previous.destroy(allocator);
            }
        }
        for (_, mut old) in self.retired_motion.drain(..) {
            // SAFETY: guaranteed by the caller.
            unsafe { old.destroy(allocator) };
        }
        if let Some(mut gpu) = self.motion_gpu.take() {
            // SAFETY: guaranteed by the caller.
            unsafe {
                gpu.motion.destroy(allocator);
                gpu.moving.destroy(allocator);
            }
        }
    }
}

/// The scene in one frame's graph, for the views that draw it
/// ([`crate::forward::ForwardRenderer::add_scene_passes`]).
#[derive(Debug, Clone, Copy)]
pub struct SceneFrame<'g> {
    pub(crate) scene: &'g GpuScene,
    /// This frame's instances (64 bytes per slot).
    pub current: Buffer,
    /// Last frame's instances, for motion vectors.
    pub previous: Buffer,
    pub(crate) current_address: vk::DeviceAddress,
    pub(crate) previous_address: vk::DeviceAddress,
}

impl SceneFrame<'_> {
    /// Slots culling visits: live instances and holes up to the highest live slot.
    pub fn slot_count(&self) -> u32 {
        self.scene.high_water
    }

    /// Holes below the highest live slot.
    pub(crate) fn holes(&self) -> u32 {
        self.scene.high_water - self.scene.live
    }

    /// Live instances.
    pub fn instances(&self) -> u32 {
        self.scene.live
    }

    /// What this frame's upload did ([`GpuScene::stats`]).
    pub fn stats(&self) -> SceneStats {
        self.scene.stats
    }

    /// World-space bounds of the live instances.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        self.scene.bounds
    }

    /// The scene's batches by id (id 0 is the sink; dead ones have `alive == false`).
    pub(crate) fn batches(&self) -> &[Batch] {
        &self.scene.batches
    }

    /// The upload this frame's data comes from: consecutive frames that uploaded have
    /// consecutive serials.
    pub(crate) fn serial(&self) -> u64 {
        self.scene.serial
    }

    /// World boxes that changed in this frame's upload (see `GpuScene::changes`).
    pub(crate) fn changes(&self) -> &[[Vec3; 2]] {
        &self.scene.changes
    }

    /// The boxes of the instances moving between ticks over their whole move
    /// ([`GpuScene::set_motion`]): they move in every frame, whatever was uploaded.
    pub fn moving_boxes(&self) -> &[[Vec3; 2]] {
        &self.scene.moving_boxes
    }

    /// Where batch `id` is, for ordering batches by distance.
    pub(crate) fn batch_center(&self, id: u32) -> Vec3 {
        self.scene.batches[id as usize].center(self.scene)
    }
}
