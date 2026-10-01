//! Meshes in a shared geometry pool (ADR 0039).
//!
//! Every mesh lives in three device-local buffers shared by all meshes, so the forward
//! renderer draws them all with indexed indirect draws and never rebinds:
//!
//! - **positions**, `[f32; 3]` (12 bytes per vertex), in a buffer of their own: depth-only
//!   passes (shadow maps) fetch nothing else;
//! - **attributes**, [`VertexAttributes`] (16 bytes): normal and tangent in octahedral
//!   encoding at 16 bits per axis (the tangent's bitangent sign in the lowest bit), one
//!   set of texture coordinates as `f32 × 2`;
//! - **indices**, 32-bit, relative to the mesh's first vertex (the draw's vertex offset
//!   adds it).
//!
//! Vertex and index ranges come from two [`RangeAllocator`]s. When one is full, its
//! buffers grow: [`GeometryPool::begin_frame`] creates buffers twice as large (or as
//! large as needed), copies the old contents on the GPU, and frees the old buffers once
//! the frames that used them have finished. Meshes are added on the CPU at any time
//! ([`GeometryPool::add_mesh`]): their encoded data waits in CPU memory until
//! `begin_frame` copies it through a staging buffer. Removed meshes release their ranges
//! only after the frames that could still draw them.
//!
//! `begin_frame` also imports the three buffers into the frame's graph
//! ([`GeometryFrame`]), so the passes that draw from them are ordered after the copies.

use crate::graph::{
    Access, Buffer, BufferState, FrameGraph, ImportedBuffer, PassContext, ShaderStages,
};
use engine_core::math::{Mat4, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Device, MemoryLocation, Range, RangeAllocator, RhiError};

/// Vertex attributes besides the position, as the shaders read them (16 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct VertexAttributes {
    /// Unit normal, [`encode_octahedral`].
    pub normal: u32,
    /// Unit tangent and bitangent sign, [`encode_tangent`].
    pub tangent: u32,
    /// Texture coordinates.
    pub uv: [f32; 2],
}

const _: () = assert!(size_of::<VertexAttributes>() == 16);

/// A snorm16 from a value in [-1, 1].
fn snorm16(v: f32) -> u32 {
    u32::from((v.clamp(-1.0, 1.0) * 32767.0).round() as i16 as u16)
}

/// The value of a snorm16.
fn from_snorm16(bits: u32) -> f32 {
    (f32::from(bits as u16 as i16) / 32767.0).max(-1.0)
}

/// `v` on the octahedron, unfolded to [-1, 1]² (Cigolle et al., JCGT 2014).
fn octahedral(v: Vec3) -> (f32, f32) {
    let l1 = v.x.abs() + v.y.abs() + v.z.abs();
    if !(l1 > 0.0 && l1.is_finite()) {
        // Degenerate input: +Z.
        return (0.0, 0.0);
    }
    let (x, y) = (v.x / l1, v.y / l1);
    if v.z >= 0.0 {
        return (x, y);
    }
    let sign = |a: f32| if a >= 0.0 { 1.0 } else { -1.0 };
    ((1.0 - y.abs()) * sign(x), (1.0 - x.abs()) * sign(y))
}

