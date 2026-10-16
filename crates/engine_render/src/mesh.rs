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
//!   adds it). A range of them may end with its levels' [`Meshlet`]s (ADR 0085), 32-byte
//!   aligned after the indices: what the cull pass reads to draw a level by meshlet.
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
//!
//! **Streamed meshes** (ADR 0076): [`GeometryPool::add_streamed_mesh`] adds a mesh with
//! only its coarsest detail level; its vertices are ordered so every level uses a prefix
//! of them (`engine_assets::order_for_streaming`). [`GeometryPool::upload_mesh_level`]
//! adds the next finer level: a vertex range of the larger prefix, the old prefix copied
//! into it on the GPU, the new vertices and the level's indices uploaded;
//! [`GeometryPool::evict_mesh_levels`] drops the finest levels the same way. Levels not
//! resident draw the finest resident one: their entries in [`MeshInfo::lods`] repeat its
//! indices, so the culling pass, which picks a level per instance by screen size, needs
//! no change.

use crate::graph::{
    Access, Buffer, BufferState, FrameGraph, ImportedBuffer, PassContext, ShaderStages,
};
use engine_core::math::{Mat4, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Device, MemoryLocation, Range, RangeAllocator, RhiError};

/// Vertex attributes besides the position, as the shaders read them (16 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
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

/// Appends the encoded attributes of `data`'s vertices (normals, tangents or tangents
/// made up from the normals, texture coordinates) as the pool stores them.
pub fn encode_attributes(data: &MeshData<'_>, out: &mut Vec<VertexAttributes>) {
    out.extend((0..data.positions.len()).map(|v| {
        let normal = data.normals.get(v).copied().unwrap_or([0.0, 0.0, 1.0]);
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
    /// The triangles' meshlets (ADR 0085): consecutive ranges of `indices`, in order, or
    /// none (the level is drawn whole).
    pub meshlets: &'a [Meshlet],
}

/// A meshlet of a detail level (ADR 0085), as the cull pass reads it: a cluster of the
/// level's triangles with its bounding sphere and normal cone. The cook makes them
/// (`engine_assets::meshlet`, the same 32 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Meshlet {
    /// Bounding sphere in object space: center.
    pub center: [f32; 3],
    /// And radius.
    pub radius: f32,
    /// Normal cone: axis (x, y, z) and cutoff as signed 8-bit values over 127. Seen from
    /// `eye`, the meshlet faces away when `dot(center − eye, axis) ≥ cutoff · |center −
    /// eye| + radius`. A cutoff of 127 means no cone (normals over more than a half space):
    /// the axis is then zero, and the test must be skipped.
    pub cone: [i8; 4],
    /// Its first index, relative to its level's.
    pub first_index: u32,
    /// Its triangles.
    pub triangles: u32,
    /// 0.
    pub reserved: u32,
}

const _: () = assert!(size_of::<Meshlet>() == 32);

/// 32-bit words of a [`Meshlet`] in the index buffer.
const MESHLET_WORDS: u64 = (size_of::<Meshlet>() / 4) as u64;

/// Whether `meshlets` cover a level of `index_count` indices exactly, in order, each with
/// at least one triangle; no meshlets cover any level (it is drawn whole).
fn meshlets_cover(meshlets: &[Meshlet], index_count: usize) -> bool {
    if meshlets.is_empty() {
        return true;
    }
    let mut next = 0u64;
    for m in meshlets {
        if u64::from(m.first_index) != next || m.triangles == 0 {
            return false;
        }
        next += u64::from(m.triangles) * 3;
    }
    next == index_count as u64
}

/// Words of an index range holding `indices` indices and `meshlets` meshlets: the
/// indices, then the meshlets from the next 32-byte boundary (the shaders index the
/// buffer as meshlets).
fn range_words(indices: u64, meshlets: u64) -> u64 {
    if meshlets == 0 {
        indices
    } else {
        indices.next_multiple_of(MESHLET_WORDS) + meshlets * MESHLET_WORDS
    }
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
            meshlets: &[],
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
    /// A streamed mesh's level that does not follow its resident ones, or a mesh that is
    /// not streamed.
    BadLevel,
    /// The mesh's last change has not reached the GPU yet ([`GeometryPool::begin_frame`]):
    /// try again next frame.
    Busy,
    /// A level's meshlets do not cover its triangles, consecutively and in order.
    BadMeshlets,
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
            Self::BadLevel => write!(f, "detail level does not follow the resident ones"),
            Self::Busy => write!(f, "the mesh's last change is not on the GPU yet"),
            Self::BadMeshlets => write!(f, "meshlets do not cover their level's triangles"),
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
    /// Where its meshlets start in the index buffer, in 32-bit words (32-byte aligned: a
    /// multiple of a meshlet's 8 words).
    pub first_meshlet: u32,
    /// Its meshlets (ADR 0085; 0: it is drawn whole).
    pub meshlet_count: u32,
}

/// A simplified level added with a mesh ([`GeometryPool::add_mesh_with_lods`]): triangles
/// over the mesh's own vertices, and their error in object-space units.
#[derive(Debug, Clone, Copy)]
pub struct LodIndices<'a> {
    /// Triangles, three indices each, into the mesh's vertices.
    pub indices: &'a [u32],
    /// Geometric error against the full mesh (object space).
    pub error: f32,
    /// The level's meshlets, as [`MeshData::meshlets`].
    pub meshlets: &'a [Meshlet],
}

/// A mesh in a [`GeometryPool`]. Ids of removed meshes are stale: lookups give `None`.
/// Saves hold it as it is (ADR 0104): the same while a game adds its meshes in the same
/// order.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    bytemuck::Pod,
    bytemuck::Zeroable,
    engine_reflect::Reflect,
)]
#[repr(C)]
pub struct MeshId {
    index: u32,
    generation: u32,
}

