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
//!   [`GpuScene::add_passes`]: one 16-byte update per touched slot plus 64 bytes of
//!   instance data go through the [`UploadRing`], and a compute pass (`scene.slang`)
//!   scatters them, copying the old values into last frame's buffer on the way. A slot
//!   that moved last frame and not this one gets a 16-byte update that settles its
//!   previous transform. A frame where nothing changed uploads nothing and records no
//!   pass.
//! - **Batches.** Instances that share a mesh and a pipeline state form a batch (a
//!   blended instance is a batch of its own, as in the draw-list path). Membership is
//!   kept incrementally: a batch is created or freed only when the set of
//!   (mesh, state) pairs in the scene changes. The forward renderer lays the batches out
//!   into indirect commands every frame, at a cost that follows the number of batches,
//!   not of instances ([`crate::forward::ForwardRenderer::add_scene_passes`]).
//! - **Holes.** Removed instances leave holes below the highest slot in use; they are
//!   parked in a *sink* batch (id 0) that culling may append to but nothing draws.
//!
//! Materials and meshes are resolved when a change is uploaded: a material's slot and
//! kind, the mesh's bounding box. Editing or removing a material re-resolves every
//! instance once ([`crate::material::MaterialTable`] counts such edits); a removed mesh
//! draws nothing, as in the draw-list path.

use crate::compute::{ComputeShader, Dispatch};
use crate::cull::transform_box;
use crate::forward::{BLENDED, InstanceData, mirrored, state};
use crate::graph::{Access, Buffer, BufferState, FrameGraph, ImportedBuffer, ShaderStages};
use crate::material::{MaterialFrame, MaterialId};
use crate::mesh::{GeometryFrame, MeshId};
use crate::upload::UploadRing;
use core::cmp::Reverse;
use core::hash::{BuildHasherDefault, Hasher};
use engine_core::math::{Mat4, Vec3, Vec4};
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