/// Angle between two directions in radians, in double precision: `atan2(|a × b|, a · b)`
/// stays accurate for tiny angles, where the arc cosine of an `f32` dot product cannot
/// resolve less than about 0.02°. For tests.
#[cfg(test)]
fn angle(a: Vec3, b: Vec3) -> f64 {
    let (a, b) = (
        [f64::from(a.x), f64::from(a.y), f64::from(a.z)],
        [f64::from(b.x), f64::from(b.y), f64::from(b.z)],
    );
    let cross = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    let sin = (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt();
    let cos = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    sin.atan2(cos)
}

/// Cosine of the angle between the unit vector `unit` and the decoded octahedral pair
/// `(x, y)`, in double precision: candidates differ by about 1e-8 in it, below what `f32`
/// resolves near 1.
fn octahedral_cosine(x: f32, y: f32, unit: [f64; 3]) -> f64 {
    let (x, y) = (f64::from(x), f64::from(y));
    let z = 1.0 - x.abs() - y.abs();
    let t = (-z).max(0.0);
    let x = if x >= 0.0 { x - t } else { x + t };
    let y = if y >= 0.0 { y - t } else { y + t };
    (x * unit[0] + y * unit[1] + z * unit[2]) / (x * x + y * y + z * z).sqrt()
}

/// The unit vector of an octahedral pair (as the shaders decode it).
fn from_octahedral(x: f32, y: f32) -> Vec3 {
    let z = 1.0 - x.abs() - y.abs();
    let t = (-z).max(0.0);
    let x = if x >= 0.0 { x - t } else { x + t };
    let y = if y >= 0.0 { y - t } else { y + t };
    Vec3::new(x, y, z).normalize()
}

/// A unit vector in octahedral encoding, two snorm16 in one `u32` (x in the low half). Of
/// the four roundings of the pair, the one that decodes closest to `v` is kept (Cigolle's
/// "precise" variant): at most about 0.002° of error. Zero, infinite or NaN input
/// encodes +Z.
pub fn encode_octahedral(v: [f32; 3]) -> u32 {
    let v = Vec3::from(v);
    let (x, y) = octahedral(v);
    let unit = v.normalize_or(Vec3::Z);
    let length =
        (f64::from(unit.x).powi(2) + f64::from(unit.y).powi(2) + f64::from(unit.z).powi(2)).sqrt();
    let unit = [
        f64::from(unit.x) / length,
        f64::from(unit.y) / length,
        f64::from(unit.z) / length,
    ];
    let mut best = (snorm16(x), snorm16(y));
    let mut best_cosine = f64::MIN;
    for (fx, fy) in [(false, false), (true, false), (false, true), (true, true)] {
        let round = |a: f32, up: bool| {
            let scaled = a.clamp(-1.0, 1.0) * 32767.0;
            let r = if up { scaled.ceil() } else { scaled.floor() };
            u32::from(r.clamp(-32767.0, 32767.0) as i16 as u16)
        };
        let (qx, qy) = (round(x, fx), round(y, fy));
        let cosine = octahedral_cosine(from_snorm16(qx), from_snorm16(qy), unit);
        if cosine > best_cosine {
            best_cosine = cosine;
            best = (qx, qy);
        }
    }
    best.0 | (best.1 << 16)
}

/// The unit vector of [`encode_octahedral`].
pub fn decode_octahedral(bits: u32) -> [f32; 3] {
    from_octahedral(from_snorm16(bits & 0xFFFF), from_snorm16(bits >> 16)).to_array()
}

/// A unit tangent with the bitangent sign in `w` (glTF's convention: the bitangent is
/// `cross(normal, tangent) × w`), in octahedral encoding with the sign in the lowest bit
/// of the second component (1: negative).
pub fn encode_tangent(t: [f32; 4]) -> u32 {
    let bits = encode_octahedral([t[0], t[1], t[2]]) & !(1 << 16);
    bits | (u32::from(t[3] < 0.0) << 16)
}

/// The tangent and bitangent sign of [`encode_tangent`].
pub fn decode_tangent(bits: u32) -> ([f32; 3], f32) {
    let sign = if bits & (1 << 16) != 0 { -1.0 } else { 1.0 };
    (decode_octahedral(bits), sign)
}

/// A unit vector perpendicular to `n` (Duff et al., "Building an Orthonormal Basis,
/// Revisited", JCGT 2017): the tangent of meshes without one.
fn any_tangent(n: Vec3) -> Vec3 {
    let n = n.normalize_or(Vec3::Z);
    let sign = if n.z >= 0.0 { 1.0 } else { -1.0 };
    let a = -1.0 / (sign + n.z);
    let b = n.x * n.y * a;
    Vec3::new(1.0 + sign * n.x * n.x * a, sign * b, -sign * n.x)
}

/// A mesh to add: an indexed triangle list with counter-clockwise front faces.
#[derive(Debug, Clone, Copy)]
pub struct MeshData<'a> {
    /// Positions.
    pub positions: &'a [[f32; 3]],
    /// Unit normals, one per vertex.
    pub normals: &'a [[f32; 3]],
    /// Unit tangents with the bitangent sign in `w`, one per vertex (glTF's convention),
    /// or empty: a tangent perpendicular to the normal is made up (enough without normal
    /// maps).
    pub tangents: &'a [[f32; 4]],
    /// Texture coordinates, one per vertex, or empty (zeros).
    pub uvs: &'a [[f32; 2]],
    /// Triangles, three indices each.
    pub indices: &'a [u32],
}

/// An owned [`MeshData`] (procedural meshes).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnedMeshData {
    /// Positions.
    pub positions: Vec<[f32; 3]>,
    /// Normals.
    pub normals: Vec<[f32; 3]>,
    /// Tangents with the bitangent sign in `w`.
    pub tangents: Vec<[f32; 4]>,
    /// Texture coordinates.
    pub uvs: Vec<[f32; 2]>,
    /// Triangle indices.
    pub indices: Vec<u32>,
}

impl OwnedMeshData {
    /// Borrowed view.
    pub fn as_data(&self) -> MeshData<'_> {
        MeshData {
            positions: &self.positions,
            normals: &self.normals,
            tangents: &self.tangents,
            uvs: &self.uvs,
            indices: &self.indices,
        }
    }
}