impl MeshId {
    /// Its slot in the pool: stable while the mesh lives, reused after it is removed.
    pub fn slot(self) -> u32 {
        self.index
    }

    /// An id from a slot and generation, for tests of code that keys off ids.
    #[doc(hidden)]
    pub fn from_parts(slot: u32, generation: u32) -> Self {
        Self {
            index: slot,
            generation,
        }
    }
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
        // Acceleration structures are built from the pool (ADR 0087).
        let rays = if device.ray_query().is_some() {
            vk::BufferUsageFlags::ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_KHR
        } else {
            vk::BufferUsageFlags::empty()
        };
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(
                usage
                    | rays
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

/// A detail level of a streamed mesh ([`GeometryPool::add_streamed_mesh`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StreamedLevel {
    /// Its indices.
    pub index_count: u32,
    /// Its error in object-space units (0 for the full mesh).
    pub error: f32,
    /// The vertices it uses: a prefix of the mesh's.
    pub vertices: u32,
}

/// A streamed mesh's levels and their index ranges.
#[derive(Debug)]
struct Streamed {
    levels: [StreamedLevel; MAX_LODS],
    ranges: [Option<Range>; MAX_LODS],
    /// Meshlets of each resident level (in its range after its indices).
    meshlets: [u32; MAX_LODS],
    count: u32,
    /// The finest resident level.
    resident: u32,
}

#[derive(Debug)]
struct SlotMesh {
    info: MeshInfo,
    vertices: Range,
    /// The indices of every level, or `None` for a streamed mesh (per level).
    indices: Option<Range>,
    streamed: Option<Box<Streamed>>,
    /// Data waits for the next `begin_frame` (vertex moves need it on the GPU).
    pending: bool,
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    mesh: Option<SlotMesh>,
}

/// A copy of a streamed mesh's vertex prefix into its new range (`begin_frame`).
#[derive(Debug, Clone, Copy)]
struct Move {
    from: Range,
    to: u32,
    count: u32,
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
    /// A vertex range moved away from, or an evicted level's indices.
    Vertices(Range),
    Indices(Range),
    /// A buffer replaced by a larger one, or a staging buffer.
    Buffer(PoolBuffer),
}

/// A [`GeometryPool`]'s positions and indices as Vulkan buffers, for libraries that read
/// the meshes themselves (Brixelizer, ADR 0090). They change when the pool grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawGeometry {
    /// Positions, three floats (12 bytes) per vertex.
    pub positions: vk::Buffer,
    /// Bytes of `positions`.
    pub positions_size: u64,
    /// 32-bit indices, relative to each mesh's first vertex.
    pub indices: vk::Buffer,
    /// Bytes of `indices`.
    pub indices_size: u64,
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
    /// Streamed meshes' vertex prefixes to copy into their new ranges.
    moves: Vec<Move>,
    /// Slots with data waiting for `begin_frame`.
    pending_slots: Vec<u32>,
    /// Bytes of streamed meshes' vertices and indices in the pool.
    streamed_bytes: u64,
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
            moves: Vec::new(),
            pending_slots: Vec::new(),
            streamed_bytes: 0,
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
        let meshlets = || core::iter::once(data.meshlets).chain(lods.iter().map(|l| l.meshlets));
        if levels()
            .zip(meshlets())
            .any(|(indices, meshlets)| !meshlets_cover(meshlets, indices.len()))
        {
            return Err(MeshError::BadMeshlets);
        }
        let vertex_count = u32::try_from(vertex_count).map_err(|_| MeshError::PoolFull)?;
        let total_indices: usize = levels().map(<[u32]>::len).sum();
        let total_meshlets: usize = meshlets().map(<[Meshlet]>::len).sum();
        let words = range_words(total_indices as u64, total_meshlets as u64);
        let index_count = u32::try_from(words).map_err(|_| MeshError::PoolFull)?;
        if let Some(&index) = levels().flatten().find(|&&i| i >= vertex_count) {
            return Err(MeshError::IndexOutOfRange {
                index,
                vertex_count,
            });
        }
        let vertices = reserve(&mut self.vertex_ranges, u64::from(vertex_count))?;
        let indices = match reserve_words(&mut self.index_ranges, words, total_meshlets > 0) {
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
        // The levels' meshlets follow every level's indices, from a 32-byte boundary.
        let mut next_meshlet =
            upload.first_index + (total_indices as u64).next_multiple_of(MESHLET_WORDS) as u32;
        for (k, ((indices, error), meshlets)) in core::iter::once((data.indices, 0.0))
            .chain(lods.iter().map(|l| (l.indices, l.error.max(0.0))))
            .zip(meshlets())
            .enumerate()
        {
            self.staged_indices.extend_from_slice(indices);
            levels_info[k] = MeshLod {
                first_index: next,
                index_count: indices.len() as u32,
                error,
                first_meshlet: if meshlets.is_empty() { 0 } else { next_meshlet },
                meshlet_count: meshlets.len() as u32,
            };
            next += indices.len() as u32;
            next_meshlet += meshlets.len() as u32 * MESHLET_WORDS as u32;
        }
        if total_meshlets > 0 {
            let pad = (total_indices as u64).next_multiple_of(MESHLET_WORDS) - total_indices as u64;
            self.staged_indices
                .extend(core::iter::repeat_n(0, pad as usize));
            for meshlets in meshlets() {
                self.staged_indices
                    .extend_from_slice(bytemuck::cast_slice(meshlets));
            }
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
        slot.mesh = Some(SlotMesh {
            info,
            vertices,
            indices: Some(indices),
            streamed: None,
            pending: true,
        });
        self.pending_slots.push(index);
        Ok(MeshId {
            index,
            generation: slot.generation,
        })
    }

    /// Stages vertices `data` (positions and attributes), its indices and meshlets for
    /// upload to `first_vertex` and `first_index`.
    fn stage(&mut self, data: &MeshData<'_>, first_vertex: u32, first_index: u32) {
        let staged_vertex = self.staged_positions.len() as u32;
        self.staged_positions.extend_from_slice(data.positions);
        encode_attributes(data, &mut self.staged_attributes);
        self.stage_level(
            data.indices,
            data.meshlets,
            (first_vertex, data.positions.len() as u32, staged_vertex),
            first_index,
        );
    }

    /// Stages vertices already encoded (positions, [`VertexAttributes`]), indices and
    /// meshlets.
    fn stage_encoded(
        &mut self,
        positions: &[[f32; 3]],
        attributes: &[VertexAttributes],
        (indices, meshlets): (&[u32], &[Meshlet]),
        first_vertex: u32,
        first_index: u32,
    ) {
        let staged_vertex = self.staged_positions.len() as u32;
        self.staged_positions.extend_from_slice(positions);
        self.staged_attributes.extend_from_slice(attributes);
        self.stage_level(
            indices,
            meshlets,
            (first_vertex, positions.len() as u32, staged_vertex),
            first_index,
        );
    }

    /// Stages one level's range: its indices, then its meshlets from the next 32-byte
    /// boundary (`range_words`), with the vertices staged before it (`first`, `count`,
    /// where they start in the staged arrays).
    fn stage_level(
        &mut self,
        indices: &[u32],
        meshlets: &[Meshlet],
        (first_vertex, vertex_count, staged_vertex): (u32, u32, u32),
        first_index: u32,
    ) {
        let words = range_words(indices.len() as u64, meshlets.len() as u64);
        self.uploads.push(Upload {
            first_vertex,
            vertex_count,
            first_index,
            index_count: words as u32,
            staged_vertex,
            staged_index: self.staged_indices.len() as u32,
        });
        self.staged_indices.extend_from_slice(indices);
        if !meshlets.is_empty() {
            let pad = indices.len().next_multiple_of(MESHLET_WORDS as usize) - indices.len();
            self.staged_indices.extend(core::iter::repeat_n(0, pad));
            self.staged_indices
                .extend_from_slice(bytemuck::cast_slice(meshlets));
        }
    }

    /// Adds a mesh streamed by detail level (ADR 0076) with its coarsest level resident:
    /// `coarse` holds the vertices that level uses and its indices; `levels` describes every
    /// level from the full mesh to the coarsest (the last is `coarse`'s); `min`, `max` are
    /// the full mesh's bounds. Finer levels follow with [`Self::upload_mesh_level`].
    pub fn add_streamed_mesh(
        &mut self,
        coarse: &MeshData<'_>,
        levels: &[StreamedLevel],
        min: Vec3,
        max: Vec3,
    ) -> Result<MeshId, MeshError> {
        if levels.is_empty() || levels.len() > MAX_LODS {
            return Err(MeshError::TooManyLods);
        }
        let coarsest = levels.len() - 1;
        let vertex_count = coarse.positions.len();
        if vertex_count as u32 != levels[coarsest].vertices
            || coarse.indices.len() as u32 != levels[coarsest].index_count
            || coarse.normals.len() != vertex_count
            || !(coarse.uvs.is_empty() || coarse.uvs.len() == vertex_count)
            || !(coarse.tangents.is_empty() || coarse.tangents.len() == vertex_count)
            || levels.windows(2).any(|w| w[1].vertices > w[0].vertices)
        {
            return Err(MeshError::MismatchedLengths);
        }
        if !coarse.indices.len().is_multiple_of(3) {
            return Err(MeshError::NotTriangles);
        }
        if let Some(&index) = coarse.indices.iter().find(|&&i| i as usize >= vertex_count) {
            return Err(MeshError::IndexOutOfRange {
                index,
                vertex_count: vertex_count as u32,
            });
        }
        if !meshlets_cover(coarse.meshlets, coarse.indices.len()) {
            return Err(MeshError::BadMeshlets);
        }
        let words = range_words(coarse.indices.len() as u64, coarse.meshlets.len() as u64);
        let vertices = reserve(&mut self.vertex_ranges, vertex_count as u64)?;
        let indices =
            match reserve_words(&mut self.index_ranges, words, !coarse.meshlets.is_empty()) {
                Ok(range) => range,
                Err(e) => {
                    self.vertex_ranges.free(vertices);
                    return Err(e);
                }
            };
        self.stage(coarse, vertices.offset() as u32, indices.offset() as u32);
        let mut streamed = Streamed {
            levels: [StreamedLevel::default(); MAX_LODS],
            ranges: [None; MAX_LODS],
            meshlets: [0; MAX_LODS],
            count: levels.len() as u32,
            resident: coarsest as u32,
        };
        streamed.levels[..levels.len()].copy_from_slice(levels);
        streamed.ranges[coarsest] = Some(indices);
        streamed.meshlets[coarsest] = coarse.meshlets.len() as u32;
        self.streamed_bytes +=
            vertex_count as u64 * (POSITION_BYTES + ATTRIBUTE_BYTES) + words * INDEX_BYTES;
        let mut info = MeshInfo {
            first_vertex: vertices.offset() as u32,
            vertex_count: vertex_count as u32,
            first_index: 0,
            index_count: 0,
            min,
            max,
            lods: [MeshLod::default(); MAX_LODS],
            lod_count: levels.len() as u32,
            revision: 0,
        };
        streamed_lods(&mut info, &streamed);
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
        slot.mesh = Some(SlotMesh {
            info,
            vertices,
            indices: None,
            streamed: Some(Box::new(streamed)),
            pending: true,
        });
        self.pending_slots.push(index);
        Ok(MeshId {
            index,
            generation: slot.generation,
        })
    }

    /// A streamed mesh's finest resident level and its level count; `None` for stale ids
    /// and meshes that are not streamed.
    pub fn mesh_residency(&self, id: MeshId) -> Option<(u32, u32)> {
        let m = self.slot_mesh(id)?;
        let s = m.streamed.as_ref()?;
        Some((s.resident, s.count))
    }

    /// Bytes of streamed meshes' resident vertices and indices (for the GPU budget).
    pub fn streamed_bytes(&self) -> u64 {
        self.streamed_bytes
    }

    fn slot_mesh(&self, id: MeshId) -> Option<&SlotMesh> {
        let slot = self.slots.get(id.index as usize)?;
        (slot.generation == id.generation)
            .then_some(slot.mesh.as_ref())
            .flatten()
    }

    /// Adds the next finer level of a streamed mesh (`level` = its finest resident − 1):
    /// `data` holds the vertices it adds to the coarser levels' (positions and attributes)
    /// and its indices over all of them. Reaches the GPU in the next
    /// [`Self::begin_frame`]; draws use it from then on. [`MeshError::Busy`] while the
    /// mesh's last change is not on the GPU yet.
    pub fn upload_mesh_level(
        &mut self,
        id: MeshId,
        level: u32,
        data: &MeshData<'_>,
    ) -> Result<(), MeshError> {
        if data.normals.len() != data.positions.len() {
            return Err(MeshError::MismatchedLengths);
        }
        let first_vertex = self.grow_level(
            id,
            level,
            data.positions.len() as u32,
            data.indices,
            data.meshlets,
        )?;
        self.stage(data, first_vertex.0, first_vertex.1);
        Ok(())
    }

    /// [`Self::upload_mesh_level`] with the vertices already encoded as the pool stores
    /// them ([`encode_attributes`]): a copy on the frame's thread, the encoding done where
    /// the data was read (the streaming thread, ADR 0076). `meshlets` are the level's, as
    /// [`MeshData::meshlets`].
    pub fn upload_mesh_level_encoded(
        &mut self,
        id: MeshId,
        level: u32,
        positions: &[[f32; 3]],
        attributes: &[VertexAttributes],
        (indices, meshlets): (&[u32], &[Meshlet]),
    ) -> Result<(), MeshError> {
        if attributes.len() != positions.len() {
            return Err(MeshError::MismatchedLengths);
        }
        let (first_vertex, first_index) =
            self.grow_level(id, level, positions.len() as u32, indices, meshlets)?;
        self.stage_encoded(
            positions,
            attributes,
            (indices, meshlets),
            first_vertex,
            first_index,
        );
        Ok(())
    }

    /// Makes room for level `level` of a streamed mesh adding `added` vertices,
    /// `indices` and `meshlets`: a new vertex range (the old prefix moved into it in
    /// `begin_frame`) and an index range. Returns where the new vertices and the indices
    /// go.
    fn grow_level(
        &mut self,
        id: MeshId,
        level: u32,
        added: u32,
        indices: &[u32],
        meshlets: &[Meshlet],
    ) -> Result<(u32, u32), MeshError> {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return Err(MeshError::BadLevel);
        };
        let Some(m) = slot
            .mesh
            .as_mut()
            .filter(|_| slot.generation == id.generation)
        else {
            return Err(MeshError::BadLevel);
        };
        let Some(s) = m.streamed.as_mut() else {
            return Err(MeshError::BadLevel);
        };
        if m.pending {
            return Err(MeshError::Busy);
        }
        if level + 1 != s.resident {
            return Err(MeshError::BadLevel);
        }
        let old = s.levels[s.resident as usize].vertices;
        let new = s.levels[level as usize].vertices;
        if old + added != new || indices.len() as u32 != s.levels[level as usize].index_count {
            return Err(MeshError::MismatchedLengths);
        }
        if let Some(&index) = indices.iter().find(|&&i| i >= new) {
            return Err(MeshError::IndexOutOfRange {
                index,
                vertex_count: new,
            });
        }
        if !meshlets_cover(meshlets, indices.len()) {
            return Err(MeshError::BadMeshlets);
        }
        let words = range_words(indices.len() as u64, meshlets.len() as u64);
        let vertices = reserve(&mut self.vertex_ranges, u64::from(new))?;
        let index_range = match reserve_words(&mut self.index_ranges, words, !meshlets.is_empty()) {
            Ok(range) => range,
            Err(e) => {
                self.vertex_ranges.free(vertices);
                return Err(e);
            }
        };
        let Some(m) = self.slots[id.index as usize].mesh.as_mut() else {
            unreachable!("looked up above");
        };
        let Some(s) = m.streamed.as_mut() else {
            unreachable!("looked up above");
        };
        self.moves.push(Move {
            from: m.vertices,
            to: vertices.offset() as u32,
            count: old,
        });
        m.vertices = vertices;
        s.ranges[level as usize] = Some(index_range);
        s.meshlets[level as usize] = meshlets.len() as u32;
        s.resident = level;
        m.info.first_vertex = vertices.offset() as u32;
        m.info.vertex_count = new;
        streamed_lods(&mut m.info, s);
        m.info.revision = m.info.revision.wrapping_add(1);
        m.pending = true;
        self.streamed_bytes +=
            u64::from(added) * (POSITION_BYTES + ATTRIBUTE_BYTES) + words * INDEX_BYTES;
        self.pending_slots.push(id.index);
        Ok((vertices.offset() as u32 + old, index_range.offset() as u32))
    }

