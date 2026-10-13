//! Streaming a cooked scene within the GPU memory budget (ADR 0076): the glue between a
//! scene's pack (`engine_assets`, ADR 0056), the streaming thread that reads it (ADR 0057)
//! and the renderer's pools and streaming policies (`engine_render::streaming`,
//! `engine_render::budget`).
//!
//! ```text
//!  feedback (GPU, a few frames late) ──► TextureStreamer: which levels, within the grant
//!                                              │ LevelRequest
//!                                              ▼
//!  pack ◄── StreamThread (I/O ring, zstd) ◄── staging ring span (host-cached)
//!                                              │ arrived
//!                                              ▼
//!                       TexturePool::upload_levels (copies within the frame budget)
//! ```
//!
//! [`SceneStreamer::open`] opens a pack's scene without its textures' levels: every texture
//! is created holding only its tail (its levels of at most 128 texels), which is requested
//! first; finer levels follow what the GPU samples. Meshes stored for streaming (pack
//! version 3) start with their coarsest detail level; finer ones follow the level the
//! culling pass draws each instance at (`MeshStreamer`), read into memory and uploaded
//! into the geometry pool. Call [`SceneStreamer::update`] once per frame before the frame's
//! graph, then the budget's [`GpuBudget::update`].

use engine_assets::pack::{Entry, EntryKind, PackError, PackStreamer, StreamThread, Streamed};
use engine_assets::{
    AlphaMode as CookedAlpha, CookedMaterial, CookedScene, MaterialTypeData, SceneLayout,
    TextureFormat as Stored, TextureSlot, TextureWrap, open_scene_streamed, read_mesh_level_entry,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Vec3};
use engine_platform::io::AlignedBuffer;
use engine_render::budget::GpuBudget;
use engine_render::feedback::TextureFeedback;
use engine_render::forward::{ForwardRenderer, MaterialTypeDesc};
use engine_render::material::MaterialTable;
use engine_render::material::{AlphaMode, MaterialDesc, TextureRef};
use engine_render::material::{MaterialId, MaterialParams, MaterialTypeId};
use engine_render::mesh::{
    GeometryPool, LodIndices, MeshData, MeshError, MeshId, StreamedLevel, VertexAttributes,
    encode_attributes,
};
use engine_render::streaming::{
    LevelRequest, LodView, MeshLevelRequest, MeshStreamer, MeshStreamerStats, StreamerSettings,
    StreamerStats, TextureStreamer, first_tail_level,
};
use engine_render::texture_pool::{
    StagingSpan, TextureError, TextureFormat, TextureId, TexturePool,
};
use engine_rhi::Allocator;
use engine_rhi::Wrap;
use std::collections::VecDeque;
use std::path::Path;

/// Why a scene could not be streamed.
#[derive(Debug)]
pub enum StreamError {
    /// The pack.
    Pack(PackError),
    /// A texture of the pool.
    Texture(TextureError),
    /// A mesh of the geometry pool.
    Mesh(MeshError),
    /// The budget has no room for another client.
    Budget,
}

