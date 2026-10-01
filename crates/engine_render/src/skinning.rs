//! Skinned meshes on the GPU (ADR 0068): pre-skinning by a compute pass into the geometry
//! pool, so every pass draws skinned meshes as ordinary meshes.
//!
//! - **A skin** ([`Skinning::add_skin`]) is a mesh's bind pose with its joint influences
//!   (4 per vertex, or 8 in two sets), stored once in a device buffer shared by all skins.
//! - **A skinned instance** ([`Skinning::add_instance`]) gets a mesh of its own in the
//!   [`GeometryPool`] (the bind pose's indices and texture coordinates; no detail
//!   levels) and draws through it like any mesh: in a draw list or a
//!   [`crate::gpu_scene::GpuScene`]. Every frame its skinning matrices
//!   ([`Skinning::set_pose`], from `engine_anim`) go through the upload ring, and
//!   [`Skinning::begin_frame`] records one dispatch per skin (its instances along y) that
//!   blends the bind pose (linear blend skinning) and writes positions, normals and
//!   tangents into the instance's mesh. The depth prepass, the shadow cascades and forward
//!   shading read them like any other vertices; their shaders are unchanged.
//! - **Motion vectors.** Before overwriting a position, the skinning pass copies the old
//!   one (last frame's skinned position) into the instance's range of a *previous
//!   positions* buffer. The forward pass writes each pixel's motion from the instance's
//!   rigid motion only; [`SkinningFrame::add_motion_pass`] then draws the skinned
//!   instances once more with depth test *equal* (placement bit-identical to the forward
//!   shaders') and overwrites their pixels' motion with each vertex's own. Views without
//!   motion vectors, and frames without skinned meshes, pay nothing.
//! - **Bounds.** At [`Skinning::add_skin`] each joint gets the bind-pose box of the
//!   vertices it influences; [`Skinning::set_pose`] transforms those boxes by the joints'
//!   matrices and takes their union, which contains every skinned vertex (a skinned
//!   vertex is a convex combination of its joints' transforms of it).
//!   [`Skinning::update_bounds`] gives the boxes to the pool (GPU culling) and the scene
//!   (shadow fit, batch order).
//!
//! Per frame: [`Skinning::set_pose`] for each animated instance, [`Skinning::update_bounds`]
//! before [`GeometryPool::begin_frame`], [`Skinning::begin_frame`] after it and before the
//! views, [`SkinningFrame::add_motion_pass`] after each view's forward pass that has
//! motion vectors. An instance that is not posed in a frame keeps its last pose (and is
//! skinned again, with no motion); one never posed shows the bind pose.

use crate::compute::{ComputeShader, Dispatch};
use crate::forward::MotionOutput;
use crate::gpu_scene::{GpuScene, InstanceId};
use crate::graph::{
    Access, Buffer, BufferState, FrameGraph, ImportedBuffer, LoadOp, ShaderStages, StoreOp, Texture,
};
use crate::mesh::{GeometryFrame, GeometryPool, MeshData, MeshError, MeshId, OwnedMeshData};
use crate::mesh::{encode_octahedral, encode_tangent};
use crate::upload::UploadRing;
use engine_core::math::{Mat4, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Device, GraphicsPipelineDesc, MemoryLocation, Pipeline,
    PipelineDesc, Range, RangeAllocator, RhiError, ShaderStageDesc, spirv_words,
};

const SKIN4: &[u8] = include_bytes!("../shaders/skinning.skin4.spv");
const SKIN8: &[u8] = include_bytes!("../shaders/skinning.skin8.spv");
const MOTION_VERTEX: &[u8] = include_bytes!("../shaders/skinned_motion.skinnedMotionVertex.spv");
const MOTION_FRAGMENT: &[u8] =
    include_bytes!("../shaders/skinned_motion.skinnedMotionFragment.spv");

/// `SkinPush` in skinning.slang.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct SkinPush {
    source: u64,
    instances: u64,
    positions: u64,
    attributes: u64,
    vertex_count: u32,
    stride: u32,
    instance_count: u32,
    _pad: u32,
}

/// `SkinInstance` in skinning.slang.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct SkinInstance {
    palette: u64,
    previous: u64,
    first_vertex: u32,
    flags: u32,
    _pad: [u32; 2],
}

/// `MotionView` in skinned_motion.slang.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct MotionView {
    clip_from_world: [f32; 16],
    previous_clip_from_world: [f32; 16],
    jitter: [f32; 4],
    size: [u32; 2],
    _pad: [u32; 2],
}

/// `MotionInstance` in skinned_motion.slang.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct MotionInstance {
    rows: [[f32; 4]; 3],
    previous_rows: [[f32; 4]; 3],
    previous: u64,
    first_vertex: u32,
    _pad: u32,
}