    /// Drops the levels of a streamed mesh finer than `first`: their indices, and the
    /// vertices only they use (the rest are copied into a smaller range in the next
    /// [`Self::begin_frame`]); their memory is reused once the frames up to
    /// `frame_number` have finished. Draws use level `first` instead from then on.
    pub fn evict_mesh_levels(
        &mut self,
        id: MeshId,
        first: u32,
        frame_number: u64,
    ) -> Result<(), MeshError> {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return Err(MeshError::BadLevel);
        };
        let Some(m) = slot
            .mesh
            .as_mut()
            .filter(|_| slot.generation == id.generation)
        else {
            return Err(MeshError::BadLevel);
        };
        let Some(s) = m.streamed.as_mut() else {
            return Err(MeshError::BadLevel);
        };
        if m.pending {
            return Err(MeshError::Busy);
        }
        if first <= s.resident || first >= s.count {
            return Err(MeshError::BadLevel);
        }
        let kept = s.levels[first as usize].vertices;
        let Some(vertices) = self.vertex_ranges.allocate(u64::from(kept), 1) else {
            return Err(MeshError::PoolFull);
        };
        let Some(m) = self.slots[id.index as usize].mesh.as_mut() else {
            unreachable!("looked up above");
        };
        let Some(s) = m.streamed.as_mut() else {
            unreachable!("looked up above");
        };
        for k in s.resident..first {
            s.meshlets[k as usize] = 0;
            if let Some(range) = s.ranges[k as usize].take() {
                self.streamed_bytes -= range.size() * INDEX_BYTES;
                self.retired.push((frame_number, Retired::Indices(range)));
            }
        }
        let dropped = m.info.vertex_count - kept;
        self.streamed_bytes -= u64::from(dropped) * (POSITION_BYTES + ATTRIBUTE_BYTES);
        self.moves.push(Move {
            from: m.vertices,
            to: vertices.offset() as u32,
            count: kept,
        });
        m.vertices = vertices;
        s.resident = first;
        m.info.first_vertex = vertices.offset() as u32;
        m.info.vertex_count = kept;
        streamed_lods(&mut m.info, s);
        m.info.revision = m.info.revision.wrapping_add(1);
        m.pending = true;
        self.pending_slots.push(id.index);
        Ok(())
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
        if let Some(m) = slot.mesh.take() {
            match (m.indices, m.streamed) {
                (Some(indices), _) => {
                    self.retired
                        .push((frame_number, Retired::Ranges(m.vertices, indices)));
                }
                (None, Some(s)) => {
                    self.streamed_bytes -=
                        u64::from(m.info.vertex_count) * (POSITION_BYTES + ATTRIBUTE_BYTES);
                    self.retired
                        .push((frame_number, Retired::Vertices(m.vertices)));
                    for range in s.ranges.into_iter().flatten() {
                        self.streamed_bytes -= range.size() * INDEX_BYTES;
                        self.retired.push((frame_number, Retired::Indices(range)));
                    }
                }
                (None, None) => {
                    self.retired
                        .push((frame_number, Retired::Vertices(m.vertices)));
                }
            }
        }
        slot.generation = slot.generation.wrapping_add(1);
        self.free_slots.push(id.index);
    }

    /// The positions and indices as Vulkan buffers ([`RawGeometry`]).
    pub fn raw_buffers(&self) -> RawGeometry {
        RawGeometry {
            positions: self.positions.buffer,
            positions_size: self.positions.size,
            indices: self.indices.buffer,
            indices_size: self.indices.size,
        }
    }

    /// Where a mesh lives; `None` for stale ids.
    pub fn mesh(&self, id: MeshId) -> Option<&MeshInfo> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.mesh.as_ref().map(|m| &m.info)
    }

    /// Replaces a mesh's object-space box (stale ids are ignored): for meshes whose
    /// vertices the GPU rewrites, like skinned meshes (`crate::skinning`). Culling and the
    /// draw order use it from the next frame on.
    pub fn set_bounds(&mut self, id: MeshId, min: Vec3, max: Vec3) {
        if let Some(slot) = self.slots.get_mut(id.index as usize)
            && slot.generation == id.generation
            && let Some(m) = slot.mesh.as_mut()
        {
            m.info.min = min;
            m.info.max = max;
        }
    }

    /// Records that the GPU gives a mesh another shape this frame (stale ids are ignored):
    /// for meshes whose vertices it rewrites, like skinned meshes (`crate::skinning`).
    /// Cached shadows of local lights in its reach are drawn again (ADR 0071).
    pub fn mark_deformed(&mut self, id: MeshId) {
        if let Some(slot) = self.slots.get_mut(id.index as usize)
            && slot.generation == id.generation
            && let Some(m) = slot.mesh.as_mut()
        {
            m.info.revision = m.info.revision.wrapping_add(1);
        }
    }

    /// Live meshes.
    pub fn mesh_count(&self) -> usize {
        self.slots.len() - self.free_slots.len()
    }

    /// Makes room for `vertices` and `indices` beyond what the pool holds now: its buffers
    /// grow once, in the next [`Self::begin_frame`], instead of doubling on the frames that
    /// add meshes or levels past them. Growing makes device memory and copies on the frame's
    /// thread (4.1 ms on the Radeon iGPU for Sponza's levels, core review 2 §3.6): a streamed
    /// scene reserves its full detail as it opens.
    pub fn reserve(&mut self, vertices: u64, indices: u64) {
        for (ranges, more) in [
            (&mut self.vertex_ranges, vertices),
            (&mut self.index_ranges, indices),
        ] {
            let needed = (ranges.used() + more).min(u64::from(u32::MAX));
            if ranges.capacity() < needed {
                ranges.grow(needed);
            }
        }
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

        if !self.moves.is_empty() {
            self.record_moves(fg, frame_number, [positions, attributes]);
        }
        if !self.uploads.is_empty() {
            self.record_uploads(fg, allocator, frame_number, targets)?;
        }
        for index in self.pending_slots.drain(..) {
            if let Some(m) = self
                .slots
                .get_mut(index as usize)
                .and_then(|s| s.mesh.as_mut())
            {
                m.pending = false;
            }
        }
        Ok(GeometryFrame {
            pool: self,
            positions,
            attributes,
            indices,
        })
    }

    /// Copies streamed meshes' vertex prefixes into their new ranges (the same buffers;
    /// ranges never overlap) and retires the old ranges with the frame.
    fn record_moves(
        &mut self,
        fg: &mut FrameGraph<'_>,
        frame_number: u64,
        [positions, attributes]: [Buffer; 2],
    ) {
        let moves = core::mem::take(&mut self.moves);
        for m in &moves {
            self.retired.push((frame_number, Retired::Vertices(m.from)));
        }
        fg.add_pass("geometry move")
            .write(positions, Access::TransferWrite)
            .write(attributes, Access::TransferWrite)
            .execute(move |ctx| {
                let region = |m: &Move, bytes: u64| vk::BufferCopy {
                    src_offset: m.from.offset() * bytes,
                    dst_offset: u64::from(m.to) * bytes,
                    size: u64::from(m.count) * bytes,
                };
                let position_regions: Vec<_> = moves
                    .iter()
                    .filter(|m| m.count > 0)
                    .map(|m| region(m, POSITION_BYTES))
                    .collect();
                let attribute_regions: Vec<_> = moves
                    .iter()
                    .filter(|m| m.count > 0)
                    .map(|m| region(m, ATTRIBUTE_BYTES))
                    .collect();
                if position_regions.is_empty() {
                    return;
                }
                let (p, a) = (ctx.buffer(positions), ctx.buffer(attributes));
                // The copies read what earlier frames' uploads wrote (available since): make
                // it visible to this pass's reads, which the graph's barrier (for its
                // writes) does not.
                let barrier = vk::MemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ);
                // SAFETY: recording outside a rendering scope; source and destination
                // ranges come from the range allocator, inside the buffers and disjoint.
                unsafe {
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().memory_barriers(&[barrier]),
                    );
                    ctx.device.cmd_copy_buffer(ctx.cmd, p, p, &position_regions);
                    ctx.device
                        .cmd_copy_buffer(ctx.cmd, a, a, &attribute_regions);
                }
            });
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
                Retired::Vertices(vertices) => self.vertex_ranges.free(vertices),
                Retired::Indices(indices) => self.index_ranges.free(indices),
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