/// A unit cube centred on the origin: 24 vertices (4 per face, so each face has its own
/// normal, tangent and texture coordinates from 0 to 1) and 36 indices, counter-clockwise
/// front faces.
pub fn cube() -> OwnedMeshData {
    let faces: [(Vec3, Vec3, Vec3); 6] = [
        // (normal, u axis, v axis): u × v = normal, so the quad winds counter-clockwise.
        (Vec3::X, Vec3::NEG_Z, Vec3::Y),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::X, Vec3::NEG_Z),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y),
    ];
    let mut mesh = OwnedMeshData::default();
    for (n, u, v) in faces {
        let c = n * 0.5;
        let base = mesh.positions.len() as u32;
        // Texture v grows downwards (glTF, Vulkan): the top of the image at the face's +v.
        for (du, dv) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
            mesh.positions.push((c + u * du + v * dv).to_array());
            mesh.normals.push(n.to_array());
            // glTF's tangent basis as exported (Blender): the tangent follows increasing
            // u, the bitangent (cross(n, t) × w) points up the image, towards decreasing
            // v, like the green channel of a normal map. cross(n, u) = v, and v decreases
            // towards the face's +v: w = +1.
            mesh.tangents.push(u.extend(1.0).to_array());
            mesh.uvs.push([du + 0.5, 0.5 - dv]);
        }
        mesh.indices
            .extend([0, 1, 2, 0, 2, 3].into_iter().map(|i| base + i));
    }
    mesh
}

/// A right-handed perspective projection for Vulkan: depth 0..1 (near..far) and the Y
/// flip of Vulkan's clip space folded in, so +Y is up on screen.
pub fn perspective(fov_y_radians: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    engine_core::math::vulkan_perspective(fov_y_radians, aspect, near, far)
}

/// Why a mesh was not added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshError {
    /// Normals, tangents or texture coordinates do not match the positions in number.
    MismatchedLengths,
    /// The index count is not a multiple of three.
    NotTriangles,
    /// An index beyond the vertices.
    IndexOutOfRange {
        /// The index.
        index: u32,
        /// Vertices in the mesh.
        vertex_count: u32,
    },
    /// The pool cannot hold more (2^32 vertices or indices).
    PoolFull,
    /// More detail levels than [`MAX_LODS`] (the full mesh included).
    TooManyLods,
}

impl core::fmt::Display for MeshError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MismatchedLengths => write!(f, "mesh attributes differ in length"),
            Self::NotTriangles => write!(f, "index count is not a multiple of three"),
            Self::TooManyLods => write!(f, "more than {MAX_LODS} detail levels"),
            Self::IndexOutOfRange {
                index,
                vertex_count,
            } => write!(f, "index {index} beyond {vertex_count} vertices"),
            Self::PoolFull => write!(f, "geometry pool is full"),
        }
    }
}

impl std::error::Error for MeshError {}

/// Most detail levels a mesh has, the full mesh included (ADR 0053).
pub const MAX_LODS: usize = 4;

/// One detail level of a mesh: a range of indices over the mesh's vertices.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeshLod {
    /// First index.
    pub first_index: u32,
    /// Indices (three per triangle).
    pub index_count: u32,
    /// How far this level's surface strays from the full mesh, in object-space units (0 for
    /// the full mesh). The renderer draws the coarsest level whose error covers at most a
    /// pixel or so on screen.
    pub error: f32,
}

/// A simplified level added with a mesh ([`GeometryPool::add_mesh_with_lods`]): triangles
/// over the mesh's own vertices, and their error in object-space units.
#[derive(Debug, Clone, Copy)]
pub struct LodIndices<'a> {
    /// Triangles, three indices each, into the mesh's vertices.
    pub indices: &'a [u32],
    /// Geometric error against the full mesh (object space).
    pub error: f32,
}

/// A mesh in a [`GeometryPool`]. Ids of removed meshes are stale: lookups give `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct MeshId {
    index: u32,
    generation: u32,
}

/// Where a mesh lives in the pool, and its bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshInfo {
    /// First vertex (the draw's vertex offset).
    pub first_vertex: u32,
    /// Vertices.
    pub vertex_count: u32,
    /// First index.
    pub first_index: u32,
    /// Indices (three per triangle).
    pub index_count: u32,
    /// Object-space bounding box: minimum corner.
    pub min: Vec3,
    /// Maximum corner.
    pub max: Vec3,
    /// Detail levels, finest first; the first is the full mesh (`first_index`,
    /// `index_count`). Only `lod_count` are used.
    pub lods: [MeshLod; MAX_LODS],
    /// Detail levels (at least 1).
    pub lod_count: u32,
    /// Changes when the GPU gives the mesh another shape ([`GeometryPool::mark_deformed`],
    /// a skinned mesh posed differently): what cached shadows of local lights compare
    /// (ADR 0071).
    pub revision: u32,
}

/// Initial sizes of a [`GeometryPool`]; it grows as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeometryCapacity {
    /// Vertices (28 bytes each).
    pub vertices: u32,
    /// Indices (4 bytes each).
    pub indices: u32,
}

impl Default for GeometryCapacity {
    fn default() -> Self {
        Self {
            vertices: 1 << 18,
            indices: 1 << 20,
        }
    }
}

