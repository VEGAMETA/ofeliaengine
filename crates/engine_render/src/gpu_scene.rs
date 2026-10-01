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
//!   this frame's instances and last frame's (for motion vectors).
//! - **Uploads.** Changes are recorded on the CPU ([`GpuScene::set_transform`] ignores a
//!   transform equal to the current one) and applied once per frame by
//!   [`GpuScene::add_passes`]. Usually one 16-byte update per touched slot plus 64 bytes
//!   of instance data go through the [`UploadRing`], and a compute pass (`scene.slang`)
//!   scatters them, copying the old values into last frame's buffer on the way; a slot
//!   that moved last frame and not this one gets a 16-byte update that settles its
//!   previous transform. When at least half of the slots changed, the whole array is
//!   copied instead and the two buffers swap roles (this frame's becomes last frame's
//!   without a copy), which moves no more memory than a draw list does. A frame where
//!   nothing changed uploads nothing and records no pass.
//! - **Batches.** Instances that share a mesh and a pipeline state form a batch (a
//!   blended instance is a batch of its own, as in the draw-list path). Membership is
//!   kept incrementally: a batch is created or freed only when the set of
//!   (mesh, state) pairs in the scene changes. The forward renderer lays the batches out
//!   into indirect commands every frame, at a cost that follows the number of batches,
//!   not of instances ([`crate::forward::ForwardRenderer::add_scene_passes`]).
//! - **Holes.** Removed instances leave holes below the highest slot in use; they are
//!   parked in a *sink* batch (id 0) that culling may append to but nothing draws.
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

/// `KEEP` and `COPY` in `scene.slang`.
const KEEP: u32 = u32::MAX;
const COPY: u32 = u32::MAX - 1;

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
    /// Removed: becomes a hole, then free.
    pub(super) const REMOVED: u8 = 32;
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

/// One of the scene's device buffers.
#[derive(Debug)]
struct SceneBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
}

impl SceneBuffer {
    fn new(device: &Device, allocator: &mut Allocator, slots: u32) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(u64::from(slots.max(1)) * INSTANCE_BYTES)
                .usage(
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
    buffers: Option<Buffers>,
    /// Slots the first buffers hold.
    initial_capacity: u32,
    retired: Vec<(u64, Buffers)>,
    frames_in_flight: u64,
    /// The state the graph left the buffers in.
    state: BufferState,
    scatter: ComputeShader<ScatterPush>,
    stats: SceneStats,
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
const HOLE: InstanceData = InstanceData {
    rows: [[0.0; 4]; 3],
    material: 0,
    batch: SINK,
    _pad: [0; 2],
};

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

/// Where this upload's instance data goes: straight into the ring (scatter), or into the
/// CPU array only (full copy afterwards).
enum Out<'a> {
    Ring(&'a mut [MaybeUninit<InstanceData>], usize),
    Full,
}

/// What a scattered slot's previous transform becomes.
#[derive(Debug, Clone, Copy)]
enum Previous {
    /// Unchanged (holes).
    Keep,
    /// The slot's current value (it moved).
    Copy,
    /// The new value too (added, teleported).
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
            buffers: None,
            initial_capacity: capacity.max(1),
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            state: BufferState::idle(),
            scatter,
            stats: SceneStats::default(),
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
                });
                self.data.push(HOLE);
                self.boxes.push(NO_BOX);
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

    /// Removes an instance (stale ids are ignored); its slot is reused after the next
    /// upload. Returns whether it was live.
    pub fn remove(&mut self, id: InstanceId) -> bool {
        let Some(slot) = self.slot_mut(id) else {
            return false;
        };
        slot.alive = false;
        slot.generation = slot.generation.wrapping_add(1);
        self.live -= 1;
        self.queue(id.index, flag::REMOVED);
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
        let m = world_from_object.matrix3;
        let t = world_from_object.translation;
        self.set_rows(
            id,
            [
                [m.x_axis.x, m.y_axis.x, m.z_axis.x, t.x],
                [m.x_axis.y, m.y_axis.y, m.z_axis.y, t.y],
                [m.x_axis.z, m.y_axis.z, m.z_axis.z, t.z],
            ],
        )
    }

    fn set_rows(&mut self, id: InstanceId, rows: [[f32; 4]; 3]) -> bool {
        if self.slot_mut(id).is_none() {
            return false;
        }
        let data = &mut self.data[id.index as usize];
        if same_rows(&data.rows, &rows) {
            return false;
        }
        data.rows = rows;
        self.queue(id.index, flag::MOVED);
        true
    }