/// A streamed mesh's levels for drawing: resident ones their own indices, finer ones the
/// finest resident level's (with their own errors, so the culling pass picks the same
/// level it would and draws what is there).
fn streamed_lods(info: &mut MeshInfo, s: &Streamed) {
    let resident = s.resident as usize;
    let own = |k: usize| {
        s.ranges[k].map_or((0, 0, 0, 0), |r| {
            let (first, count, meshlets) =
                (r.offset() as u32, s.levels[k].index_count, s.meshlets[k]);
            let first_meshlet = if meshlets == 0 {
                0
            } else {
                first + count.next_multiple_of(MESHLET_WORDS as u32)
            };
            (first, count, first_meshlet, meshlets)
        })
    };
    for k in 0..s.count as usize {
        let (first_index, index_count, first_meshlet, meshlet_count) = own(k.max(resident));
        info.lods[k] = MeshLod {
            first_index,
            index_count,
            error: s.levels[k].error,
            first_meshlet,
            meshlet_count,
        };
    }
    info.first_index = info.lods[0].first_index;
    info.index_count = info.lods[0].index_count;
}

/// A range of `size` units, growing the allocator when it is full: to twice its capacity,
/// or more if the request needs it (the buffers follow in the next frame).
fn reserve(ranges: &mut RangeAllocator, size: u64) -> Result<Range, MeshError> {
    reserve_aligned(ranges, size, 1)
}