const POSITION_BYTES: u64 = 12;
const ATTRIBUTE_BYTES: u64 = size_of::<VertexAttributes>() as u64;
const INDEX_BYTES: u64 = 4;

/// Reads of the vertex buffers by vertex shaders, and of the index buffer by the input
/// assembler: the state the buffers are left in at the end of every frame.
const VERTEX_READ: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::VERTEX_SHADER,
    access: vk::AccessFlags2::SHADER_STORAGE_READ,
};
const INDEX_READ: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::INDEX_INPUT,
    access: vk::AccessFlags2::INDEX_READ,
};

/// A device-local buffer with its address and the state the graph left it in.
#[derive(Debug)]
struct PoolBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    size: u64,
    state: BufferState,
}

impl PoolBuffer {
    fn new(
        device: &Device,
        allocator: &mut Allocator,
        size: u64,
        usage: vk::BufferUsageFlags,
    ) -> Result<Self, RhiError> {
        let size = size.max(16);
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(
                usage
                    | vk::BufferUsageFlags::STORAGE_BUFFER
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

#[derive(Debug)]
struct Slot {
    generation: u32,
    mesh: Option<(MeshInfo, Range, Range)>,
}

/// A mesh's data waiting for [`GeometryPool::begin_frame`]: where it goes in the pool and
/// where it starts in the staged arrays.
#[derive(Debug, Clone, Copy)]
struct Upload {
    first_vertex: u32,
    vertex_count: u32,
    first_index: u32,
    index_count: u32,
    staged_vertex: u32,
    staged_index: u32,
}

/// What waits for the frames that may still use it.
#[derive(Debug)]
enum Retired {
    /// A removed mesh's vertex and index ranges.
    Ranges(Range, Range),
    /// A buffer replaced by a larger one, or a staging buffer.
    Buffer(PoolBuffer),
}

/// Vertices and indices of every mesh (see the module documentation).
#[derive(Debug)]
pub struct GeometryPool {
    positions: PoolBuffer,
    attributes: PoolBuffer,
    indices: PoolBuffer,
    vertex_ranges: RangeAllocator,
    index_ranges: RangeAllocator,
    slots: Vec<Slot>,
    free_slots: Vec<u32>,
    staged_positions: Vec<[f32; 3]>,
    staged_attributes: Vec<VertexAttributes>,
    staged_indices: Vec<u32>,
    uploads: Vec<Upload>,
    retired: Vec<(u64, Retired)>,
    frames_in_flight: u64,
}

impl GeometryPool {
    /// An empty pool with room for `capacity`; frames in flight bound how long removed
    /// meshes and replaced buffers are kept.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        capacity: GeometryCapacity,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let vertices = u64::from(capacity.vertices.max(1));
        let indices = u64::from(capacity.indices.max(3));
        Ok(Self {
            positions: PoolBuffer::new(
                device,
                allocator,
                vertices * POSITION_BYTES,
                vk::BufferUsageFlags::empty(),
            )?,
            attributes: PoolBuffer::new(
                device,
                allocator,
                vertices * ATTRIBUTE_BYTES,
                vk::BufferUsageFlags::empty(),
            )?,
            indices: PoolBuffer::new(
                device,
                allocator,
                indices * INDEX_BYTES,
                vk::BufferUsageFlags::INDEX_BUFFER,
            )?,
            vertex_ranges: RangeAllocator::new(vertices, 1),
            index_ranges: RangeAllocator::new(indices, 1),
            slots: Vec::new(),
            free_slots: Vec::new(),
            staged_positions: Vec::new(),
            staged_attributes: Vec::new(),
            staged_indices: Vec::new(),
            uploads: Vec::new(),
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
        })
    }

    /// Adds a mesh; its data reaches the GPU in the next [`Self::begin_frame`], before any
    /// pass of that frame draws it.
    pub fn add_mesh(&mut self, data: &MeshData<'_>) -> Result<MeshId, MeshError> {
        self.add_mesh_with_lods(data, &[])
    }

    /// Adds a mesh with simplified levels (coarser ones last, their errors increasing),
    /// drawn instead of the full mesh when they differ from it by about a pixel or less on
    /// screen (ADR 0053). The levels share the mesh's vertices; `engine_assets` makes them.
    pub fn add_mesh_with_lods(
        &mut self,
        data: &MeshData<'_>,
        lods: &[LodIndices<'_>],
    ) -> Result<MeshId, MeshError> {
        if lods.len() >= MAX_LODS {
            return Err(MeshError::TooManyLods);
        }
        let vertex_count = data.positions.len();
        if data.normals.len() != vertex_count
            || !(data.tangents.is_empty() || data.tangents.len() == vertex_count)
            || !(data.uvs.is_empty() || data.uvs.len() == vertex_count)
        {
            return Err(MeshError::MismatchedLengths);
        }
        let levels = || core::iter::once(data.indices).chain(lods.iter().map(|l| l.indices));
        if levels().any(|indices| !indices.len().is_multiple_of(3)) {
            return Err(MeshError::NotTriangles);
        }
        let vertex_count = u32::try_from(vertex_count).map_err(|_| MeshError::PoolFull)?;
        let total_indices: usize = levels().map(<[u32]>::len).sum();
        let index_count = u32::try_from(total_indices).map_err(|_| MeshError::PoolFull)?;
        if let Some(&index) = levels().flatten().find(|&&i| i >= vertex_count) {
            return Err(MeshError::IndexOutOfRange {
                index,
                vertex_count,
            });
        }
        let vertices = reserve(&mut self.vertex_ranges, u64::from(vertex_count))?;
        let indices = match reserve(&mut self.index_ranges, u64::from(index_count)) {
            Ok(range) => range,
            Err(e) => {
                self.vertex_ranges.free(vertices);
                return Err(e);
            }
        };

        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for &p in data.positions {
            min = min.min(Vec3::from(p));
            max = max.max(Vec3::from(p));
        }
        if vertex_count == 0 {
            (min, max) = (Vec3::ZERO, Vec3::ZERO);
        }
        let upload = Upload {
            first_vertex: vertices.offset() as u32,
            vertex_count,
            first_index: indices.offset() as u32,
            index_count,
            staged_vertex: self.staged_positions.len() as u32,
            staged_index: self.staged_indices.len() as u32,
        };
        self.staged_positions.extend_from_slice(data.positions);
        self.staged_attributes
            .extend((0..vertex_count as usize).map(|v| {
                let normal = data.normals[v];
                let tangent = data
                    .tangents
                    .get(v)
                    .copied()
                    .unwrap_or_else(|| any_tangent(Vec3::from(normal)).extend(1.0).to_array());
                VertexAttributes {
                    normal: encode_octahedral(normal),
                    tangent: encode_tangent(tangent),
                    uv: data.uvs.get(v).copied().unwrap_or([0.0; 2]),
                }
            }));
        let mut levels_info = [MeshLod::default(); MAX_LODS];
        let mut next = upload.first_index;
        for (k, (indices, error)) in core::iter::once((data.indices, 0.0))
            .chain(lods.iter().map(|l| (l.indices, l.error.max(0.0))))
            .enumerate()
        {
            self.staged_indices.extend_from_slice(indices);
            levels_info[k] = MeshLod {
                first_index: next,
                index_count: indices.len() as u32,
                error,
            };
            next += indices.len() as u32;
        }
        self.uploads.push(upload);

        let info = MeshInfo {
            first_vertex: upload.first_vertex,
            vertex_count,
            first_index: upload.first_index,
            index_count: data.indices.len() as u32,
            min,
            max,
            lods: levels_info,
            lod_count: 1 + lods.len() as u32,
            revision: 0,
        };
        let index = match self.free_slots.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    mesh: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.mesh = Some((info, vertices, indices));
        Ok(MeshId {
            index,
            generation: slot.generation,
        })
    }

    /// Removes a mesh. Draws of it in later frames draw nothing; its ranges are reused once
    /// the frames up to `frame_number` (the current one) have finished.
    pub fn remove_mesh(&mut self, id: MeshId, frame_number: u64) {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return;
        };
        if slot.generation != id.generation {
            return;
        }
        if let Some((_, vertices, indices)) = slot.mesh.take() {
            self.retired
                .push((frame_number, Retired::Ranges(vertices, indices)));
        }
        slot.generation = slot.generation.wrapping_add(1);
        self.free_slots.push(id.index);
    }

    /// Where a mesh lives; `None` for stale ids.
    pub fn mesh(&self, id: MeshId) -> Option<&MeshInfo> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.mesh.as_ref().map(|(info, _, _)| info)
    }

    /// Replaces a mesh's object-space box (stale ids are ignored): for meshes whose
    /// vertices the GPU rewrites, like skinned meshes (`crate::skinning`). Culling and the
    /// draw order use it from the next frame on.
    pub fn set_bounds(&mut self, id: MeshId, min: Vec3, max: Vec3) {
        if let Some(slot) = self.slots.get_mut(id.index as usize)
            && slot.generation == id.generation
            && let Some((info, _, _)) = slot.mesh.as_mut()
        {
            info.min = min;
            info.max = max;
        }
    }

    /// Records that the GPU gives a mesh another shape this frame (stale ids are ignored):
    /// for meshes whose vertices it rewrites, like skinned meshes (`crate::skinning`).
    /// Cached shadows of local lights in its reach are drawn again (ADR 0071).
    pub fn mark_deformed(&mut self, id: MeshId) {
        if let Some(slot) = self.slots.get_mut(id.index as usize)
            && slot.generation == id.generation
            && let Some((info, _, _)) = slot.mesh.as_mut()
        {
            info.revision = info.revision.wrapping_add(1);
        }
    }

    /// Live meshes.
    pub fn mesh_count(&self) -> usize {
        self.slots.len() - self.free_slots.len()
    }

    /// Vertices and indices in use, and the pool's capacity in both.
    pub fn usage(&self) -> (u64, u64, u64, u64) {
        (
            self.vertex_ranges.used(),
            self.vertex_ranges.capacity(),
            self.index_ranges.used(),
            self.index_ranges.capacity(),
        )
    }

    /// Frees what the finished frames no longer use, grows the buffers if meshes were
    /// added beyond their capacity, copies the added meshes, and imports the buffers into
    /// `fg` for the frame's draws. Call once per frame, before the passes that draw.
    /// Allocates only on frames that add meshes.
    pub fn begin_frame<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        allocator: &mut Allocator,
        frame_number: u64,
    ) -> Result<GeometryFrame<'g>, RhiError> {
        engine_profiler::zone!("render::GeometryPool::begin_frame");
        self.collect_retired(allocator, frame_number);