    /// Moves an instance without motion in this frame (a teleport, a respawn): its
    /// previous transform is the new one too.
    pub fn teleport(&mut self, id: InstanceId, world_from_object: &Mat4) {
        if self.slot_mut(id).is_none() {
            return;
        }
        self.data[id.index as usize].rows = rows_of(world_from_object);
        self.queue(id.index, flag::MOVED | flag::TELEPORT);
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

    /// Applies a slot's pending changes to the CPU state; in scatter mode also writes its
    /// update and data into the ring.
    fn apply(
        &mut self,
        index: u32,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
        out: &mut Out<'_>,
        incremental_bounds: bool,
    ) {
        let flags = self.slots[index as usize].flags;
        self.slots[index as usize].flags = 0;
        // A removed slot is freed only after this upload, so it cannot be live again yet.
        if flags & flag::REMOVED != 0 {
            let old = self.boxes.get(index as usize);
            if incremental_bounds && flags & flag::NEW == 0 && self.on_boundary(old) {
                self.bounds_stale = true;
            }
            self.boxes.set(index as usize, NO_BOX);
            self.leave(index);
            self.data[index as usize] = HOLE;
            self.write(index, HOLE, Previous::Keep, out);
            self.slots[index as usize].written = self.serial;
            // Free once its hole is written (nothing inserts during the upload).
            self.free.push(Reverse(index));
            return;
        }
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
        // The world box, as the draw-list path computes it.
        let (center, extents) =
            transform_box(&world_from_object, slot.local_center, slot.local_extents);
        let new_box = [center - extents, center + extents];
        if incremental_bounds {
            if flags & flag::NEW == 0 && self.on_boundary(self.boxes.get(i)) {
                self.bounds_stale = true;
            }
            self.bounds = (self.bounds.0.min(new_box[0]), self.bounds.1.max(new_box[1]));
        }
        self.boxes.set(i, new_box);
        // Batch membership: kept unless the mesh or the state changed.
        let batch = &mut self.batches[batch_id as usize];
        if batch_id != SINK && batch.mesh == mesh && batch.state == state_bits {
            for k in 0..3 {
                batch.sum[k] += f64::from(center[k] - old_center[k]);
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
        s.center = center;
        s.written = self.serial;
        let d = &mut self.data[i];
        d.material = s.material_slot;
        d.batch = s.batch;
        let data = *d;
        let still = flags & (flag::NEW | flag::TELEPORT) != 0;
        if !still {
            // Its previous transform differs now: settled by the next upload unless it
            // moves again.
            self.written.push(index);
        }
        if !matches!(out, Out::Full) {
            let previous = if still {
                Previous::Same
            } else {
                Previous::Copy
            };
            self.write(index, data, previous, out);
        } else if still {
            // The full copy writes every slot; only new and teleported slots need their
            // previous transform set, from the copied array.
            self.updates.push(Update {
                slot: index,
                current: KEEP,
                previous: index,
                _pad: 0,
            });
        }
    }

    /// Scatter mode: `data` into the ring for slot `index`, and its update.
    fn write(&mut self, index: u32, data: InstanceData, previous: Previous, out: &mut Out<'_>) {
        let Out::Ring(ring, next) = out else {
            return;
        };
        let at = *next as u32;
        ring[*next].write(data);
        *next += 1;
        self.updates.push(Update {
            slot: index,
            current: at,
            previous: match previous {
                Previous::Keep => KEEP,
                Previous::Copy => COPY,
                Previous::Same => at,
            },
            _pad: 0,
        });
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
        let full = slot_total >= FULL_MIN_SLOTS && 2 * touched >= slot_total;
        let needed = if full {
            slot_total as u64 * INSTANCE_BYTES + touched as u64 * UPDATE_BYTES
        } else {
            touched as u64 * INSTANCE_BYTES
                + (touched + self.written_last.len()) as u64 * UPDATE_BYTES
        };
        if needed + 64 > ring.available() {
            return Ok(None);
        }
        // The scattered data's room, taken before anything changes.
        let mut scattered = if !full && touched > 0 {
            match ring.alloc::<InstanceData>(touched) {
                Some(room) => Some(room),
                None => return Ok(None),
            }
        } else {
            None
        };
        self.serial += 1;
        core::mem::swap(&mut self.written, &mut self.written_last);
        self.written.clear();
        self.updates.clear();
        // Bounds: kept incrementally for a few changes, recomputed for many.
        let incremental_bounds = 8 * touched < slot_total;
        if !incremental_bounds {
            self.bounds_stale = true;
        }
        let mut data_staged = None;
        if let Some((slice, staged)) = scattered.take() {
            let mut out = Out::Ring(slice, 0);
            for k in 0..touched {
                let index = self.dirty[k];
                self.apply(index, geometry, materials, &mut out, incremental_bounds);
            }
            data_staged = Some(staged);
        } else {
            for k in 0..touched {
                let index = self.dirty[k];
                self.apply(
                    index,
                    geometry,
                    materials,
                    &mut Out::Full,
                    incremental_bounds,
                );
            }
        }
        let uploaded = touched as u32;
        // Slots that moved last frame and not now: their previous transform catches up.
        // (After a full copy the buffers swapped: last frame's current values are this
        // frame's previous ones, and every slot not moved now holds the same in both.)
        let mut settled = 0;
        if !full {
            for k in 0..self.written_last.len() {
                let index = self.written_last[k];
                let slot = self.slots[index as usize];
                if slot.alive && slot.written != self.serial {
                    self.updates.push(Update {
                        slot: index,
                        current: KEEP,
                        previous: COPY,
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

        // The buffers: grown to hold every slot (the holes above the highest live slot
        // too: their hole data may still be in flight).
        let needed_slots = (slot_total as u32).max(1);
        let capacity = self.buffers.as_ref().map_or(0, |b| b.capacity);
        let mut grew = None;
        if needed_slots > capacity {
            let new_capacity = needed_slots
                .max(capacity.saturating_mul(2))
                .max(self.initial_capacity);
            let fresh = Buffers {
                current: SceneBuffer::new(device, allocator, new_capacity)?,
                previous: SceneBuffer::new(device, allocator, new_capacity)?,
                capacity: new_capacity,
            };
            grew = self.buffers.replace(fresh);
        }
        let Some(buffers) = self.buffers.as_mut() else {
            return Ok(None);
        };
        if full {
            // This frame's values go to the buffer that held last frame's previous ones;
            // last frame's current ones become this frame's previous ones.
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
            let state = self.state;
            let import_old = |fg: &mut FrameGraph<'g>, name, b: &SceneBuffer| {
                fg.import_buffer(
                    name,
                    ImportedBuffer {
                        buffer: b.buffer,
                        size: bytes,
                        address: b.address,
                        initial: state,
                        final_state: None,
                    },
                )
            };
            // A full copy rewrites every slot of this frame's buffer: only last frame's
            // values must carry over, into what is now the previous buffer.
            let (old_current, old_previous) = if full {
                (&old.previous, &old.current)
            } else {
                (&old.current, &old.previous)
            };
            let old_current = import_old(fg, "scene instances (old)", old_current);
            let old_previous = import_old(fg, "scene previous instances (old)", old_previous);
            fg.add_pass("scene grow")
                .read(old_current, Access::TransferRead)
                .read(old_previous, Access::TransferRead)
                .write(current, Access::TransferWrite)
                .write(previous, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = [vk::BufferCopy {
                        src_offset: 0,
                        dst_offset: 0,
                        size: bytes,
                    }];
                    // SAFETY: recording outside a rendering scope; the new buffers are
                    // larger than the old ones.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(old_current),
                            ctx.buffer(current),
                            &region,
                        );
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(old_previous),
                            ctx.buffer(previous),
                            &region,
                        );
                    }
                });
            self.retired.push((frame_number, old));
        }
        self.state = SCENE_ACCESS;
        let count = self.updates.len() as u32;
        let upload_bytes = u64::from(count) * UPDATE_BYTES
            + full_data.map_or(0, |(_, bytes)| bytes)
            + data_staged.map_or(0, |_| uploaded as u64 * INSTANCE_BYTES);
        self.stats = SceneStats {
            instances: self.live,
            slots: self.high_water,
            batches: self.live_batches,
            uploaded,
            settled,
            upload_bytes,
            full,
            grew: needed_slots > capacity,
        };
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
            let data = match (full_data, data_staged) {
                (Some((staged, _)), _) | (None, Some(staged)) => staged.address,
                (None, None) => 0,
            };
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

    /// Where batch `id` is, for ordering batches by distance.
    pub(crate) fn batch_center(&self, id: u32) -> Vec3 {
        self.scene.batches[id as usize].center(self.scene)
    }
}