/// `MotionPush` in skinned_motion.slang.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct MotionPush {
    view: u64,
    instances: u64,
    positions: u64,
}

const _: () = assert!(size_of::<SkinPush>() == 48);
const _: () = assert!(size_of::<SkinInstance>() == 32);
const _: () = assert!(size_of::<MotionView>() == 160);
const _: () = assert!(size_of::<MotionInstance>() == 112);
const _: () = assert!(size_of::<MotionPush>() == 24);

/// `RESET` in skinning.slang.
const RESET: u32 = 1;

/// Bytes of one previous position.
const PREVIOUS_BYTES: u64 = 12;

/// A mesh's bind pose and joint influences, for [`Skinning::add_skin`].
#[derive(Debug, Clone, Copy)]
pub struct SkinnedMeshData<'a> {
    /// The bind pose, as for [`GeometryPool::add_mesh`].
    pub mesh: MeshData<'a>,
    /// Joint indices (into the skin's skinning matrices), four per set; one set per vertex
    /// (four influences) or two (eight): `vertices × sets` entries, a vertex's sets
    /// consecutive.
    pub joints: &'a [[u16; 4]],
    /// Weights of `joints` (normalized here per vertex).
    pub weights: &'a [[f32; 4]],
    /// Skinning matrices the skin uses (one past its highest joint index).
    pub joint_count: u16,
}

/// Why a skin or a skinned instance was not added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkinError {
    /// The mesh is invalid, or the pool cannot hold it.
    Mesh(MeshError),
    /// Influences are not one or two sets per vertex, or joints and weights differ in
    /// length.
    Influences,
    /// A joint index at or past `joint_count`.
    JointOutOfRange(u16),
    /// The skin was removed.
    Stale,
}

impl core::fmt::Display for SkinError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mesh(e) => write!(f, "{e}"),
            Self::Influences => write!(f, "joint influences are not 4 or 8 per vertex"),
            Self::JointOutOfRange(j) => write!(f, "joint {j} past the skin's joints"),
            Self::Stale => write!(f, "the skin was removed"),
        }
    }
}

impl std::error::Error for SkinError {}

/// A skin (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SkinId {
    index: u32,
    generation: u32,
}

/// A skinned instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SkinnedId {
    index: u32,
    generation: u32,
}

/// A device buffer suballocated by a [`RangeAllocator`], with its graph state.
#[derive(Debug)]
struct Pooled {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    size: u64,
    state: BufferState,
}

impl Pooled {
    fn new(allocator: &mut Allocator, device: &Device, size: u64) -> Result<Self, RhiError> {
        let size = size.max(16);
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::TRANSFER_DST,
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
            size,
            state: BufferState::idle(),
        })
    }

    fn import(&mut self, fg: &mut FrameGraph<'_>, name: &'static str, last: BufferState) -> Buffer {
        let handle = fg.import_buffer(
            name,
            ImportedBuffer {
                buffer: self.buffer,
                size: self.size,
                address: self.address,
                initial: self.state,
                final_state: Some(last),
            },
        );
        self.state = last;
        handle
    }

    /// # Safety
    ///
    /// The GPU no longer uses the buffer.
    unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// What the skinning and motion passes leave the buffers in.
const SKIN_STATE: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::from_raw(
        vk::PipelineStageFlags2::COMPUTE_SHADER.as_raw()
            | vk::PipelineStageFlags2::VERTEX_SHADER.as_raw(),
    ),
    access: vk::AccessFlags2::from_raw(
        vk::AccessFlags2::SHADER_STORAGE_READ.as_raw()
            | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw(),
    ),
};

#[derive(Debug)]
struct Skin {
    generation: u32,
    live: bool,
    /// The bind pose, for the instances' meshes.
    mesh: OwnedMeshData,
    /// Bind-pose box of the vertices each joint influences (`[MAX, MIN]` for none).
    joint_boxes: Vec<[Vec3; 2]>,
    joint_count: u16,
    vertex_count: u32,
    /// Words per source vertex: 9 (four influences) or 13 (eight).
    stride: u32,
    /// Its words in the source buffer.
    range: Option<Range>,
    /// Words still to upload (cleared once copied).
    pending: Vec<u32>,
    /// Live instances (indices).
    instances: Vec<u32>,
}

#[derive(Debug)]
struct Skinned {
    generation: u32,
    live: bool,
    skin: u32,
    mesh: MeshId,
    /// Its vertices in the previous-positions buffer.
    previous: Option<Range>,
    palette: Vec<[f32; 12]>,
    posed: bool,
    reset: bool,
    /// World transform rows for this frame, and as drawn last frame.
    rows: [[f32; 4]; 3],
    drawn: Option<[[f32; 4]; 3]>,
    bounds: [Vec3; 2],
    bounds_dirty: bool,
    scene: Option<InstanceId>,
}