        // Grow: new buffers as large as the range allocators now are, the old contents
        // copied into them.
        let vertex_capacity = self.vertex_ranges.capacity();
        let index_capacity = self.index_ranges.capacity();
        let mut grown: [Option<(PoolBuffer, Buffer)>; 3] = [None, None, None];
        if vertex_capacity * POSITION_BYTES > self.positions.size {
            for (k, bytes) in [(0, POSITION_BYTES), (1, ATTRIBUTE_BYTES)] {
                let buffer = PoolBuffer::new(
                    device,
                    allocator,
                    vertex_capacity * bytes,
                    vk::BufferUsageFlags::empty(),
                )?;
                let target = if k == 0 {
                    &mut self.positions
                } else {
                    &mut self.attributes
                };
                let mut old = core::mem::replace(target, buffer);
                let handle = old.import(fg, "geometry (old)", BufferState::idle());
                grown[k] = Some((old, handle));
            }
        }
        if index_capacity * INDEX_BYTES > self.indices.size {
            let buffer = PoolBuffer::new(
                device,
                allocator,
                index_capacity * INDEX_BYTES,
                vk::BufferUsageFlags::INDEX_BUFFER,
            )?;
            let mut old = core::mem::replace(&mut self.indices, buffer);
            let handle = old.import(fg, "geometry indices (old)", BufferState::idle());
            grown[2] = Some((old, handle));
        }
        let positions = self.positions.import(fg, "geometry positions", VERTEX_READ);
        let attributes = self
            .attributes
            .import(fg, "geometry attributes", VERTEX_READ);
        let indices = self.indices.import(fg, "geometry indices", INDEX_READ);
        let targets = [positions, attributes, indices];
        for (k, grown) in grown.iter_mut().enumerate() {
            let Some((old, handle)) = grown.take() else {
                continue;
            };
            let (source, target, size) = (handle, targets[k], old.size);
            fg.add_pass("geometry grow")
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

        if !self.uploads.is_empty() {
            self.record_uploads(fg, allocator, frame_number, targets)?;
        }
        Ok(GeometryFrame {
            pool: self,
            positions,
            attributes,
            indices,
        })
    }