impl core::fmt::Display for StreamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pack(e) => write!(f, "{e}"),
            Self::Texture(e) => write!(f, "{e}"),
            Self::Mesh(e) => write!(f, "{e}"),
            Self::Budget => write!(f, "the GPU budget has no room for another client"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<PackError> for StreamError {
    fn from(e: PackError) -> Self {
        Self::Pack(e)
    }
}

impl From<MeshError> for StreamError {
    fn from(e: MeshError) -> Self {
        Self::Mesh(e)
    }
}

impl From<TextureError> for StreamError {
    fn from(e: TextureError) -> Self {
        Self::Texture(e)
    }
}

/// The renderer's format of a stored texture format.
pub fn texture_format(format: Stored) -> TextureFormat {
    match format {
        Stored::Rgba8Srgb => TextureFormat::Rgba8Srgb,
        Stored::Rgba8Unorm => TextureFormat::Rgba8Unorm,
        Stored::Bc7Srgb => TextureFormat::Bc7Srgb,
        Stored::Bc7Unorm => TextureFormat::Bc7Unorm,
        Stored::Bc5Unorm => TextureFormat::Bc5Unorm,
    }
}

/// A cooked scene's material types (ADR 0092) registered with the forward renderer, their
/// pipelines made from the pack's SPIR-V; ids in the scene's order.
pub fn register_material_types(
    forward: &mut ForwardRenderer,
    types: &[MaterialTypeData],
) -> Vec<MaterialTypeId> {
    types
        .iter()
        .map(|t| {
            forward.register_material_type_precompiled(
                MaterialTypeDesc {
                    name: t.type_name.clone(),
                    module: t.module.clone().into(),
                    type_name: t.type_name.clone(),
                },
                &t.entries,
            )
        })
        .collect()
}

/// A cooked scene's materials added to `table`, in the scene's order: glTF ones, and the
/// materials of types (`types` from [`register_material_types`]) with their parameters,
/// their glTF records their fallbacks. Texture slots index `textures`.
pub fn add_materials(
    table: &mut MaterialTable,
    scene: &CookedScene,
    types: &[MaterialTypeId],
    textures: &[TextureId],
) -> Vec<MaterialId> {
    let wrap = |w: TextureWrap| match w {
        TextureWrap::Repeat => Wrap::Repeat,
        TextureWrap::MirroredRepeat => Wrap::MirroredRepeat,
        TextureWrap::ClampToEdge => Wrap::ClampToEdge,
    };
    scene
        .materials
        .iter()
        .enumerate()
        .map(|(k, m)| {
            let desc = material_desc(m, textures);
            let typed = scene
                .typed_materials
                .iter()
                .find(|t| t.material as usize == k)
                .and_then(|t| Some((t, *types.get(t.ty as usize)?)));
            match typed {
                Some((t, ty)) => table.add_typed(
                    ty,
                    desc,
                    Some(MaterialParams {
                        bytes: t.params.clone(),
                        textures: t
                            .textures
                            .iter()
                            .filter_map(|&(offset, slot)| {
                                Some((
                                    offset,
                                    TextureRef {
                                        texture: *textures.get(slot.texture as usize)?,
                                        wrap: slot.wrap.map(wrap),
                                    },
                                ))
                            })
                            .collect(),
                    }),
                ),
                None => table.add(desc),
            }
        })
        .collect()
}

/// The renderer's description of a cooked material whose texture slots index `textures`.
pub fn material_desc(m: &CookedMaterial, textures: &[TextureId]) -> MaterialDesc {
    let wrap = |w: TextureWrap| match w {
        TextureWrap::Repeat => Wrap::Repeat,
        TextureWrap::MirroredRepeat => Wrap::MirroredRepeat,
        TextureWrap::ClampToEdge => Wrap::ClampToEdge,
    };
    let find = |slot: Option<TextureSlot>| {
        let slot = slot?;
        Some(TextureRef {
            texture: *textures.get(slot.texture as usize)?,
            wrap: slot.wrap.map(wrap),
        })
    };
    MaterialDesc {
        base_color: m.base_color,
        base_color_texture: find(m.base_color_texture),
        metallic: m.metallic,
        roughness: m.roughness,
        metallic_roughness_texture: find(m.metallic_roughness_texture),
        normal_texture: find(m.normal_texture),
        normal_scale: m.normal_scale,
        occlusion_texture: find(m.occlusion_texture),
        occlusion_strength: m.occlusion_strength,
        emissive: m.emissive,
        emissive_texture: find(m.emissive_texture),
        alpha: match m.alpha {
            CookedAlpha::Mask(bits) => AlphaMode::Mask {
                cutoff: f32::from_bits(bits),
            },
            CookedAlpha::Opaque => AlphaMode::Opaque,
            CookedAlpha::Blend => AlphaMode::Blend,
        },
        double_sided: m.double_sided,
        subsurface: None,
    }
}

/// How a [`SceneStreamer`] reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneStreamSettings {
    /// Reads in flight on the I/O ring.
    pub depth: usize,
    /// Entries requested from the streaming thread at a time.
    pub capacity: usize,
    /// Decompression threads.
    pub workers: usize,
    /// The texture and mesh streamers' policy.
    pub textures: StreamerSettings,
    /// Vertices of mesh levels decoded and staged on the frame's thread per frame (at
    /// least one level each frame): a level that arrives beyond waits for the next frame.
    pub mesh_vertices_per_frame: u32,
}