/// What the scene's buffers are left in after a frame: read (and, by the scatter pass,
/// written) by compute and vertex shaders.
const SCENE_ACCESS: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::from_raw(
        vk::PipelineStageFlags2::COMPUTE_SHADER.as_raw()
            | vk::PipelineStageFlags2::VERTEX_SHADER.as_raw()
            | vk::PipelineStageFlags2::COPY.as_raw(),
    ),
    access: vk::AccessFlags2::from_raw(
        vk::AccessFlags2::SHADER_STORAGE_READ.as_raw()
            | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw()
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
pub struct Instance {
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
    /// Pipeline state bits (`forward.rs`), subsurface included.
    state: u8,
    mesh: MeshId,
    material: MaterialId,
    batch: u32,
    /// Upload (frame serial) that last wrote it.
    written: u64,
    /// World-space box: center, and its corners.
    center: Vec3,
    min: Vec3,
    max: Vec3,
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
    pub(crate) fn center(&self, slots: &GpuScene) -> Vec3 {
        if self.state as usize & BLENDED != 0 {
            return slots.slots[self.blended_slot as usize].center;
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
    /// Slots whose instance data was uploaded (added, moved, edited, removed).
    pub uploaded: u32,
    /// Slots whose previous transform was settled (moved last frame, still now).
    pub settled: u32,
    /// Bytes written into the upload ring.
    pub upload_bytes: u64,
    /// Whether the buffers grew this frame.
    pub grew: bool,
}

/// The persistent GPU scene (see the module documentation).
pub struct GpuScene {
    slots: Vec<Slot>,
    /// The instance data the GPU holds once the pending changes are uploaded.
    data: Vec<InstanceData>,
    /// Free slots below `slots.len()`, lowest first.
    free: BinaryHeap<Reverse<u32>>,
    /// Slots below this are live or holes; culling visits them.
    high_water: u32,
    live: u32,
    /// Slots with pending changes, each once.
    dirty: Vec<u32>,
    /// Slots moved by the last upload and by this one (to settle in the next).
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
    /// Upload scratch.
    updates: Vec<Update>,
    uploads: Vec<InstanceData>,
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

fn same_rows(a: &[[f32; 4]; 3], b: &[[f32; 4]; 3]) -> bool {
    a.iter()
        .flatten()
        .zip(b.iter().flatten())
        .all(|(x, y)| x.to_bits() == y.to_bits())
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
        let capacity = capacity.max(1) as usize;
        let mut scene = Self {
            slots: Vec::with_capacity(capacity),
            data: Vec::with_capacity(capacity),
            free: BinaryHeap::new(),
            high_water: 0,
            live: 0,
            dirty: Vec::with_capacity(capacity),
            written_last: Vec::with_capacity(capacity),
            written: Vec::with_capacity(capacity),
            serial: 0,
            batches: Vec::new(),
            batch_keys: HashMap::default(),
            free_batches: Vec::new(),
            live_batches: 0,
            bounds: (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
            bounds_stale: false,
            material_revision: None,
            updates: Vec::with_capacity(capacity),
            uploads: Vec::with_capacity(capacity),
            buffers: None,
            initial_capacity: 0,
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
        scene.initial_capacity = capacity as u32;
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
    pub fn insert(&mut self, instance: Instance) -> InstanceId {
        let index = match self.free.pop() {
            Some(Reverse(index)) => index,
            None => {
                let index = self.slots.len() as u32;
                self.slots.push(Slot {
                    generation: 0,
                    alive: false,
                    flags: 0,
                    state: 0,
                    mesh: instance.mesh,
                    material: instance.material,
                    batch: SINK,
                    written: 0,
                    center: Vec3::ZERO,
                    min: Vec3::ZERO,
                    max: Vec3::ZERO,
                });
                self.data.push(HOLE);
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
        if self.slot_mut(id).is_none() {
            return false;
        }
        let rows = rows_of(world_from_object);
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
    pub fn get(&self, id: InstanceId) -> Option<Instance> {
        let slot = self
            .slots
            .get(id.index as usize)
            .filter(|s| s.alive && s.generation == id.generation)?;
        let r = self.data[id.index as usize].rows;
        Some(Instance {
            mesh: slot.mesh,
            material: slot.material,
            world_from_object: Mat4::from_cols(
                Vec4::new(r[0][0], r[1][0], r[2][0], 0.0),
                Vec4::new(r[0][1], r[1][1], r[2][1], 0.0),
                Vec4::new(r[0][2], r[1][2], r[2][2], 0.0),
                Vec4::new(r[0][3], r[1][3], r[2][3], 1.0),
            ),
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

    /// Whether a box lies on the bounds (so moving it inward or removing it can shrink
    /// them).
    fn on_boundary(&self, min: Vec3, max: Vec3) -> bool {
        let (lo, hi) = self.bounds;
        min.cmple(lo).any() || max.cmpge(hi).any()
    }

    /// Applies a slot's pending changes to the CPU state and records its upload.
    fn apply(&mut self, index: u32, geometry: &GeometryFrame<'_>, materials: &MaterialFrame<'_>) {
        let slot = self.slots[index as usize];
        let flags = slot.flags;
        self.slots[index as usize].flags = 0;
        let had_box = flags & flag::NEW == 0;
        // A removed slot is freed only after this upload, so it cannot be live again yet.
        if flags & flag::REMOVED != 0 {
            if had_box && self.on_boundary(slot.min, slot.max) {
                self.bounds_stale = true;
            }
            self.leave(index);
            self.data[index as usize] = HOLE;
            self.uploads.push(HOLE);
            self.updates.push(Update {
                slot: index,
                current: self.uploads.len() as u32 - 1,
                previous: KEEP,
                _pad: 0,
            });
            self.mark_written(index, false);
            return;
        }
        self.apply_live(index, flags, had_box, geometry, materials);
    }

    fn apply_live(
        &mut self,
        index: u32,
        flags: u8,
        had_box: bool,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
    ) {
        let slot = self.slots[index as usize];
        let rows = self.data[index as usize].rows;
        let world_from_object = Mat4::from_cols(
            Vec4::new(rows[0][0], rows[1][0], rows[2][0], 0.0),
            Vec4::new(rows[0][1], rows[1][1], rows[2][1], 0.0),
            Vec4::new(rows[0][2], rows[1][2], rows[2][2], 0.0),
            Vec4::new(rows[0][3], rows[1][3], rows[2][3], 1.0),
        );
        let (material_slot, kind) = materials.resolve(slot.material);
        let state_bits = state(kind, mirrored(&world_from_object), true) as u8;
        // The world box, as the draw-list path computes it.
        let (local_center, local_extents) = geometry
            .mesh(slot.mesh)
            .map_or((Vec3::ZERO, Vec3::ZERO), |m| {
                ((m.min + m.max) * 0.5, (m.max - m.min) * 0.5)
            });
        let (center, extents) = transform_box(&world_from_object, local_center, local_extents);
        let (min, max) = (center - extents, center + extents);
        if had_box && self.on_boundary(slot.min, slot.max) {
            self.bounds_stale = true;
        }
        self.bounds = (self.bounds.0.min(min), self.bounds.1.max(max));
        // Batch membership: kept unless the mesh or the state changed.
        let current = self.batches[slot.batch as usize];
        let keep = slot.batch != SINK
            && current.alive
            && current.mesh == slot.mesh
            && current.state == state_bits;
        if keep {
            let batch = &mut self.batches[slot.batch as usize];
            for k in 0..3 {
                batch.sum[k] += f64::from(center[k]) - f64::from(slot.center[k]);
            }
        } else {
            self.leave(index);
            let id = self.batch_for(index, slot.mesh, state_bits);
            let batch = &mut self.batches[id as usize];
            batch.count += 1;
            for k in 0..3 {
                batch.sum[k] += f64::from(center[k]);
            }
            self.slots[index as usize].batch = id;
        }
        let s = &mut self.slots[index as usize];
        s.state = state_bits;
        s.center = center;
        s.min = min;
        s.max = max;
        let data = InstanceData {
            rows,
            material: material_slot,
            batch: s.batch,
            _pad: [0; 2],
        };
        self.data[index as usize] = data;
        self.uploads.push(data);
        let at = self.uploads.len() as u32 - 1;
        let still = flags & (flag::NEW | flag::TELEPORT) != 0;
        self.updates.push(Update {
            slot: index,
            current: at,
            previous: if still { at } else { COPY },
            _pad: 0,
        });
        self.mark_written(index, !still);
    }

    /// Records that this upload writes `index`; `moved`: its previous transform differs
    /// from the new one, so the next upload settles it unless it moves again.
    fn mark_written(&mut self, index: u32, moved: bool) {
        let slot = &mut self.slots[index as usize];
        slot.written = self.serial;
        if moved {
            self.written.push(index);
        }
    }

    /// Applies the changes since the last call and declares their upload in `fg`: a
    /// compute pass that scatters the changed slots into the scene's buffers (none when
    /// nothing changed), after growing them if the scene outgrew them. Call it once per
    /// frame, after the frame's changes and before the views draw the scene (with the
    /// returned [`SceneFrame`]). Returns `None`, uploading nothing, when `ring` is full.
    ///
    /// Allocates only when the scene grows (or a new batch needs room in its tables).
    #[allow(clippy::too_many_arguments)]
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
        self.serial += 1;
        core::mem::swap(&mut self.written, &mut self.written_last);
        self.written.clear();
        self.updates.clear();
        self.uploads.clear();
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
        let mut uploaded = 0;
        for k in 0..self.dirty.len() {
            let index = self.dirty[k];
            self.apply(index, geometry, materials);
            uploaded += 1;
        }
        // Slots that moved last frame and not now: their previous transform catches up.
        let mut settled = 0;
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
        // Removed slots become free now that their holes are written; culling stops at
        // the highest slot in use.
        for k in 0..self.dirty.len() {
            let index = self.dirty[k];
            if !self.slots[index as usize].alive {
                self.free.push(Reverse(index));
            }
        }
        self.dirty.clear();
        self.high_water = self.slots.len() as u32;
        while self.high_water > 0 && !self.slots[self.high_water as usize - 1].alive {
            self.high_water -= 1;
        }
        if self.bounds_stale {
            self.bounds_stale = false;
            let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for slot in &self.slots[..self.high_water as usize] {
                if slot.alive {
                    lo = lo.min(slot.min);
                    hi = hi.max(slot.max);
                }
            }
            self.bounds = (lo, hi);
        }

        // The buffers: grown to hold every slot (the holes above the highest live slot
        // too: their hole data may still be in flight).
        let needed = (self.slots.len() as u32).max(1);
        let mut grew = None;
        let capacity = self.buffers.as_ref().map_or(0, |b| b.capacity);
        if needed > capacity {
            let new_capacity = needed
                .max(capacity.saturating_mul(2))
                .max(self.initial_capacity);
            let fresh = Buffers {
                current: SceneBuffer::new(device, allocator, new_capacity)?,
                previous: SceneBuffer::new(device, allocator, new_capacity)?,
                capacity: new_capacity,
            };
            grew = self.buffers.replace(fresh);
        }
        let Some(buffers) = self.buffers.as_ref() else {
            return Ok(None);
        };
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
            let old_current = import_old(fg, "scene instances (old)", &old.current);
            let old_previous = import_old(fg, "scene previous instances (old)", &old.previous);
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
        let mut push = None;
        if count > 0 {
            let Some(updates) = ring.stage(&self.updates) else {
                return Ok(None);
            };
            let data = if self.uploads.is_empty() {
                0
            } else {
                let Some(data) = ring.push(&self.uploads) else {
                    return Ok(None);
                };
                data
            };
            push = Some(ScatterPush {
                instances: current_address,
                previous: previous_address,
                updates: updates.address,
                data,
                count,
                _pad: [0; 3],
            });
        }
        self.stats = SceneStats {
            instances: self.live,
            slots: self.high_water,
            batches: self.live_batches,
            uploaded,
            settled,
            upload_bytes: u64::from(count) * size_of::<Update>() as u64
                + self.uploads.len() as u64 * INSTANCE_BYTES,
            grew: needed > capacity,
        };
        let this: &'g Self = self;
        if let Some(push) = push {
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