/// What waits for the frames that may still use it.
#[derive(Debug)]
enum Retired {
    Buffer(Pooled),
    Words(Range),
    Previous(Range),
}

/// One dispatch of a frame.
#[derive(Debug, Clone, Copy)]
struct Group {
    eight: bool,
    push: SkinPush,
}

/// One draw of the motion pass.
#[derive(Debug, Clone, Copy)]
struct MotionDraw {
    index_count: u32,
    first_index: u32,
    vertex_offset: i32,
}

/// Skinned meshes (see the module documentation).
pub struct Skinning {
    skin4: ComputeShader<SkinPush>,
    skin8: ComputeShader<SkinPush>,
    motion: Pipeline,
    skins: Vec<Skin>,
    free_skins: Vec<u32>,
    instances: Vec<Skinned>,
    free_instances: Vec<u32>,
    /// Source vertices of every skin, in 32-bit words.
    words: RangeAllocator,
    sources: Pooled,
    /// Last frame's skinned positions of every instance, in vertices.
    previous_ranges: RangeAllocator,
    previous: Pooled,
    retired: Vec<(u64, Retired)>,
    frames_in_flight: u64,
    // Per-frame scratch, kept for its capacity.
    groups: Vec<Group>,
    uploads: Vec<vk::BufferCopy>,
    upload_source: vk::Buffer,
    table: Vec<SkinInstance>,
    motion_instances: Vec<MotionInstance>,
    motion_draws: Vec<MotionDraw>,
    motion_table: u64,
}

impl core::fmt::Debug for Skinning {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Skinning")
            .field("skins", &(self.skins.len() - self.free_skins.len()))
            .field(
                "instances",
                &(self.instances.len() - self.free_instances.len()),
            )
            .finish_non_exhaustive()
    }
}

/// The skinning matrices' boxes: the union of each joint's bind-pose box transformed by
/// its matrix (rows of a 3 × 4).
fn posed_bounds(boxes: &[[Vec3; 2]], palette: &[[f32; 12]]) -> [Vec3; 2] {
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for (b, m) in boxes.iter().zip(palette) {
        if b[0].x > b[1].x {
            continue;
        }
        let center = (b[0] + b[1]) * 0.5;
        let extents = (b[1] - b[0]) * 0.5;
        let row = |r: usize| {
            (
                Vec3::new(m[4 * r], m[4 * r + 1], m[4 * r + 2]),
                m[4 * r + 3],
            )
        };
        let mut c = Vec3::ZERO;
        let mut e = Vec3::ZERO;
        for r in 0..3 {
            let (axis, t) = row(r);
            c[r] = axis.dot(center) + t;
            e[r] = axis.abs().dot(extents);
        }
        lo = lo.min(c - e);
        hi = hi.max(c + e);
    }
    if lo.x > hi.x {
        [Vec3::ZERO, Vec3::ZERO]
    } else {
        [lo, hi]
    }
}

fn rows_of(m: &Mat4) -> [[f32; 4]; 3] {
    let r = |i: usize| m.row(i).to_array();
    [r(0), r(1), r(2)]
}

/// A range of `size` units, growing the allocator when it is full (its buffer follows in
/// the next `begin_frame`).
fn reserve(ranges: &mut RangeAllocator, size: u64) -> Option<Range> {
    if let Some(range) = ranges.allocate(size.max(1), 1) {
        return Some(range);
    }
    let capacity = ranges.capacity();
    ranges.grow((capacity * 2).max(capacity + size.max(1)));
    ranges.allocate(size.max(1), 1)
}