impl Default for SceneStreamSettings {
    /// 16 reads in flight, 64 entries requested, 2 decompression threads, the default
    /// policy, 64k mesh vertices per frame (decoded on the streaming thread, a mesh level
    /// is a copy on the frame's thread).
    fn default() -> Self {
        Self {
            depth: 16,
            capacity: 64,
            workers: 2,
            textures: StreamerSettings::default(),
            mesh_vertices_per_frame: 64 << 10,
        }
    }
}

/// Bytes a mesh level entry takes once decoded in place ([`decode_mesh_level`]): a
/// 16-byte header, positions, encoded attributes, indices.
fn decoded_mesh_level_bytes(entry: &Entry) -> usize {
    let (vertices, indices) = (entry.meta[2] as usize, entry.meta[3] as usize);
    16 + vertices * (12 + 16) + indices * 4
}

/// Runs on the streaming thread on every entry it read: a mesh level entry is decoded and
/// its vertices encoded as the geometry pool stores them, in place (`[vertices, indices,
/// first vertex, level]`, positions, attributes, indices), so the frame's thread only
/// copies it.
fn decode_mesh_level(entry: &Entry, bytes: &mut [u8]) -> Result<(), PackError> {
    if entry.kind() != EntryKind::MeshLevel {
        return Ok(());
    }
    let invalid = |what: &str| PackError::Io(std::io::Error::other(what.to_owned()));
    let raw = bytes
        .get(..entry.bytes as usize)
        .ok_or_else(|| invalid("mesh level entry truncated"))?;
    let level = read_mesh_level_entry(entry.meta, raw)?;
    let mut attributes = Vec::with_capacity(level.positions.len());
    encode_attributes(
        &MeshData {
            positions: &level.positions,
            normals: &level.normals,
            tangents: &level.tangents,
            uvs: &level.uvs,
            indices: &level.indices,
        },
        &mut attributes,
    );
    let (n, i) = (level.positions.len(), level.indices.len());
    let needed = 16 + n * 28 + i * 4;
    let out = bytes
        .get_mut(..needed)
        .ok_or_else(|| invalid("mesh level target too small"))?;
    let header = [n as u32, i as u32, level.first_vertex, level.level];
    out[..16].copy_from_slice(bytemuck::cast_slice(&header));
    out[16..16 + n * 12].copy_from_slice(bytemuck::cast_slice(&level.positions));
    out[16 + n * 12..16 + n * 28].copy_from_slice(bytemuck::cast_slice(&attributes));
    out[16 + n * 28..].copy_from_slice(bytemuck::cast_slice(&level.indices));
    Ok(())
}

/// When a request was made: the frame and the time.
#[derive(Debug, Clone, Copy)]
struct Made {
    frame: u64,
    at: std::time::Instant,
}

/// What a read fills: a texture's levels in the staging ring, or a mesh level in memory.
#[derive(Debug)]
enum Target {
    Texture(StagingSpan, LevelRequest),
    Mesh(AlignedBuffer, MeshLevelRequest, u32),
}

/// A request not started yet.
#[derive(Debug, Clone, Copy)]
enum Waiting {
    Texture(LevelRequest),
    Mesh(MeshLevelRequest),
}