/// An index range of `words`, 32-byte aligned when it holds meshlets (they follow its
/// indices from a 32-byte boundary of the buffer).
fn reserve_words(
    ranges: &mut RangeAllocator,
    words: u64,
    meshlets: bool,
) -> Result<Range, MeshError> {
    reserve_aligned(ranges, words, if meshlets { MESHLET_WORDS } else { 1 })
}

/// [`reserve`] with an alignment, in units.
fn reserve_aligned(ranges: &mut RangeAllocator, size: u64, align: u64) -> Result<Range, MeshError> {
    if let Some(range) = ranges.allocate(size, align) {
        return Ok(range);
    }
    let capacity = ranges.capacity();
    let wanted = (capacity * 2)
        .max(capacity + size.max(1) + align)
        .min(u64::from(u32::MAX));
    ranges.grow(wanted);
    ranges.allocate(size, align).ok_or(MeshError::PoolFull)
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

    /// Device address of the indices (32-bit, relative to each mesh's first vertex).
    pub fn index_address(&self) -> vk::DeviceAddress {
        self.pool.indices.address
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

    fn meshlet(first_index: u32, triangles: u32) -> Meshlet {
        Meshlet {
            first_index,
            triangles,
            ..Meshlet::default()
        }
    }

    /// Meshlets must cover their level's triangles consecutively and in order; none
    /// covers any level. Their words follow the indices from a 32-byte boundary.
    #[test]
    fn meshlets_cover_their_level_and_follow_its_indices() {
        let level = [meshlet(0, 2), meshlet(6, 1)];
        assert!(meshlets_cover(&level, 9));
        assert!(meshlets_cover(&[], 9));
        assert!(!meshlets_cover(&level, 12), "short of the level");
        assert!(!meshlets_cover(&[meshlet(0, 1), meshlet(6, 1)], 6), "a gap");
        assert!(
            !meshlets_cover(&[meshlet(0, 0), meshlet(0, 3)], 9),
            "an empty one"
        );
        assert_eq!(range_words(9, 0), 9);
        assert_eq!(range_words(9, 2), 16 + 16);
        assert_eq!(range_words(16, 1), 16 + 8);
    }

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