    /// Copies the staged meshes into the pool through a staging buffer freed with the
    /// frame.
    fn record_uploads(
        &mut self,
        fg: &mut FrameGraph<'_>,
        allocator: &mut Allocator,
        frame_number: u64,
        [positions, attributes, indices]: [Buffer; 3],
    ) -> Result<(), RhiError> {
        let position_bytes = self.staged_positions.len() as u64 * POSITION_BYTES;
        let attribute_bytes = self.staged_attributes.len() as u64 * ATTRIBUTE_BYTES;
        let index_bytes = self.staged_indices.len() as u64 * INDEX_BYTES;
        let total = position_bytes + attribute_bytes + index_bytes;
        let (buffer, mut allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(total.max(16))
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )?;
        if let Some(target) = allocation.mapped_slice_mut() {
            // SAFETY: the staged arrays are plain `repr(C)` data; the byte views cover
            // exactly their elements.
            let bytes = |ptr: *const u8, len: u64| unsafe {
                core::slice::from_raw_parts(ptr, len as usize)
            };
            let (a, rest) = target.split_at_mut(position_bytes as usize);
            a.copy_from_slice(bytes(self.staged_positions.as_ptr().cast(), position_bytes));
            let (b, c) = rest.split_at_mut(attribute_bytes as usize);
            b.copy_from_slice(bytes(
                self.staged_attributes.as_ptr().cast(),
                attribute_bytes,
            ));
            c[..index_bytes as usize]
                .copy_from_slice(bytes(self.staged_indices.as_ptr().cast(), index_bytes));
        }
        let region = |src: u64, dst: u64, size: u64| vk::BufferCopy {
            src_offset: src,
            dst_offset: dst,
            size,
        };
        let mut position_regions = Vec::with_capacity(self.uploads.len());
        let mut attribute_regions = Vec::with_capacity(self.uploads.len());
        let mut index_regions = Vec::with_capacity(self.uploads.len());
        for u in &self.uploads {
            let (sv, dv, n) = (
                u64::from(u.staged_vertex),
                u64::from(u.first_vertex),
                u64::from(u.vertex_count),
            );
            if n > 0 {
                position_regions.push(region(
                    sv * POSITION_BYTES,
                    dv * POSITION_BYTES,
                    n * POSITION_BYTES,
                ));
                attribute_regions.push(region(
                    position_bytes + sv * ATTRIBUTE_BYTES,
                    dv * ATTRIBUTE_BYTES,
                    n * ATTRIBUTE_BYTES,
                ));
            }
            if u.index_count > 0 {
                index_regions.push(region(
                    position_bytes + attribute_bytes + u64::from(u.staged_index) * INDEX_BYTES,
                    u64::from(u.first_index) * INDEX_BYTES,
                    u64::from(u.index_count) * INDEX_BYTES,
                ));
            }
        }
        let staging = fg.import_buffer(
            "geometry staging",
            ImportedBuffer {
                buffer,
                size: total.max(16),
                address: 0,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        fg.add_pass("geometry upload")
            .read(staging, Access::TransferRead)
            .write(positions, Access::TransferWrite)
            .write(attributes, Access::TransferWrite)
            .write(indices, Access::TransferWrite)
            .execute(move |ctx| {
                let source = ctx.buffer(staging);
                // SAFETY: recording outside a rendering scope; every region lies inside
                // its buffer (ranges come from the allocators, sized to the buffers).
                unsafe {
                    for (target, regions) in [
                        (positions, &position_regions),
                        (attributes, &attribute_regions),
                        (indices, &index_regions),
                    ] {
                        if !regions.is_empty() {
                            ctx.device.cmd_copy_buffer(
                                ctx.cmd,
                                source,
                                ctx.buffer(target),
                                regions,
                            );
                        }
                    }
                }
            });
        self.retired.push((
            frame_number,
            Retired::Buffer(PoolBuffer {
                buffer,
                allocation: Some(allocation),
                address: 0,
                size: total,
                state: BufferState::idle(),
            }),
        ));
        self.staged_positions.clear();
        self.staged_attributes.clear();
        self.staged_indices.clear();
        self.uploads.clear();
        Ok(())
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
                Retired::Ranges(vertices, indices) => {
                    self.vertex_ranges.free(vertices);
                    self.index_ranges.free(indices);
                }
                // SAFETY: the frames that used it have finished.
                Retired::Buffer(mut buffer) => unsafe { buffer.destroy(allocator) },
            }
        }
    }