/// What a [`SceneStreamer`] has done, for overlays, benchmarks and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SceneStreamStats {
    /// The texture streamer's last frame.
    pub textures: StreamerStats,
    /// The mesh streamer's last frame.
    pub meshes: MeshStreamerStats,
    /// Texture levels and mesh levels read and uploaded so far.
    pub arrived: u64,
    /// Mesh levels among them.
    pub mesh_levels: u64,
    /// Vertices of mesh levels decoded and staged so far.
    pub mesh_vertices: u64,
    /// Time spent decoding and staging mesh levels on the frame's thread: in all, and the
    /// most in one frame (ms).
    pub mesh_upload_ms_sum: f64,
    /// The most in one frame.
    pub mesh_upload_ms_max: f64,
    /// Bytes read so far.
    pub bytes: u64,
    /// Requests waiting for room (staging or the streaming thread).
    pub waiting: usize,
    /// Reads or uploads failed so far (a level evicted while it was read counts).
    pub failed: u64,
    /// Request to arrival, in frames: the last.
    pub latency_frames: u64,
    /// The largest request-to-arrival latency so far, in frames.
    pub worst_latency_frames: u64,
    /// Request to arrival (uploaded to the pool), in milliseconds: the last.
    pub latency_ms: f64,
    /// The largest so far.
    pub worst_latency_ms: f64,
    /// The sum of every arrival's (for the mean over `arrived`).
    pub latency_ms_sum: f64,
}

/// Streams a cooked scene's textures and mesh detail levels (see the crate documentation).
#[derive(Debug)]
pub struct SceneStreamer {
    thread: StreamThread,
    layout: SceneLayout,
    textures: Vec<TextureId>,
    /// The scene texture of each pool slot (`u32::MAX`: none).
    by_slot: Vec<u32>,
    meshes: Vec<MeshId>,
    /// The scene mesh of each geometry slot (`u32::MAX`: none).
    mesh_by_slot: Vec<u32>,
    /// Instances of streamed meshes: the mesh and its transform.
    instances: Vec<(MeshId, Mat4)>,
    streamer: TextureStreamer,
    mesh_streamer: MeshStreamer,
    /// Requests not started yet, oldest first, with when they were made.
    waiting: VecDeque<(Waiting, Made)>,
    /// Reads started, by tag (`None`: a free tag), with when they were made.
    in_flight: Vec<Option<(Target, Made)>>,
    free_tags: Vec<u32>,
    /// Read buffers of mesh levels, kept for the next.
    buffers: Vec<AlignedBuffer>,
    requests: Vec<LevelRequest>,
    mesh_requests: Vec<MeshLevelRequest>,
    finished: Vec<Streamed>,
    /// Mesh levels read and not yet uploaded (within the frame's vertices).
    meshes_read: VecDeque<(AlignedBuffer, MeshLevelRequest, u32, Made)>,
    mesh_vertices_per_frame: u32,
    stats: SceneStreamStats,
}