impl Skinning {
    /// The skinning shaders and the motion pass's pipeline (for depth targets of
    /// `depth_format`), with room for `capacity_vertices` source and instance vertices to
    /// start (both grow).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        depth_format: vk::Format,
        capacity_vertices: u32,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::ShaderInterface("skinned motion: not SPIR-V".into());
        let vertex = spirv_words(MOTION_VERTEX).ok_or_else(invalid)?;
        let fragment = spirv_words(MOTION_FRAGMENT).ok_or_else(invalid)?;
        let formats = [crate::forward::MOTION_FORMAT];
        let desc = GraphicsPipelineDesc {
            color_formats: &formats,
            depth_format,
            cull_mode: vk::CullModeFlags::NONE,
            depth_write: false,
            depth_compare: vk::CompareOp::EQUAL,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &vertex,
                    entry: c"skinnedMotionVertex",
                },
                Some(ShaderStageDesc {
                    spirv: &fragment,
                    entry: c"skinnedMotionFragment",
                }),
            )
        };
        let motion = Pipeline::create_many(
            device,
            bindless.pipeline_layout(),
            &[PipelineDesc::Graphics(desc)],
        )?
        .pop()
        .ok_or_else(invalid)?;
        let vertices = u64::from(capacity_vertices.max(64));
        Ok(Self {
            skin4: ComputeShader::new(device, bindless, SKIN4, c"skin4")?,
            skin8: ComputeShader::new(device, bindless, SKIN8, c"skin8")?,
            motion,
            skins: Vec::new(),
            free_skins: Vec::new(),
            instances: Vec::new(),
            free_instances: Vec::new(),
            words: RangeAllocator::new(vertices * 9, 1),
            sources: Pooled::new(allocator, device, vertices * 9 * 4)?,
            previous_ranges: RangeAllocator::new(vertices, 1),
            previous: Pooled::new(allocator, device, vertices * PREVIOUS_BYTES)?,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            groups: Vec::new(),
            uploads: Vec::new(),
            upload_source: vk::Buffer::null(),
            table: Vec::new(),
            motion_instances: Vec::new(),
            motion_draws: Vec::new(),
            motion_table: 0,
        })
    }

    /// Adds a skin: its source vertices reach the GPU in the next [`Self::begin_frame`].
    pub fn add_skin(&mut self, data: &SkinnedMeshData<'_>) -> Result<SkinId, SkinError> {
        let m = &data.mesh;
        let n = m.positions.len();
        if m.normals.len() != n
            || !(m.tangents.is_empty() || m.tangents.len() == n)
            || !(m.uvs.is_empty() || m.uvs.len() == n)
        {
            return Err(SkinError::Mesh(MeshError::MismatchedLengths));
        }
        if !m.indices.len().is_multiple_of(3) {
            return Err(SkinError::Mesh(MeshError::NotTriangles));
        }
        if let Some(&index) = m.indices.iter().find(|&&i| i as usize >= n) {
            return Err(SkinError::Mesh(MeshError::IndexOutOfRange {
                index,
                vertex_count: n as u32,
            }));
        }
        if data.joints.len() != data.weights.len() || n == 0 && !data.joints.is_empty() {
            return Err(SkinError::Influences);
        }
        let sets = if n == 0 { 1 } else { data.joints.len() / n };
        if !(sets == 1 || sets == 2) || data.joints.len() != sets * n {
            return Err(SkinError::Influences);
        }
        if let Some(&j) = data
            .joints
            .iter()
            .flatten()
            .find(|&&j| j >= data.joint_count)
        {
            return Err(SkinError::JointOutOfRange(j));
        }
        let stride = 5 + 4 * sets as u32;
        let mut words = Vec::with_capacity(n * stride as usize);
        let mut joint_boxes =
            vec![[Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)]; data.joint_count as usize];
        for v in 0..n {
            let p = m.positions[v];
            words.extend(p.map(f32::to_bits));
            words.push(encode_octahedral(m.normals[v]));
            let tangent = m.tangents.get(v).copied().unwrap_or_else(|| {
                let t = Vec3::from(m.normals[v]).any_orthonormal_vector();
                [t.x, t.y, t.z, 1.0]
            });
            words.push(encode_tangent(tangent));
            let joints = &data.joints[v * sets..][..sets];
            let weights = &data.weights[v * sets..][..sets];
            let total: f32 = weights.iter().flatten().map(|w| w.max(0.0)).sum();
            for set in joints {
                words.push(u32::from(set[0]) | u32::from(set[1]) << 16);
                words.push(u32::from(set[2]) | u32::from(set[3]) << 16);
            }
            for (k, set) in weights.iter().enumerate() {
                let q = |i: usize| -> u32 {
                    if total <= 0.0 {
                        // No weight: all on the first joint.
                        u32::from(k == 0 && i == 0) * 65535
                    } else {
                        ((set[i].max(0.0) / total) * 65535.0).round() as u32
                    }
                };
                words.push(q(0) | q(1) << 16);
                words.push(q(2) | q(3) << 16);
                for i in 0..4 {
                    if q(i) > 0 {
                        let b = &mut joint_boxes[joints[k][i] as usize];
                        b[0] = b[0].min(Vec3::from(p));
                        b[1] = b[1].max(Vec3::from(p));
                    }
                }
            }
        }
        let range = reserve(&mut self.words, words.len() as u64)
            .ok_or(SkinError::Mesh(MeshError::PoolFull))?;
        let skin = Skin {
            generation: 0,
            live: true,
            mesh: OwnedMeshData {
                positions: m.positions.to_vec(),
                normals: m.normals.to_vec(),
                tangents: m.tangents.to_vec(),
                uvs: m.uvs.to_vec(),
                indices: m.indices.to_vec(),
            },
            joint_boxes,
            joint_count: data.joint_count,
            vertex_count: n as u32,
            stride,
            range: Some(range),
            pending: words,
            instances: Vec::new(),
        };
        let index = match self.free_skins.pop() {
            Some(index) => {
                let generation = self.skins[index as usize].generation;
                self.skins[index as usize] = Skin { generation, ..skin };
                index
            }
            None => {
                self.skins.push(skin);
                (self.skins.len() - 1) as u32
            }
        };
        Ok(SkinId {
            index,
            generation: self.skins[index as usize].generation,
        })
    }

    fn skin(&self, id: SkinId) -> Option<&Skin> {
        self.skins
            .get(id.index as usize)
            .filter(|s| s.live && s.generation == id.generation)
    }

    /// Removes a skin and its instances (their meshes leave `geometry`), after the frames
    /// that may still use it (`frame_number` is the current frame).
    pub fn remove_skin(&mut self, id: SkinId, geometry: &mut GeometryPool, frame_number: u64) {
        if self.skin(id).is_none() {
            return;
        }
        let instances = core::mem::take(&mut self.skins[id.index as usize].instances);
        for i in instances {
            let generation = self.instances[i as usize].generation;
            self.remove_instance(
                SkinnedId {
                    index: i,
                    generation,
                },
                geometry,
                frame_number,
            );
        }
        let skin = &mut self.skins[id.index as usize];
        skin.live = false;
        skin.generation = skin.generation.wrapping_add(1);
        skin.pending = Vec::new();
        if let Some(range) = skin.range.take() {
            self.retired.push((frame_number, Retired::Words(range)));
        }
        self.free_skins.push(id.index);
    }

    /// Adds an instance of `skin` with a mesh of its own in `geometry` (returned: draw it
    /// like any mesh). It shows the bind pose until [`Self::set_pose`].
    pub fn add_instance(
        &mut self,
        skin: SkinId,
        geometry: &mut GeometryPool,
    ) -> Result<(SkinnedId, MeshId), SkinError> {
        let s = self.skin(skin).ok_or(SkinError::Stale)?;
        let mesh = geometry
            .add_mesh(&s.mesh.as_data())
            .map_err(SkinError::Mesh)?;
        let (vertex_count, joint_count) = (s.vertex_count, s.joint_count);
        let bounds = geometry
            .mesh(mesh)
            .map_or([Vec3::ZERO; 2], |m| [m.min, m.max]);
        let Some(previous) = reserve(&mut self.previous_ranges, u64::from(vertex_count)) else {
            geometry.remove_mesh(mesh, 0);
            return Err(SkinError::Mesh(MeshError::PoolFull));
        };
        let instance = Skinned {
            generation: 0,
            live: true,
            skin: skin.index,
            mesh,
            previous: Some(previous),
            palette: vec![
                [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];
                joint_count as usize
            ],
            posed: false,
            reset: true,
            rows: rows_of(&Mat4::IDENTITY),
            drawn: None,
            bounds,
            bounds_dirty: false,
            scene: None,
        };
        let index = match self.free_instances.pop() {
            Some(index) => {
                let generation = self.instances[index as usize].generation;
                self.instances[index as usize] = Skinned {
                    generation,
                    ..instance
                };
                index
            }
            None => {
                self.instances.push(instance);
                (self.instances.len() - 1) as u32
            }
        };
        self.skins[skin.index as usize].instances.push(index);
        Ok((
            SkinnedId {
                index,
                generation: self.instances[index as usize].generation,
            },
            mesh,
        ))
    }

    fn instance_mut(&mut self, id: SkinnedId) -> Option<&mut Skinned> {
        self.instances
            .get_mut(id.index as usize)
            .filter(|s| s.live && s.generation == id.generation)
    }

    /// Removes an instance; its mesh leaves `geometry`.
    pub fn remove_instance(
        &mut self,
        id: SkinnedId,
        geometry: &mut GeometryPool,
        frame_number: u64,
    ) {
        let Some(instance) = self.instance_mut(id) else {
            return;
        };
        instance.live = false;
        instance.generation = instance.generation.wrapping_add(1);
        let (mesh, skin, previous) = (instance.mesh, instance.skin, instance.previous.take());
        geometry.remove_mesh(mesh, frame_number);
        if let Some(range) = previous {
            self.retired.push((frame_number, Retired::Previous(range)));
        }
        self.skins[skin as usize]
            .instances
            .retain(|&i| i != id.index);
        self.free_instances.push(id.index);
    }

    /// The pose of an instance for the next frame: its skinning matrices (`model[joint] ·
    /// inverse_bind[joint]` as three rows each, `engine_anim::skinning_matrices`; one per
    /// joint of the skin, extra ones ignored) and its object-to-world transform (the same
    /// one its draw item or scene instance uses, bit for bit: the motion pass matches the
    /// forward pass's depth). Allocates nothing.
    pub fn set_pose(&mut self, id: SkinnedId, palette: &[[f32; 12]], world_from_object: &Mat4) {
        let Some(index) = self.instance_mut(id).map(|_| id.index as usize) else {
            return;
        };
        let skin = self.instances[index].skin as usize;
        let boxes = &self.skins[skin].joint_boxes;
        let instance = &mut self.instances[index];
        let n = instance.palette.len().min(palette.len());
        instance.palette[..n].copy_from_slice(&palette[..n]);
        instance.posed = true;
        instance.rows = rows_of(world_from_object);
        let bounds = posed_bounds(boxes, &instance.palette);
        if bounds != instance.bounds {
            instance.bounds = bounds;
            instance.bounds_dirty = true;
        }
    }

    /// No motion for this instance in the next frame (a teleport, a new animation that
    /// should not blur from the old one): its previous positions and transform are the
    /// new ones.
    pub fn teleport(&mut self, id: SkinnedId) {
        if let Some(instance) = self.instance_mut(id) {
            instance.reset = true;
            instance.drawn = None;
        }
    }

    /// Ties an instance to its scene instance, whose bounds [`Self::update_bounds`] then
    /// refreshes (`None` for draw lists).
    pub fn set_scene_instance(&mut self, id: SkinnedId, scene: Option<InstanceId>) {
        if let Some(instance) = self.instance_mut(id) {
            instance.scene = scene;
            instance.bounds_dirty = true;
        }
    }

    /// The object-space box of an instance's posed mesh (as of its last [`Self::set_pose`]).
    pub fn bounds(&self, id: SkinnedId) -> Option<(Vec3, Vec3)> {
        let s = self
            .instances
            .get(id.index as usize)
            .filter(|s| s.live && s.generation == id.generation)?;
        Some((s.bounds[0], s.bounds[1]))
    }

    /// Gives the posed instances' boxes to the geometry pool (GPU culling) and their scene
    /// instances. Call after the frame's [`Self::set_pose`] calls, before
    /// [`GeometryPool::begin_frame`].
    pub fn update_bounds(&mut self, geometry: &mut GeometryPool, mut scene: Option<&mut GpuScene>) {
        for instance in self
            .instances
            .iter_mut()
            .filter(|i| i.live && i.bounds_dirty)
        {
            instance.bounds_dirty = false;
            geometry.set_bounds(instance.mesh, instance.bounds[0], instance.bounds[1]);
            if let (Some(scene), Some(id)) = (scene.as_deref_mut(), instance.scene) {
                scene.refresh_bounds(id);
            }
        }
    }

    fn collect_retired(&mut self, allocator: &mut Allocator, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            match self.retired.swap_remove(i).1 {
                Retired::Words(range) => self.words.free(range),
                Retired::Previous(range) => self.previous_ranges.free(range),
                // SAFETY: the frames that used it have finished.
                Retired::Buffer(mut buffer) => unsafe { buffer.destroy(allocator) },
            }
        }
    }

    /// Uploads new skins, then records the skinning pass: every posed instance's mesh in
    /// `geometry` is skinned (and its previous positions kept). Call once per frame after
    /// [`GeometryPool::begin_frame`], before the passes that draw. Allocates only on frames
    /// that add skins or grow the buffers.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_frame<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        frame_number: u64,
    ) -> Result<SkinningFrame<'g>, RhiError> {
        engine_profiler::zone!("render::Skinning::begin_frame");
        self.collect_retired(allocator, frame_number);
        // Grow: new buffers as large as the range allocators, the old contents copied.
        let mut grown: [Option<(Pooled, Buffer)>; 2] = [None, None];
        if self.words.capacity() * 4 > self.sources.size {
            let buffer = Pooled::new(allocator, device, self.words.capacity() * 4)?;
            let mut old = core::mem::replace(&mut self.sources, buffer);
            let handle = old.import(fg, "skin sources (old)", BufferState::idle());
            grown[0] = Some((old, handle));
        }
        if self.previous_ranges.capacity() * PREVIOUS_BYTES > self.previous.size {
            let buffer = Pooled::new(
                allocator,
                device,
                self.previous_ranges.capacity() * PREVIOUS_BYTES,
            )?;
            let mut old = core::mem::replace(&mut self.previous, buffer);
            let handle = old.import(fg, "skin previous (old)", BufferState::idle());
            grown[1] = Some((old, handle));
        }
        let sources = self.sources.import(fg, "skin sources", SKIN_STATE);
        let previous = self
            .previous
            .import(fg, "skin previous positions", SKIN_STATE);
        for (k, grown) in grown.iter_mut().enumerate() {
            let Some((old, handle)) = grown.take() else {
                continue;
            };
            let (source, target, size) = (handle, [sources, previous][k], old.size);
            fg.add_pass("skinning grow")
                .read(source, Access::TransferRead)
                .write(target, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the new buffer is at
                    // least as large as the old one.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(source),
                            ctx.buffer(target),
                            &[vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: 0,
                                size,
                            }],
                        );
                    }
                });
            self.retired.push((frame_number, Retired::Buffer(old)));
        }

        // New skins' words, through the ring.
        self.uploads.clear();
        self.upload_source = vk::Buffer::null();
        for skin in self
            .skins
            .iter_mut()
            .filter(|s| s.live && !s.pending.is_empty())
        {
            let (Some(range), Some(staged)) = (skin.range, ring.stage(&skin.pending)) else {
                // The ring is full this frame: try again next frame.
                continue;
            };
            self.upload_source = staged.buffer;
            self.uploads.push(vk::BufferCopy {
                src_offset: staged.offset,
                dst_offset: range.offset() * 4,
                size: skin.pending.len() as u64 * 4,
            });
            skin.pending = Vec::new();
        }
        self.record_dispatches(ring, geometry)?;

        // Everything below only reads `self` for the rest of the frame.
        let this: &'g Self = self;
        if !this.uploads.is_empty() {
            let (uploads, ring_buffer) = (&this.uploads, this.upload_source);
            fg.add_pass("skin upload")
                .write(sources, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the regions lie in the
                    // ring slot and the source buffer.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ring_buffer,
                            ctx.buffer(sources),
                            uploads,
                        );
                    }
                });
        }
        if !this.groups.is_empty() {
            let groups = &this.groups;
            let (skin4, skin8) = (&this.skin4, &this.skin8);
            fg.add_compute("skinning", bindless)
                .read(sources, Access::StorageRead(ShaderStages::COMPUTE))
                .write(
                    geometry.positions,
                    Access::StorageReadWrite(ShaderStages::COMPUTE),
                )
                .write(
                    geometry.attributes,
                    Access::StorageWrite(ShaderStages::COMPUTE),
                )
                .write(previous, Access::StorageWrite(ShaderStages::COMPUTE))
                .record(move |rec| {
                    for g in groups {
                        let shader = if g.eight { skin8 } else { skin4 };
                        let size =
                            Dispatch::Threads([g.push.vertex_count, g.push.instance_count, 1]);
                        rec.dispatch(shader, size, &g.push);
                    }
                });
        }
        Ok(SkinningFrame {
            skinning: this,
            previous,
        })
    }

    /// The frame's dispatch groups, palettes and instance tables (in the ring), and the
    /// motion pass's instance table.
    fn record_dispatches(
        &mut self,
        ring: &mut UploadRing,
        geometry: &GeometryFrame<'_>,
    ) -> Result<(), RhiError> {
        let full = || RhiError::Vulkan {
            call: "skinning: upload ring full",
            result: vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        };
        self.groups.clear();
        self.motion_instances.clear();
        self.motion_draws.clear();
        let previous_address = self.previous.address;
        for skin in self.skins.iter().filter(|s| s.live && s.pending.is_empty()) {
            let Some(range) = skin.range else { continue };
            if skin.vertex_count == 0 {
                continue;
            }
            self.table.clear();
            for &i in &skin.instances {
                let instance = &mut self.instances[i as usize];
                if !instance.posed {
                    continue;
                }
                let (Some(info), Some(prev)) = (geometry.mesh(instance.mesh), instance.previous)
                else {
                    continue;
                };
                let palette = ring.push(&instance.palette).ok_or_else(full)?;
                let previous = previous_address + prev.offset() * PREVIOUS_BYTES;
                self.table.push(SkinInstance {
                    palette,
                    previous,
                    first_vertex: info.first_vertex,
                    flags: if instance.reset { RESET } else { 0 },
                    _pad: [0; 2],
                });
                instance.reset = false;
                let rows = instance.rows;
                self.motion_instances.push(MotionInstance {
                    rows,
                    previous_rows: instance.drawn.unwrap_or(rows),
                    previous,
                    first_vertex: info.first_vertex,
                    _pad: 0,
                });
                self.motion_draws.push(MotionDraw {
                    index_count: info.index_count,
                    first_index: info.first_index,
                    vertex_offset: info.first_vertex as i32,
                });
                instance.drawn = Some(rows);
            }
            if self.table.is_empty() {
                continue;
            }
            let table = ring.push(&self.table).ok_or_else(full)?;
            self.groups.push(Group {
                eight: skin.stride > 9,
                push: SkinPush {
                    source: self.sources.address + range.offset() * 4,
                    instances: table,
                    positions: geometry.position_address(),
                    attributes: geometry.attribute_address(),
                    vertex_count: skin.vertex_count,
                    stride: skin.stride,
                    instance_count: self.table.len() as u32,
                    _pad: 0,
                },
            });
        }
        self.motion_table = if self.motion_instances.is_empty() {
            0
        } else {
            ring.push(&self.motion_instances).ok_or_else(full)?
        };
        Ok(())
    }

    /// Instances skinned in the last [`Self::begin_frame`].
    pub fn skinned_last_frame(&self) -> usize {
        self.motion_draws.len()
    }

    /// Frees the buffers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.sources.destroy(allocator);
            self.previous.destroy(allocator);
            for (_, retired) in self.retired.drain(..) {
                if let Retired::Buffer(mut buffer) = retired {
                    buffer.destroy(allocator);
                }
            }
        }
    }
}