    /// Frees the buffers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the pool (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.positions.destroy(allocator);
            self.attributes.destroy(allocator);
            self.indices.destroy(allocator);
            for (_, retired) in self.retired.drain(..) {
                if let Retired::Buffer(mut buffer) = retired {
                    buffer.destroy(allocator);
                }
            }
        }
    }
}

/// A range of `size` units, growing the allocator when it is full: to twice its capacity,
/// or more if the request needs it (the buffers follow in the next frame).
fn reserve(ranges: &mut RangeAllocator, size: u64) -> Result<Range, MeshError> {
    if let Some(range) = ranges.allocate(size, 1) {
        return Ok(range);
    }
    let capacity = ranges.capacity();
    let wanted = (capacity * 2)
        .max(capacity + size.max(1))
        .min(u64::from(u32::MAX));
    ranges.grow(wanted);
    ranges.allocate(size, 1).ok_or(MeshError::PoolFull)
}

/// The pool's buffers in one frame's graph, for the passes that draw from it.
#[derive(Debug, Clone, Copy)]
pub struct GeometryFrame<'g> {
    pool: &'g GeometryPool,
    /// Positions (`[f32; 3]` per vertex).
    pub positions: Buffer,
    /// [`VertexAttributes`] per vertex.
    pub attributes: Buffer,
    /// 32-bit indices.
    pub indices: Buffer,
}