impl SceneStreamer {
    /// Opens the scene of the pack at `path` (its materials and instances are in
    /// [`Self::layout`]), creates every texture in `pool` holding its tail, which is
    /// requested first, and adds every mesh to `geometry`: meshes stored for streaming
    /// with their coarsest level, the others whole ([`Self::meshes`]). Registers the
    /// texture and mesh streamers with `budget`.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        path: &Path,
        jobs: &ThreadPool,
        pool: &mut TexturePool,
        geometry: &mut GeometryPool,
        allocator: &mut Allocator,
        budget: &mut GpuBudget,
        settings: SceneStreamSettings,
    ) -> Result<Self, StreamError> {
        engine_profiler::zone!("streaming::SceneStreamer::open");
        let (pack, layout, _) = open_scene_streamed(path, jobs, settings.depth)?;
        let mut streamer =
            TextureStreamer::new(budget, settings.textures).ok_or(StreamError::Budget)?;
        let mut mesh_streamer =
            MeshStreamer::new(budget, settings.textures).ok_or(StreamError::Budget)?;
        let mut textures = Vec::with_capacity(layout.textures.len());
        for record in &layout.textures {
            let format = Stored::from_code(record.format)
                .ok_or_else(|| PackError::Io(std::io::Error::other("unknown texture format")))?;
            let tail = first_tail_level(record.width, record.height, record.levels);
            let id = pool.add_streamed(
                allocator,
                record.width,
                record.height,
                texture_format(format),
                record.levels,
                tail,
            )?;
            streamer.track(pool, id);
            textures.push(id);
        }
        let mut meshes = Vec::with_capacity(layout.scene.meshes.len());
        for (m, mesh) in layout.scene.meshes.iter().enumerate() {
            let data = MeshData {
                positions: &mesh.positions,
                normals: &mesh.normals,
                tangents: &mesh.tangents,
                uvs: &mesh.uvs,
                indices: &mesh.indices,
            };
            let (min, max) = (Vec3::from(mesh.min), Vec3::from(mesh.max));
            let id = match layout.mesh_streams.get(m).and_then(Option::as_ref) {
                Some(streamed) => {
                    let levels: Vec<StreamedLevel> = streamed
                        .levels
                        .iter()
                        .map(|l| StreamedLevel {
                            index_count: l.index_count,
                            error: l.error,
                            vertices: l.vertices,
                        })
                        .collect();
                    let id = geometry.add_streamed_mesh(&data, &levels, min, max)?;
                    mesh_streamer.track(id, &levels, min, max);
                    id
                }
                None => {
                    let lods: Vec<LodIndices<'_>> = mesh
                        .lods
                        .iter()
                        .map(|l| LodIndices {
                            indices: &l.indices,
                            error: l.error,
                        })
                        .collect();
                    geometry.add_mesh_with_lods(&data, &lods)?
                }
            };
            meshes.push(id);
        }
        // Room for every streamed mesh at full detail, so that the pool's buffers are made
        // once, now, instead of growing on the frame's thread as finer levels arrive (core
        // review 2 §3.6). A finer level's vertices go to a new range before the old one is
        // freed: twice the finest vertices, and every level's indices.
        let (mut vertices, mut indices) = (0u64, 0u64);
        for streamed in layout.mesh_streams.iter().flatten() {
            if let Some(finest) = streamed.levels.first() {
                vertices += 2 * u64::from(finest.vertices);
            }
            indices += streamed
                .levels
                .iter()
                .map(|l| u64::from(l.index_count))
                .sum::<u64>();
        }
        geometry.reserve(vertices, indices);
        let slots = |ids: &[u32]| {
            let mut by_slot = Vec::new();
            for (k, &slot) in ids.iter().enumerate() {
                let slot = slot as usize;
                if by_slot.len() <= slot {
                    by_slot.resize(slot + 1, u32::MAX);
                }
                by_slot[slot] = k as u32;
            }
            by_slot
        };
        let by_slot = slots(&textures.iter().map(|t| t.slot()).collect::<Vec<_>>());
        let mesh_by_slot = slots(&meshes.iter().map(|m| m.slot()).collect::<Vec<_>>());
        let instances = layout
            .scene
            .instances
            .iter()
            .filter(|i| {
                layout
                    .mesh_streams
                    .get(i.mesh as usize)
                    .is_some_and(Option::is_some)
            })
            .map(|i| {
                (
                    meshes[i.mesh as usize],
                    Mat4::from_cols_array(&i.world_from_object),
                )
            })
            .collect();
        let thread = StreamThread::spawn_with(
            pack,
            settings.depth,
            settings.capacity,
            settings.workers,
            Some(Box::new(decode_mesh_level)),
        )?;
        Ok(Self {
            thread,
            layout,
            textures,
            by_slot,
            meshes,
            mesh_by_slot,
            instances,
            streamer,
            mesh_streamer,
            waiting: VecDeque::new(),
            in_flight: Vec::new(),
            free_tags: Vec::new(),
            buffers: Vec::new(),
            requests: Vec::new(),
            mesh_requests: Vec::new(),
            finished: Vec::new(),
            meshes_read: VecDeque::new(),
            mesh_vertices_per_frame: settings.mesh_vertices_per_frame,
            stats: SceneStreamStats::default(),
        })
    }

    /// The scene without its textures' levels: materials (their texture slots index
    /// [`Self::textures`]), instances (their meshes index [`Self::meshes`]), and the meshes
    /// as opened (streamed ones with their coarsest level only).
    pub fn layout(&self) -> &SceneLayout {
        &self.layout
    }

    /// The pool's id of each of the scene's textures.
    pub fn textures(&self) -> &[TextureId] {
        &self.textures
    }

    /// The geometry pool's id of each of the scene's meshes.
    pub fn meshes(&self) -> &[MeshId] {
        &self.meshes
    }

    /// The texture streamer (its settings and numbers).
    pub fn texture_streamer(&mut self) -> &mut TextureStreamer {
        &mut self.streamer
    }

    /// The mesh streamer (its numbers; [`MeshStreamer::observe`] for instances of your
    /// own).
    pub fn mesh_streamer(&mut self) -> &mut MeshStreamer {
        &mut self.mesh_streamer
    }

    /// What it has done.
    pub fn stats(&self) -> SceneStreamStats {
        self.stats
    }

    /// Whether nothing is waiting, being read or waiting for upload.
    pub fn idle(&self) -> bool {
        self.waiting.is_empty() && self.thread.pending() == 0 && self.meshes_read.is_empty()
    }

    fn tag(&mut self) -> u32 {
        match self.free_tags.pop() {
            Some(tag) => tag,
            None => {
                self.in_flight.push(None);
                self.in_flight.len() as u32 - 1
            }
        }
    }

    /// One frame: folds in feedback that arrived (`feedback`, polled for `frame`) and the
    /// scene's instances seen from `view` (mesh levels; `None`: no view this frame), plans
    /// both within the budget's grants, starts reads (texture levels into the pool's
    /// staging ring, mesh levels into memory) and uploads what arrived. Call before the
    /// frame's graph (the pools' `begin_frame` record the uploads), then
    /// [`GpuBudget::update`].
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        pool: &mut TexturePool,
        geometry: &mut GeometryPool,
        allocator: &mut Allocator,
        budget: &mut GpuBudget,
        feedback: Option<&mut TextureFeedback>,
        view: Option<&LodView>,
        materials: &MaterialTable,
        frame: u64,
    ) {
        engine_profiler::zone!("streaming::SceneStreamer::update");
        if let Some(feedback) = feedback
            && let Some(seen) = feedback.poll(frame)
        {
            self.streamer.feedback(feedback.results(), materials, seen);
        }
        if let Some(view) = view {
            for (mesh, world) in &self.instances {
                self.mesh_streamer.observe(view, *mesh, world, frame);
            }
        }
        self.requests.clear();
        self.streamer
            .update(pool, budget, frame, &mut self.requests);
        self.mesh_requests.clear();
        self.mesh_streamer
            .update(geometry, budget, frame, &mut self.mesh_requests);
        let made = Made {
            frame,
            at: std::time::Instant::now(),
        };
        // Mesh levels first: few and small, and what is close needs its shape.
        self.waiting
            .extend(self.mesh_requests.iter().map(|&r| (Waiting::Mesh(r), made)));
        self.waiting
            .extend(self.requests.iter().map(|&r| (Waiting::Texture(r), made)));
        // Start what fits: the streaming thread's queue and the staging ring.
        while self.thread.has_room() {
            let Some(&(waiting, made)) = self.waiting.front() else {
                break;
            };
            let entry = match waiting {
                Waiting::Texture(r) => self.texture_entry(&r),
                Waiting::Mesh(r) => self.mesh_entry(&r),
            };
            let Some(entry) = entry else {
                self.waiting.pop_front();
                self.fail(&waiting);
                continue;
            };
            let e = self.thread.entries()[entry as usize];
            let bytes = PackStreamer::target_bytes(&e);
            let target = match waiting {
                Waiting::Texture(r) => match pool.staging(allocator, bytes as u64) {
                    Ok(Some(span)) => Target::Texture(span, r),
                    // The ring is full: wait for spans to come back.
                    Ok(None) => break,
                    Err(_) => {
                        self.waiting.pop_front();
                        self.fail(&waiting);
                        continue;
                    }
                },
                Waiting::Mesh(r) => {
                    // Room for the level decoded in place too.
                    let bytes = bytes.max(decoded_mesh_level_bytes(&e));
                    let buffer = match self.buffers.iter().position(|b| b.len() >= bytes) {
                        Some(k) => self.buffers.swap_remove(k),
                        None => AlignedBuffer::new(bytes),
                    };
                    Target::Mesh(buffer, r, entry)
                }
            };
            self.waiting.pop_front();
            let tag = self.tag();
            let mut target = target;
            let (ptr, len) = match &mut target {
                Target::Texture(span, _) => (span.as_mut_ptr(), span.len() as usize),
                Target::Mesh(buffer, ..) => {
                    let slice = buffer.as_mut_slice();
                    (slice.as_mut_ptr(), slice.len())
                }
            };
            // SAFETY: the span or buffer is this request's alone and stays alive (in
            // `in_flight`) until the entry comes back from the thread.
            let started = unsafe { self.thread.request(entry, ptr, len, u64::from(tag)) };
            if started.is_err() {
                match target {
                    Target::Texture(span, _) => pool.cancel_staging(span),
                    Target::Mesh(buffer, ..) => self.buffers.push(buffer),
                }
                self.free_tags.push(tag);
                self.waiting.push_front((waiting, made));
                break;
            }
            self.stats.bytes += bytes as u64;
            self.in_flight[tag as usize] = Some((target, made));
        }
        // Upload what arrived.
        self.thread.poll(&mut self.finished);
        let finished = core::mem::take(&mut self.finished);
        for done in &finished {
            let Some(slot) = self.in_flight.get_mut(done.tag as usize) else {
                continue;
            };
            let Some((target, made)) = slot.take() else {
                continue;
            };
            self.free_tags.push(done.tag as u32);
            let uploaded = match target {
                Target::Texture(span, r) => {
                    let ok = if done.result.is_ok() {
                        // A refused upload gives the span back itself.
                        pool.upload_levels(r.texture, r.first, r.count, span)
                            .is_ok()
                    } else {
                        pool.cancel_staging(span);
                        false
                    };
                    if ok {
                        self.streamer.arrived(r.texture, r.first, r.count);
                    } else {
                        self.streamer.failed(r.texture, r.first, r.count);
                    }
                    ok
                }
                Target::Mesh(buffer, r, entry) => {
                    if done.result.is_ok() {
                        // Uploaded below, within the frame's vertices.
                        self.meshes_read.push_back((buffer, r, entry, made));
                        continue;
                    }
                    self.buffers.push(buffer);
                    self.mesh_streamer.failed(r.mesh, r.level);
                    false
                }
            };
            if uploaded {
                self.stats.arrived += 1;
                let frames = frame - made.frame;
                let ms = made.at.elapsed().as_secs_f64() * 1e3;
                self.stats.latency_frames = frames;
                self.stats.worst_latency_frames = self.stats.worst_latency_frames.max(frames);
                self.stats.latency_ms = ms;
                self.stats.worst_latency_ms = self.stats.worst_latency_ms.max(ms);
                self.stats.latency_ms_sum += ms;
            } else {
                self.stats.failed += 1;
            }
        }
        self.finished = finished;
        self.finished.clear();
        // Mesh levels read: decoded and staged within the frame's vertices.
        let mesh_start = std::time::Instant::now();
        let mut vertices = 0u32;
        while let Some((buffer, r, entry, made)) = self.meshes_read.pop_front() {
            let count = self
                .thread
                .entries()
                .get(entry as usize)
                .map_or(0, |e| e.meta[2]);
            if vertices > 0 && vertices + count > self.mesh_vertices_per_frame {
                self.meshes_read.push_front((buffer, r, entry, made));
                break;
            }
            vertices += count;
            let ok = self.upload_mesh(geometry, &buffer, r, entry);
            self.buffers.push(buffer);
            if ok {
                self.mesh_streamer.arrived(r.mesh, r.level);
                self.stats.mesh_levels += 1;
                self.stats.arrived += 1;
                let ms = made.at.elapsed().as_secs_f64() * 1e3;
                self.stats.latency_ms_sum += ms;
                self.stats.worst_latency_ms = self.stats.worst_latency_ms.max(ms);
            } else {
                self.mesh_streamer.failed(r.mesh, r.level);
                self.stats.failed += 1;
            }
        }
        if vertices > 0 {
            let ms = mesh_start.elapsed().as_secs_f64() * 1e3;
            self.stats.mesh_upload_ms_max = self.stats.mesh_upload_ms_max.max(ms);
            self.stats.mesh_vertices += u64::from(vertices);
            self.stats.mesh_upload_ms_sum += ms;
        }
        self.stats.textures = self.streamer.stats();
        self.stats.meshes = self.mesh_streamer.stats();
        self.stats.waiting = self.waiting.len();
    }

    fn fail(&mut self, waiting: &Waiting) {
        match *waiting {
            Waiting::Texture(r) => self.streamer.failed(r.texture, r.first, r.count),
            Waiting::Mesh(r) => self.mesh_streamer.failed(r.mesh, r.level),
        }
        self.stats.failed += 1;
    }

    /// A mesh level read and decoded into `buffer` (by [`decode_mesh_level`]), into the
    /// geometry pool: a copy.
    fn upload_mesh(
        &self,
        geometry: &mut GeometryPool,
        buffer: &AlignedBuffer,
        request: MeshLevelRequest,
        entry: u32,
    ) -> bool {
        let Some(e) = self.thread.entries().get(entry as usize) else {
            return false;
        };
        let bytes = buffer.as_slice();
        let needed = decoded_mesh_level_bytes(e);
        let Some(bytes) = bytes.get(..needed) else {
            return false;
        };
        let header: &[u32] = bytemuck::cast_slice(&bytes[..16]);
        let (n, i) = (header[0] as usize, header[1] as usize);
        if header[3] != request.level || 16 + n * 28 + i * 4 != needed {
            return false;
        }
        let positions: &[[f32; 3]] = bytemuck::cast_slice(&bytes[16..16 + n * 12]);
        let attributes: &[VertexAttributes] =
            bytemuck::cast_slice(&bytes[16 + n * 12..16 + n * 28]);
        let indices: &[u32] = bytemuck::cast_slice(&bytes[16 + n * 28..]);
        geometry
            .upload_mesh_level_encoded(request.mesh, request.level, positions, attributes, indices)
            .is_ok()
    }

    /// The pack entry holding a texture request's levels.
    fn texture_entry(&self, request: &LevelRequest) -> Option<u32> {
        let texture = *self.by_slot.get(request.texture.slot() as usize)? as usize;
        if self.textures.get(texture) != Some(&request.texture) {
            return None;
        }
        let record = self.layout.textures.get(texture)?;
        let entry = *record.entries.get(request.first as usize)?;
        let e = self.thread.entries().get(entry as usize)?;
        // The entry starts at the request's first level and holds its levels.
        (e.meta[4] == request.first && e.meta[5] >= request.count).then_some(entry)
    }

    /// The pack entry holding a mesh level.
    fn mesh_entry(&self, request: &MeshLevelRequest) -> Option<u32> {
        let mesh = *self.mesh_by_slot.get(request.mesh.slot() as usize)? as usize;
        if self.meshes.get(mesh) != Some(&request.mesh) {
            return None;
        }
        let streamed = self.layout.mesh_streams.get(mesh)?.as_ref()?;
        let entry = streamed.levels.get(request.level as usize)?.entry;
        (entry != u32::MAX).then_some(entry)
    }

    /// Stops streaming: waits for reads in flight and gives their spans back.
    pub fn finish(mut self, pool: &mut TexturePool) {
        // Dropping the thread finishes what is in flight.
        drop(self.thread);
        for (target, _) in self.in_flight.drain(..).flatten() {
            if let Target::Texture(span, _) = target {
                pool.cancel_staging(span);
            }
        }
    }
}