/// A frame's skinning, for the views' motion passes.
#[derive(Debug, Clone, Copy)]
pub struct SkinningFrame<'g> {
    skinning: &'g Skinning,
    previous: Buffer,
}

impl<'g> SkinningFrame<'g> {
    /// Instances skinned this frame.
    pub fn instances(&self) -> usize {
        self.skinning.motion_draws.len()
    }

    /// The previous-positions buffer (three floats per vertex), for passes of your own
    /// (debug views, tests).
    pub fn previous_buffer(&self) -> Buffer {
        self.previous
    }

    /// Where an instance's previous positions are in [`Self::previous_buffer`]: byte offset
    /// and size.
    pub fn previous_range(&self, id: SkinnedId) -> Option<(u64, u64)> {
        let s = self
            .skinning
            .instances
            .get(id.index as usize)
            .filter(|s| s.live && s.generation == id.generation)?;
        let range = s.previous?;
        Some((
            range.offset() * PREVIOUS_BYTES,
            range.size() * PREVIOUS_BYTES,
        ))
    }

    /// Overwrites the motion vectors of the skinned instances' pixels in `motion` (from a
    /// forward pass that drew into `depth`) with their skinned vertices' own motion.
    /// Nothing is recorded without skinned instances.
    #[allow(clippy::too_many_arguments)]
    pub fn add_motion_pass(
        &self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        depth: Texture,
        motion: &MotionOutput,
    ) -> Result<(), RhiError> {
        let skinning = self.skinning;
        if skinning.motion_draws.is_empty() {
            return Ok(());
        }
        let desc = fg.texture_desc(motion.velocity);
        let (width, height) = (desc.width.max(1), desc.height.max(1));
        let jitter_ndc = [
            2.0 * motion.jitter[0] / width as f32,
            2.0 * motion.jitter[1] / height as f32,
        ];
        let view = ring
            .push(&[MotionView {
                clip_from_world: motion.clip_from_world.to_cols_array(),
                previous_clip_from_world: motion.previous_clip_from_world.to_cols_array(),
                jitter: [jitter_ndc[0], jitter_ndc[1], 0.0, 0.0],
                size: [width, height],
                _pad: [0; 2],
            }])
            .ok_or(RhiError::Vulkan {
                call: "skinned motion: upload ring full",
                result: vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
            })?;
        let push = MotionPush {
            view,
            instances: skinning.motion_table,
            positions: geometry.position_address(),
        };
        let geometry = *geometry;
        let draws = &skinning.motion_draws;
        let pipeline = &skinning.motion;
        fg.add_pass("skinned motion")
            .color(motion.velocity, LoadOp::Load, StoreOp::Store)
            .depth_read_only(depth)
            .read(
                geometry.positions,
                Access::StorageRead(ShaderStages::VERTEX),
            )
            .read(geometry.indices, Access::IndexBuffer)
            .read(self.previous, Access::StorageRead(ShaderStages::VERTEX))
            .execute(move |ctx| {
                let area = ctx.render_area();
                // SAFETY: recording inside the pass's rendering scope; the pass declared
                // the pool's reads, the tables are in the ring, every draw's ranges are
                // its mesh's in the pool.
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
                    geometry.bind_indices(ctx);
                    for (k, d) in draws.iter().enumerate() {
                        ctx.device.cmd_draw_indexed(
                            ctx.cmd,
                            d.index_count,
                            1,
                            d.first_index,
                            d.vertex_offset,
                            k as u32,
                        );
                    }
                }
            });
        Ok(())
    }
}