impl GeometryFrame<'_> {
    /// Where a mesh lives; `None` for stale ids.
    pub fn mesh(&self, id: MeshId) -> Option<&MeshInfo> {
        self.pool.mesh(id)
    }

    /// Device address of the positions (three floats per vertex).
    pub fn position_address(&self) -> vk::DeviceAddress {
        self.pool.positions.address
    }

    /// Device address of the [`VertexAttributes`].
    pub fn attribute_address(&self) -> vk::DeviceAddress {
        self.pool.attributes.address
    }

    /// Declares that a pass reads the pool: vertex shaders the vertex buffers, the input
    /// assembler the indices.
    pub fn read<'a, 'g>(
        &self,
        pass: crate::graph::PassBuilder<'a, 'g>,
    ) -> crate::graph::PassBuilder<'a, 'g> {
        pass.read(self.positions, Access::StorageRead(ShaderStages::VERTEX))
            .read(self.attributes, Access::StorageRead(ShaderStages::VERTEX))
            .read(self.indices, Access::IndexBuffer)
    }

    /// Declares that a pass reads only the positions and the indices (depth-only passes).
    pub fn read_positions<'a, 'g>(
        &self,
        pass: crate::graph::PassBuilder<'a, 'g>,
    ) -> crate::graph::PassBuilder<'a, 'g> {
        pass.read(self.positions, Access::StorageRead(ShaderStages::VERTEX))
            .read(self.indices, Access::IndexBuffer)
    }

    /// Binds the index buffer for indexed draws.
    ///
    /// # Safety
    ///
    /// `ctx` records a pass that declared [`Self::read`] or [`Self::read_positions`].
    pub unsafe fn bind_indices(&self, ctx: &PassContext<'_>) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            ctx.device.cmd_bind_index_buffer(
                ctx.cmd,
                ctx.buffer(self.indices),
                0,
                vk::IndexType::UINT32,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic generator (xorshift64*).
    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
        }
        fn direction(&mut self) -> Vec3 {
            loop {
                let v = Vec3::new(self.unit(), self.unit(), self.unit()) * 2.0 - 1.0;
                let l = v.length();
                if l > 0.1 && l <= 1.0 {
                    return v / l;
                }
            }
        }
    }

    #[test]
    fn octahedral_error_is_below_three_thousandths_of_a_degree() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut worst = 0.0f64;
        for i in 0..200_000 {
            // Random directions, plus the axes and octant boundaries.
            let v = match i {
                0 => Vec3::X,
                1 => Vec3::NEG_Z,
                2 => Vec3::new(1.0, -1.0, 0.0).normalize(),
                3 => Vec3::new(0.0, 1.0, -1e-7).normalize(),
                _ => rng.direction(),
            };
            let d = Vec3::from(decode_octahedral(encode_octahedral(v.to_array())));
            worst = worst.max(angle(d, v).to_degrees());
        }
        println!("octahedral 2 × 16 bits: worst error {worst:.5}°");
        assert!(worst < 0.003, "worst error {worst}°");
        // Degenerate input decodes to +Z.
        for bad in [[0.0; 3], [f32::NAN, 0.0, 0.0], [f32::INFINITY, 1.0, 0.0]] {
            assert_eq!(decode_octahedral(encode_octahedral(bad)), [0.0, 0.0, 1.0]);
        }
    }

    #[test]
    fn tangent_sign_survives_encoding() {
        let mut rng = Rng(7);
        for _ in 0..10_000 {
            let t = rng.direction();
            for sign in [1.0f32, -1.0] {
                let (d, s) = decode_tangent(encode_tangent(t.extend(sign).to_array()));
                assert_eq!(s, sign);
                let error = angle(Vec3::from(d), t).to_degrees();
                assert!(error < 0.01, "{error}°");
            }
        }
    }

    #[test]
    fn made_up_tangents_are_perpendicular_units() {
        let mut rng = Rng(11);
        for _ in 0..10_000 {
            let n = rng.direction();
            let t = any_tangent(n);
            assert!(
                (t.length() - 1.0).abs() < 1e-4 && t.dot(n).abs() < 1e-4,
                "{n} {t}"
            );
        }
        let t = any_tangent(Vec3::NEG_Z);
        assert!((t.length() - 1.0).abs() < 1e-5 && t.z.abs() < 1e-5);
    }

    #[test]
    fn cube_faces_are_consistent() {
        let cube = cube();
        assert_eq!((cube.positions.len(), cube.indices.len()), (24, 36));
        for tri in cube.indices.chunks(3) {
            let p = |i: u32| Vec3::from(cube.positions[i as usize]);
            let n = Vec3::from(cube.normals[tri[0] as usize]);
            let face = (p(tri[1]) - p(tri[0])).cross(p(tri[2]) - p(tri[0]));
            assert!(
                face.normalize().dot(n) > 0.999,
                "counter-clockwise front faces"
            );
            let [tx, ty, tz, w] = cube.tangents[tri[0] as usize];
            let t = Vec3::new(tx, ty, tz);
            assert!(t.dot(n).abs() < 1e-6);
            // The bitangent (sign · n × t) points towards decreasing v.
            let b = n.cross(t) * w;
            let uv = |i: u32| cube.uvs[i as usize];
            let (e1, e2) = (p(tri[1]) - p(tri[0]), p(tri[2]) - p(tri[0]));
            let (dv1, dv2) = (uv(tri[1])[1] - uv(tri[0])[1], uv(tri[2])[1] - uv(tri[0])[1]);
            assert!(b.dot(e1) * dv1 <= 1e-6 && b.dot(e2) * dv2 <= 1e-6);
        }
    }
}
