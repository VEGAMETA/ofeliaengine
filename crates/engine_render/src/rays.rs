//! The scene for ray queries (ADR 0087): a bottom-level acceleration structure for each
//! mesh the GPU scene draws, built when it is first drawn (again when its chosen detail
//! level changes), and a top-level one written on the GPU from the GPU scene's instances
//! (`rays.slang`), so instances placed on the GPU (ADR 0064) move in it too.
//!
//! - **Bottom levels:** a mesh's triangles at a detail level ([`BlasDetail`]) from the
//!   geometry pool's positions and indices, opaque, `PREFER_FAST_TRACE` and
//!   `ALLOW_COMPACTION` (static geometry: NVIDIA's and AMD's advice). Once the frame that
//!   built one has finished, its compacted size is read back and it is copied into a
//!   structure of that size. The pool may move its buffers: a built structure keeps its
//!   own copy. Builds per frame are held to [`RaySettings::build_budget`].
//! - **Top level:** one instance per slot of the GPU scene, its custom index the slot.
//!   Holes, removed meshes and meshes not built yet are inactive (a reference of 0). The
//!   mask is the batch's kind ([`RAY_MASK_OPAQUE`], [`RAY_MASK_MASKED`],
//!   [`RAY_MASK_BLENDED`]); facing culling is off (shaders read the face). Built with
//!   `PREFER_FAST_TRACE`; in frames where instances only moved, refitted or rebuilt as
//!   [`TopLevelUpdate`] says (by default refitted, rebuilt after 8 refits); rebuilt at
//!   once when instances came, went, teleported or changed mesh or a bottom level
//!   changed; kept in a still scene.
//! - **Shaders** reach it by its address (`accel_at` in `rays_common.slang`), the pass
//!   declaring [`crate::graph::Access::AccelerationStructureRead`] of [`RayFrame::tlas`].
//!   [`RayFrame::add_cast_pass`] casts a batch of rays (tests, picking),
//!   [`RayFrame::add_camera_pass`] one through every pixel.
//! - Nothing exists before [`RayScene::new`]: a renderer without ray queries pays
//!   nothing. Skinned meshes keep the shape of their first build for now.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::forward::{BLENDED, MASKED};
use crate::gpu_scene::SceneFrame;
use crate::graph::{
    Access, Buffer, BufferDesc, BufferState, FrameGraph, ImportedBuffer, ShaderStages,
};
use crate::mesh::{GeometryFrame, MeshId};
use crate::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AccelLevel, AccelerationStructure, AdapterKind, Allocator, Bindless, Device, RhiError,
};

const TLAS_INSTANCES: &[u8] = include_bytes!("../shaders/rays.tlasInstances.spv");
const CAST_RAYS: &[u8] = include_bytes!("../shaders/rays.castRays.spv");
const CAST_CAMERA_RAYS: &[u8] = include_bytes!("../shaders/rays.castCameraRays.spv");

/// Instance mask of opaque surfaces (`RAY_MASK_OPAQUE` in `rays_common.slang`).
pub const RAY_MASK_OPAQUE: u32 = 1;
/// Instance mask of alpha-tested surfaces, traced as opaque for now.
pub const RAY_MASK_MASKED: u32 = 2;
/// Instance mask of transparent surfaces.
pub const RAY_MASK_BLENDED: u32 = 4;
/// Every instance.
pub const RAY_MASK_ALL: u32 = 0xFF;

/// Bytes of a `VkAccelerationStructureInstanceKHR`.
const INSTANCE_BYTES: u64 = 64;

/// Which detail level of a mesh its bottom-level structure is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlasDetail {
    /// The finest level resident (rebuilt when streaming changes it).
    #[default]
    Finest,
    /// The coarsest level: a fraction of the memory and build time, enough for diffuse
    /// light (streamed meshes always have it).
    Coarsest,
}

/// How the top level follows instances that moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopLevelUpdate {
    /// Rebuilt in every frame where an instance moved.
    Rebuild,
    /// Refitted where only transforms changed (its hierarchy kept, its boxes moved:
    /// cheaper, but its rays slow down as instances travel away from where it was
    /// built), rebuilt after `rebuild_after` refits in a row and whenever instances were
    /// added, removed, teleported or given another mesh, or a bottom level changed.
    Refit {
        /// Refits in a row before a rebuild.
        rebuild_after: u32,
    },
}

impl Default for TopLevelUpdate {
    /// Refitted, rebuilt after 8 refits: with 10,000 instances, a tenth of them moving,
    /// its rays stay within 1 % of a rebuilt structure's while it costs a fifth to a
    /// quarter of rebuilding every frame (`ray_bench`, ADR 0087's addendum).
    fn default() -> Self {
        Self::Refit { rebuild_after: 8 }
    }
}

/// Settings of a [`RayScene`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaySettings {
    /// The detail level structures are built from.
    pub detail: BlasDetail,
    /// How the top level follows moving instances.
    pub top_level: TopLevelUpdate,
    /// Triangles of bottom levels built in one frame at most (at least one structure is
    /// built per frame): meshes appearing during play do not make one frame build them
    /// all. Instances of meshes waiting for theirs are not in the top level yet
    /// ([`RayStats::pending`]). `u32::MAX`: no limit.
    pub build_budget: u32,
}

impl Default for RaySettings {
    /// The finest level, refitted top level, no build budget (everything drawn is built
    /// in the frame it first appears: right while loading).
    fn default() -> Self {
        Self {
            detail: BlasDetail::default(),
            top_level: TopLevelUpdate::default(),
            build_budget: u32::MAX,
        }
    }
}

impl RaySettings {
    /// The defaults with a build budget for play on `device`'s class of GPU: 131,072
    /// triangles on discrete GPUs (an RTX 4060 Ti builds Sponza's finest level in three
    /// frames of at most 0.8 ms), 32,768 on integrated ones (a two-CU Radeon: nine
    /// frames, 5.7 ms on average). A mesh larger than the budget is built alone in a
    /// frame (Sponza's largest: 11 ms on that Radeon): integrated GPUs want
    /// [`BlasDetail::Coarsest`] (Sponza's coarsest level: two frames of at most 5.6 ms).
    pub fn for_device(device: &Device) -> Self {
        let build_budget = match device.adapter().kind {
            AdapterKind::Integrated | AdapterKind::Cpu => 1 << 15,
            _ => 1 << 17,
        };
        Self {
            build_budget,
            ..Self::default()
        }
    }
}

/// What a [`RayScene`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RayStats {
    /// Bottom-level structures.
    pub blas_count: u32,
    /// Their bytes.
    pub blas_bytes: u64,
    /// Their triangles.
    pub triangles: u64,
    /// Bottom-level structures built this frame.
    pub built: u32,
    /// Bottom-level structures compacted this frame.
    pub compacted: u32,
    /// Whether the top level was rebuilt this frame.
    pub rebuilt: bool,
    /// Whether the top level was refitted this frame ([`TopLevelUpdate::Refit`]).
    pub refitted: bool,
    /// Batches whose mesh waits for its bottom level ([`RaySettings::build_budget`]).
    pub pending: u32,
    /// The top level's bytes.
    pub tlas_bytes: u64,
    /// Instances (the GPU scene's slots) in the top level this frame.
    pub instances: u32,
}

/// `BatchRay` in `rays.slang`.
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct BatchRay {
    blas: u64,
    mask: u32,
    flags: u32,
}

/// `InstancePush` in `rays.slang`.
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct InstancePush {
    instances: u64,
    batches: u64,
    out: u64,
    count: u32,
    batch_count: u32,
}

/// A ray for [`RayFrame::add_cast_pass`] (`RayCast` in `rays.slang`).
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct RayCast {
    /// xyz: the origin; w: the nearest distance.
    pub origin: [f32; 4],
    /// xyz: the direction (its length scales the distances); w: the farthest distance.
    pub direction: [f32; 4],
}

/// A ray's nearest hit (`RayHit` in `rays.slang`).
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct RayHit {
    /// Distance along the ray, -1 without a hit.
    pub t: f32,
    /// The GPU scene's slot of the instance hit.
    pub slot: u32,
    /// The triangle, in the mesh's level its structure was built from.
    pub primitive: u32,
    /// 1 when the ray hit the triangle's front.
    pub front: u32,
    /// The hit's barycentric coordinates (of the second and third vertices).
    pub barycentrics: [f32; 2],
    _pad: [u32; 2],
}

/// `CastPush` in `rays.slang`.
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct CastPush {
    accel: u64,
    rays: u64,
    hits: u64,
    count: u32,
    mask: u32,
}

/// `CameraPush` in `rays.slang`.
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct CameraPush {
    accel: u64,
    world_from_clip: u64,
    out: u64,
    size: [u32; 2],
    mask: u32,
    _pad: u32,
}

/// A mesh's bottom-level structure.
#[derive(Debug)]
struct Blas {
    accel: AccelerationStructure,
    mesh: MeshId,
    /// The level it was built from: first index, index count.
    level: (u32, u32),
}

/// A bottom-level build recorded this frame.
#[derive(Debug, Clone, Copy)]
struct PendingBuild {
    accel: vk::AccelerationStructureKHR,
    vertices: vk::DeviceAddress,
    max_vertex: u32,
    indices: vk::DeviceAddress,
    triangles: u32,
    scratch_offset: u64,
}

/// A bottom level whose compacted size is on its way.
#[derive(Debug, Clone, Copy)]
struct Compacting {
    mesh: MeshId,
    level: (u32, u32),
    query: u32,
    frame: u64,
}

/// Queries for compacted sizes.
const QUERIES: u32 = 1024;

/// Triangles of bottom levels compacted in one frame at most (the rest wait for the
/// next): a level loaded at once does not make one frame copy all of it (Bevy Solari
/// bounds its compaction the same way).
const COMPACT_TRIANGLES_PER_FRAME: u32 = 1 << 20;

/// Bottom levels: static geometry traced often (NVIDIA's and AMD's advice), compacted.
const BLAS_FLAGS: vk::BuildAccelerationStructureFlagsKHR =
    vk::BuildAccelerationStructureFlagsKHR::from_raw(
        vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_TRACE.as_raw()
            | vk::BuildAccelerationStructureFlagsKHR::ALLOW_COMPACTION.as_raw(),
    );

/// The top-level structure.
#[derive(Debug)]
struct Tlas {
    accel: AccelerationStructure,
    capacity: u32,
    scratch: u64,
}

/// The state the top level is left in at the end of a frame: read by shaders.
const TLAS_READ: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::from_raw(
        vk::PipelineStageFlags2::COMPUTE_SHADER.as_raw()
            | vk::PipelineStageFlags2::FRAGMENT_SHADER.as_raw()
            | vk::PipelineStageFlags2::VERTEX_SHADER.as_raw(),
    ),
    access: vk::AccessFlags2::ACCELERATION_STRUCTURE_READ_KHR,
};

/// A frame's top level, from [`RayScene::add_passes`].
#[derive(Debug, Clone, Copy)]
pub struct RayFrame<'g> {
    scene: &'g RayScene,
    /// Its buffer in the frame's graph: passes that trace declare
    /// [`Access::AccelerationStructureRead`] of it.
    pub tlas: Buffer,
    /// Its device address, for `accel_at` (two words in a push constant block).
    pub address: vk::DeviceAddress,
    /// Instances (the GPU scene's slots).
    pub instances: u32,
}

impl<'g> RayFrame<'g> {
    /// Declares a pass that casts `count` rays ([`RayCast`] at `rays`: the upload ring's
    /// memory, written before the frame) against this top level, the instances in
    /// `mask`, writing each ray's nearest hit ([`RayHit`] in `hits`).
    pub fn add_cast_pass(
        &self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        rays: vk::DeviceAddress,
        hits: Buffer,
        count: u32,
        mask: u32,
    ) {
        let address = self.address;
        fg.add_compute("cast rays", bindless)
            .read(
                self.tlas,
                Access::AccelerationStructureRead(ShaderStages::COMPUTE),
            )
            .write(hits, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.scene.cast,
                Dispatch::Threads([count, 1, 1]),
                move |ctx| {
                    Some(CastPush {
                        accel: address,
                        rays,
                        hits: ctx.address(hits),
                        count,
                        mask,
                    })
                },
            );
    }

    /// Declares a pass that casts a ray through every pixel's center of a `size` image
    /// from the camera whose inverse view-projection is at `world_from_clip` (16 floats,
    /// column-major, in the upload ring), against the instances in `mask`, writing each
    /// pixel's distance to its nearest hit (-1 for none) into `out` (a float per pixel,
    /// rows top to bottom).
    pub fn add_camera_pass(
        &self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        world_from_clip: vk::DeviceAddress,
        out: Buffer,
        size: [u32; 2],
        mask: u32,
    ) {
        let address = self.address;
        fg.add_compute("camera rays", bindless)
            .read(
                self.tlas,
                Access::AccelerationStructureRead(ShaderStages::COMPUTE),
            )
            .write(out, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.scene.cast_camera,
                Dispatch::Threads([size[0], size[1], 1]),
                move |ctx| {
                    Some(CameraPush {
                        accel: address,
                        world_from_clip,
                        out: ctx.address(out),
                        size,
                        mask,
                        _pad: 0,
                    })
                },
            );
    }
}

/// The scene for ray queries (see the module documentation).
pub struct RayScene {
    loader: engine_rhi::ash::khr::acceleration_structure::Device,
    scratch_alignment: u64,
    write_instances: ComputeShader<InstancePush>,
    cast: ComputeShader<CastPush>,
    cast_camera: ComputeShader<CameraPush>,
    settings: RaySettings,
    /// By mesh slot.
    blas: Vec<Option<Blas>>,
    /// Structures replaced or dropped, with the frame that last used them.
    retired: Vec<(u64, AccelerationStructure)>,
    tlas: Option<Tlas>,
    tlas_state: BufferState,
    builds: Vec<PendingBuild>,
    batch_rays: Vec<BatchRay>,
    frames_in_flight: u64,
    stats: RayStats,
    /// Compacted sizes of bottom levels ([`QUERIES`]), and the builds waiting for theirs.
    queries: vk::QueryPool,
    next_query: u32,
    compacting: Vec<Compacting>,
    /// This frame's builds' queries: first, count (0: none).
    build_queries: (u32, u32),
    /// This frame's compactions: from, into.
    copies: Vec<(vk::AccelerationStructureKHR, vk::AccelerationStructureKHR)>,
    /// A structure changed since the top level's last build.
    dirty: bool,
    /// Instances in the top level's last full build (0: none since it was made).
    built_count: u32,
    /// Refits since the top level's last full build.
    refits: u32,
}

impl core::fmt::Debug for RayScene {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RayScene")
            .field("settings", &self.settings)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl RayScene {
    /// Creates the scene's passes; `frames_in_flight` bounds how long replaced structures
    /// are kept. Fails on a device without ray queries ([`Device::ray_query`]).
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        settings: RaySettings,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let ray_query = device.ray_query().ok_or(RhiError::Vulkan {
            call: "the scene for ray queries (ray queries are not enabled)",
            result: vk::Result::ERROR_FEATURE_NOT_PRESENT,
        })?;
        let [write_instances] = ComputeSource::create_all(
            device,
            bindless,
            [ComputeSource::new(TLAS_INSTANCES, c"tlasInstances", &[])?],
        )?;
        let [cast] = ComputeSource::create_all(
            device,
            bindless,
            [ComputeSource::new(CAST_RAYS, c"castRays", &[])?],
        )?;
        let [cast_camera] = ComputeSource::create_all(
            device,
            bindless,
            [ComputeSource::new(
                CAST_CAMERA_RAYS,
                c"castCameraRays",
                &[],
            )?],
        )?;
        // SAFETY: a valid create info.
        let queries = unsafe {
            device.raw().create_query_pool(
                &vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::ACCELERATION_STRUCTURE_COMPACTED_SIZE_KHR)
                    .query_count(QUERIES),
                None,
            )
        }
        .map_err(RhiError::vk("vkCreateQueryPool"))?;
        Ok(Self {
            loader: ray_query.loader().clone(),
            scratch_alignment: ray_query.scratch_alignment(),
            write_instances,
            cast,
            cast_camera,
            settings,
            blas: Vec::new(),
            retired: Vec::new(),
            tlas: None,
            tlas_state: BufferState::idle(),
            builds: Vec::new(),
            batch_rays: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            stats: RayStats::default(),
            queries,
            next_query: 0,
            compacting: Vec::new(),
            build_queries: (0, 0),
            copies: Vec::new(),
            dirty: true,
            built_count: 0,
            refits: 0,
        })
    }

    /// What it holds, as of the last frame.
    pub fn stats(&self) -> RayStats {
        self.stats
    }

    /// The settings.
    pub fn settings(&self) -> RaySettings {
        self.settings
    }

    /// Declares this frame's builds: the bottom levels of meshes drawn for the first time
    /// (or at another level), then the top level from the GPU scene's instances (after
    /// `scene`'s passes, which placed them). `None` when the scene has no slots. Allocates
    /// nothing in frames that build no bottom level, once its lists have grown.
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        allocator: &mut Allocator,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        scene: &SceneFrame<'g>,
        frame_number: u64,
    ) -> Result<Option<RayFrame<'g>>, RhiError> {
        engine_profiler::zone!("render::RayScene::add_passes");
        self.collect_retired(device, allocator, frame_number);
        let batches = scene.batches();
        let positions = geometry.position_address();
        let indices = geometry.index_address();

        // Structures of removed meshes go.
        for entry in &mut self.blas {
            if entry
                .as_ref()
                .is_some_and(|blas| geometry.mesh(blas.mesh).is_none())
                && let Some(blas) = entry.take()
            {
                self.retired.push((frame_number, blas.accel));
                self.dirty = true;
            }
        }
        self.compact(device, allocator, frame_number)?;
        // Bottom levels of the meshes drawn (batch 0 is the GPU scene's holes).
        self.builds.clear();
        let mut scratch = 0u64;
        let mut budget = self.settings.build_budget;
        let mut pending = 0;
        for b in batches.iter().skip(1).filter(|b| b.alive) {
            let Some(info) = geometry.mesh(b.mesh) else {
                continue;
            };
            let lod = match self.settings.detail {
                BlasDetail::Finest => info.lods[0],
                BlasDetail::Coarsest => info.lods[info.lod_count.max(1) as usize - 1],
            };
            let level = (lod.first_index, lod.index_count);
            if level.1 < 3 {
                continue;
            }
            let slot = b.mesh.slot() as usize;
            if self.blas.len() <= slot {
                self.blas.resize_with(slot + 1, || None);
            }
            if self.blas[slot]
                .as_ref()
                .is_some_and(|blas| blas.mesh == b.mesh && blas.level == level)
            {
                continue;
            }
            // Over this frame's budget: a later frame builds it.
            let triangles = level.1 / 3;
            if !self.builds.is_empty() && triangles > budget {
                pending += 1;
                continue;
            }
            budget = budget.saturating_sub(triangles);
            let build = PendingBuild {
                accel: vk::AccelerationStructureKHR::null(),
                vertices: positions + u64::from(info.first_vertex) * 12,
                max_vertex: info.vertex_count.saturating_sub(1),
                indices: indices + u64::from(level.0) * 4,
                triangles,
                scratch_offset: scratch.next_multiple_of(self.scratch_alignment),
            };
            let sizes = self.blas_sizes(&build);
            let accel = AccelerationStructure::new(
                device,
                allocator,
                AccelLevel::Bottom,
                sizes.acceleration_structure_size,
            )?;
            scratch = build.scratch_offset + sizes.build_scratch_size;
            self.builds.push(PendingBuild {
                accel: accel.raw(),
                ..build
            });
            if let Some(old) = self.blas[slot].replace(Blas {
                accel,
                mesh: b.mesh,
                level,
            }) {
                self.retired.push((frame_number, old.accel));
            }
            self.dirty = true;
        }
        self.assign_queries(frame_number);
        self.stats.pending = pending;

        // The batches' structures and instance kinds.
        self.batch_rays.clear();
        for (k, b) in batches.iter().enumerate() {
            let blas = (k > 0 && b.alive)
                .then(|| self.blas.get(b.mesh.slot() as usize))
                .flatten()
                .and_then(Option::as_ref)
                .filter(|blas| blas.mesh == b.mesh);
            let state = b.state as usize;
            let mask = if state & BLENDED != 0 {
                RAY_MASK_BLENDED
            } else if state & MASKED != 0 {
                RAY_MASK_MASKED
            } else {
                RAY_MASK_OPAQUE
            };
            self.batch_rays.push(BatchRay {
                blas: blas.map_or(0, |blas| blas.accel.address()),
                mask,
                flags: (vk::GeometryInstanceFlagsKHR::TRIANGLE_FACING_CULL_DISABLE
                    | vk::GeometryInstanceFlagsKHR::FORCE_OPAQUE)
                    .as_raw(),
            });
        }

        // The top level, grown to the scene's slots.
        let count = scene.slot_count();
        self.stats.instances = count;
        if count == 0 {
            self.update_stats();
            return Ok(None);
        }
        if self.tlas.as_ref().is_none_or(|t| t.capacity < count) {
            let capacity = count.next_power_of_two().max(64);
            let sizes = self.tlas_sizes(capacity);
            let accel = AccelerationStructure::new(
                device,
                allocator,
                AccelLevel::Top,
                sizes.acceleration_structure_size,
            )?;
            if let Some(old) = self.tlas.replace(Tlas {
                accel,
                capacity,
                scratch: sizes.build_scratch_size.max(sizes.update_scratch_size),
            }) {
                self.retired.push((frame_number, old.accel));
            }
            self.tlas_state = BufferState::idle();
            self.built_count = 0;
        }
        // Rebuilt when instances came or went or a structure changed; when instances only
        // moved, refitted or rebuilt as the settings say; kept otherwise. (A move that
        // settles needs nothing: the slot's transform is the one already in the structure.)
        let changes = scene.stats();
        let restructured =
            self.dirty || count != self.built_count || changes.grew || changes.restructured > 0;
        let moved = changes.uploaded > 0 || changes.moving > 0;
        let (rebuild, refit) = match self.settings.top_level {
            _ if restructured => (true, false),
            _ if !moved => (false, false),
            TopLevelUpdate::Refit { rebuild_after } if self.refits < rebuild_after => (false, true),
            _ => (true, false),
        };
        if rebuild {
            self.built_count = count;
            self.refits = 0;
        }
        self.refits += u32::from(refit);
        self.dirty = false;
        self.stats.rebuilt = rebuild;
        self.stats.refitted = refit;
        self.update_stats();
        // The top level's state before this frame, and after it (read by shaders).
        let initial = core::mem::replace(&mut self.tlas_state, TLAS_READ);

        let this: &'g Self = self;
        let tlas = this.tlas.as_ref().ok_or(RhiError::Vulkan {
            call: "the ray scene's top level",
            result: vk::Result::ERROR_INITIALIZATION_FAILED,
        })?;
        // Bottom levels built or compacted this frame: the top level's build waits for
        // them (the graph orders passes by what they share).
        let token = (!this.builds.is_empty() || !this.copies.is_empty())
            .then(|| fg.create_buffer("bottom levels", BufferDesc { size: 256 }));
        if let Some(token) = token
            && !this.builds.is_empty()
        {
            let blas_scratch = fg.create_buffer(
                "blas scratch",
                BufferDesc {
                    size: scratch + this.scratch_alignment,
                },
            );
            fg.add_pass("blas builds")
                .read(geometry.positions, Access::AccelerationStructureInput)
                .read(geometry.indices, Access::AccelerationStructureInput)
                .write(blas_scratch, Access::AccelerationStructureScratch)
                .write(token, Access::AccelerationStructureWrite)
                // The structures it builds are not the graph's.
                .side_effects()
                .execute(move |ctx| {
                    let base = ctx
                        .address(blas_scratch)
                        .next_multiple_of(this.scratch_alignment);
                    // SAFETY: recording outside a rendering scope; the structures,
                    // inputs and scratch live until the frame completes.
                    unsafe { this.record_blas_builds(ctx.device, ctx.cmd, base) };
                });
        }
        if let Some(token) = token
            && !this.copies.is_empty()
        {
            fg.add_pass("blas compaction")
                .write(token, Access::AccelerationStructureWrite)
                .side_effects()
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the sources were built
                    // in a finished frame, the destinations made for their sizes.
                    unsafe { this.record_compaction(ctx.device, ctx.cmd) };
                });
        }
        let handle = fg.import_buffer(
            "tlas",
            ImportedBuffer {
                buffer: tlas.accel.buffer(),
                size: tlas.accel.size(),
                address: 0,
                initial,
                final_state: Some(TLAS_READ),
            },
        );
        if !rebuild && !refit {
            return Ok(Some(RayFrame {
                scene: this,
                tlas: handle,
                address: tlas.accel.address(),
                instances: count,
            }));
        }
        let batch_table = ring.push(&this.batch_rays).ok_or(RhiError::Vulkan {
            call: "the ray scene's batch table (upload ring full)",
            result: vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        })?;
        let instances = fg.create_buffer(
            "tlas instances",
            BufferDesc {
                size: u64::from(count) * INSTANCE_BYTES,
            },
        );
        let batch_count = this.batch_rays.len() as u32;
        let current = scene.current;
        fg.add_compute("tlas instances", bindless)
            .read(current, Access::StorageRead(ShaderStages::COMPUTE))
            .write(instances, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &this.write_instances,
                Dispatch::Threads([count, 1, 1]),
                move |ctx| {
                    Some(InstancePush {
                        instances: ctx.address(current),
                        batches: batch_table,
                        out: ctx.address(instances),
                        count,
                        batch_count,
                    })
                },
            );
        let tlas_scratch = fg.create_buffer(
            "tlas scratch",
            BufferDesc {
                size: tlas.scratch + this.scratch_alignment,
            },
        );
        let raw = tlas.accel.raw();
        let mut build = fg.add_pass("tlas build");
        if let Some(token) = token {
            build = build.read(token, Access::AccelerationStructureBuildRead);
        }
        build
            .read(instances, Access::AccelerationStructureInput)
            .write(tlas_scratch, Access::AccelerationStructureScratch)
            .write(handle, Access::AccelerationStructureWrite)
            .execute(move |ctx| {
                let geometry = vk::AccelerationStructureGeometryKHR::default()
                    .geometry_type(vk::GeometryTypeKHR::INSTANCES)
                    .geometry(vk::AccelerationStructureGeometryDataKHR {
                        instances: vk::AccelerationStructureGeometryInstancesDataKHR::default()
                            .array_of_pointers(false)
                            .data(vk::DeviceOrHostAddressConstKHR {
                                device_address: ctx.address(instances),
                            }),
                    });
                // A refit updates the structure in place: its own source.
                let (mode, source) = if refit {
                    (vk::BuildAccelerationStructureModeKHR::UPDATE, raw)
                } else {
                    (
                        vk::BuildAccelerationStructureModeKHR::BUILD,
                        vk::AccelerationStructureKHR::null(),
                    )
                };
                let info = vk::AccelerationStructureBuildGeometryInfoKHR::default()
                    .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL)
                    .flags(this.tlas_flags())
                    .mode(mode)
                    .src_acceleration_structure(source)
                    .dst_acceleration_structure(raw)
                    .geometries(core::slice::from_ref(&geometry))
                    .scratch_data(vk::DeviceOrHostAddressKHR {
                        device_address: ctx
                            .address(tlas_scratch)
                            .next_multiple_of(this.scratch_alignment),
                    });
                let range =
                    vk::AccelerationStructureBuildRangeInfoKHR::default().primitive_count(count);
                // Through the function itself: ash's wrapper collects the ranges into a
                // vector, an allocation every frame.
                let ranges = [&raw const range];
                // SAFETY: recording outside a rendering scope; the graph made the
                // instances visible to the build and the structure writable; one info
                // with one geometry, one range.
                unsafe {
                    (this.loader.fp().cmd_build_acceleration_structures_khr)(
                        ctx.cmd,
                        1,
                        &raw const info,
                        ranges.as_ptr(),
                    );
                }
            });
        Ok(Some(RayFrame {
            scene: this,
            tlas: handle,
            address: tlas.accel.address(),
            instances: count,
        }))
    }

    /// Frees every structure.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(&mut self, device: &Device, allocator: &mut Allocator) {
        if self.queries != vk::QueryPool::null() {
            // SAFETY: guaranteed by the caller.
            unsafe { device.raw().destroy_query_pool(self.queries, None) };
            self.queries = vk::QueryPool::null();
        }
        for (_, mut accel) in self.retired.drain(..) {
            // SAFETY: guaranteed by the caller.
            unsafe { accel.destroy(device, allocator) };
        }
        for mut blas in self.blas.drain(..).flatten() {
            // SAFETY: guaranteed by the caller.
            unsafe { blas.accel.destroy(device, allocator) };
        }
        if let Some(mut tlas) = self.tlas.take() {
            // SAFETY: guaranteed by the caller.
            unsafe { tlas.accel.destroy(device, allocator) };
        }
    }

    /// Frees what frames up to `frame_number - frames_in_flight` used.
    fn collect_retired(&mut self, device: &Device, allocator: &mut Allocator, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, mut accel) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it have finished.
            unsafe { accel.destroy(device, allocator) };
        }
    }

    /// Bottom levels whose frame has finished get their compacted copy: made now, copied
    /// by this frame's compaction pass, swapped in (the original retired).
    fn compact(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        frame_number: u64,
    ) -> Result<(), RhiError> {
        self.copies.clear();
        let mut triangles = 0u32;
        let mut i = 0;
        while i < self.compacting.len() {
            let c = self.compacting[i];
            if c.frame + self.frames_in_flight > frame_number
                || triangles >= COMPACT_TRIANGLES_PER_FRAME
            {
                i += 1;
                continue;
            }
            let mut size = [0u64];
            // SAFETY: the query was written by a finished frame (not ready: kept).
            let read = unsafe {
                device.raw().get_query_pool_results(
                    self.queries,
                    c.query,
                    &mut size,
                    vk::QueryResultFlags::TYPE_64,
                )
            };
            if read.is_err() {
                i += 1;
                continue;
            }
            self.compacting.swap_remove(i);
            let slot = c.mesh.slot() as usize;
            let Some(current) = self.blas.get(slot).and_then(Option::as_ref) else {
                continue;
            };
            // Replaced since (another level, another mesh), or no smaller: kept as is.
            if current.mesh != c.mesh || current.level != c.level || size[0] == 0 {
                continue;
            }
            if size[0] >= current.accel.size() {
                continue;
            }
            triangles += c.level.1 / 3;
            let compact =
                AccelerationStructure::new(device, allocator, AccelLevel::Bottom, size[0])?;
            self.copies.push((current.accel.raw(), compact.raw()));
            if let Some(blas) = self.blas[slot].as_mut() {
                let old = core::mem::replace(&mut blas.accel, compact);
                self.retired.push((frame_number, old));
            }
            self.dirty = true;
        }
        Ok(())
    }

    /// Queries for this frame's builds' compacted sizes: a free range of the pool, or
    /// none (their structures then stay as built).
    fn assign_queries(&mut self, frame_number: u64) {
        self.build_queries = (0, 0);
        let n = self.builds.len() as u32;
        if n == 0 || n > QUERIES {
            return;
        }
        let first = if self.next_query + n > QUERIES {
            0
        } else {
            self.next_query
        };
        if self
            .compacting
            .iter()
            .any(|c| (first..first + n).contains(&c.query))
        {
            return;
        }
        self.next_query = first + n;
        self.build_queries = (first, n);
        // The builds are in `blas` order of this frame: find each one's entry.
        for (k, build) in self.builds.iter().enumerate() {
            if let Some(blas) = self
                .blas
                .iter()
                .flatten()
                .find(|blas| blas.accel.raw() == build.accel)
            {
                self.compacting.push(Compacting {
                    mesh: blas.mesh,
                    level: blas.level,
                    query: first + k as u32,
                    frame: frame_number,
                });
            }
        }
    }

    /// Records this frame's compaction copies, then a barrier that makes them visible to
    /// the top level's build and to shaders.
    ///
    /// # Safety
    ///
    /// `cmd` is recording outside a rendering scope.
    unsafe fn record_compaction(&self, device: &engine_rhi::ash::Device, cmd: vk::CommandBuffer) {
        for &(from, into) in &self.copies {
            // SAFETY: guaranteed by the caller.
            unsafe {
                self.loader.cmd_copy_acceleration_structure(
                    cmd,
                    &vk::CopyAccelerationStructureInfoKHR::default()
                        .src(from)
                        .dst(into)
                        .mode(vk::CopyAccelerationStructureModeKHR::COMPACT),
                );
            }
        }
        // SAFETY: guaranteed by the caller.
        unsafe { structures_written(device, cmd) };
    }

    fn update_stats(&mut self) {
        let mut stats = RayStats {
            instances: self.stats.instances,
            rebuilt: self.stats.rebuilt,
            refitted: self.stats.refitted,
            pending: self.stats.pending,
            compacted: self.copies.len() as u32,
            built: self.builds.len() as u32,
            tlas_bytes: self.tlas.as_ref().map_or(0, |t| t.accel.size()),
            ..RayStats::default()
        };
        for blas in self.blas.iter().flatten() {
            stats.blas_count += 1;
            stats.blas_bytes += blas.accel.size();
            stats.triangles += u64::from(blas.level.1 / 3);
        }
        self.stats = stats;
    }

    /// A bottom-level build's geometry.
    fn blas_geometry(build: &PendingBuild) -> vk::AccelerationStructureGeometryKHR<'static> {
        vk::AccelerationStructureGeometryKHR::default()
            .geometry_type(vk::GeometryTypeKHR::TRIANGLES)
            .geometry(vk::AccelerationStructureGeometryDataKHR {
                triangles: vk::AccelerationStructureGeometryTrianglesDataKHR::default()
                    .vertex_format(vk::Format::R32G32B32_SFLOAT)
                    .vertex_data(vk::DeviceOrHostAddressConstKHR {
                        device_address: build.vertices,
                    })
                    .vertex_stride(12)
                    .max_vertex(build.max_vertex)
                    .index_type(vk::IndexType::UINT32)
                    .index_data(vk::DeviceOrHostAddressConstKHR {
                        device_address: build.indices,
                    }),
            })
            .flags(vk::GeometryFlagsKHR::OPAQUE)
    }

    fn blas_sizes(
        &self,
        build: &PendingBuild,
    ) -> vk::AccelerationStructureBuildSizesInfoKHR<'static> {
        let geometry = Self::blas_geometry(build);
        let info = vk::AccelerationStructureBuildGeometryInfoKHR::default()
            .ty(vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL)
            .flags(BLAS_FLAGS)
            .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
            .geometries(core::slice::from_ref(&geometry));
        let mut sizes = vk::AccelerationStructureBuildSizesInfoKHR::default();
        // SAFETY: a valid build description; sizes only.
        unsafe {
            self.loader.get_acceleration_structure_build_sizes(
                vk::AccelerationStructureBuildTypeKHR::DEVICE,
                &info,
                &[build.triangles],
                &mut sizes,
            );
        }
        sizes
    }

    /// The top level's build flags: fast trace, and updatable when it is refitted.
    fn tlas_flags(&self) -> vk::BuildAccelerationStructureFlagsKHR {
        match self.settings.top_level {
            TopLevelUpdate::Rebuild => vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_TRACE,
            TopLevelUpdate::Refit { .. } => {
                vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_TRACE
                    | vk::BuildAccelerationStructureFlagsKHR::ALLOW_UPDATE
            }
        }
    }

    fn tlas_sizes(&self, capacity: u32) -> vk::AccelerationStructureBuildSizesInfoKHR<'static> {
        let geometry = vk::AccelerationStructureGeometryKHR::default()
            .geometry_type(vk::GeometryTypeKHR::INSTANCES)
            .geometry(vk::AccelerationStructureGeometryDataKHR {
                instances: vk::AccelerationStructureGeometryInstancesDataKHR::default()
                    .array_of_pointers(false),
            });
        let info = vk::AccelerationStructureBuildGeometryInfoKHR::default()
            .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL)
            .flags(self.tlas_flags())
            .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
            .geometries(core::slice::from_ref(&geometry));
        let mut sizes = vk::AccelerationStructureBuildSizesInfoKHR::default();
        // SAFETY: a valid build description; sizes only.
        unsafe {
            self.loader.get_acceleration_structure_build_sizes(
                vk::AccelerationStructureBuildTypeKHR::DEVICE,
                &info,
                &[capacity],
                &mut sizes,
            );
        }
        sizes
    }

    /// Records this frame's bottom-level builds, then a barrier that makes them visible
    /// to the top level's build and to shaders.
    ///
    /// # Safety
    ///
    /// `cmd` is recording outside a rendering scope; `scratch` is aligned and large
    /// enough; the builds' inputs are visible to the build stage.
    unsafe fn record_blas_builds(
        &self,
        device: &engine_rhi::ash::Device,
        cmd: vk::CommandBuffer,
        scratch: u64,
    ) {
        let geometries: Vec<_> = self.builds.iter().map(Self::blas_geometry).collect();
        let infos: Vec<_> = self
            .builds
            .iter()
            .zip(&geometries)
            .map(|(b, g)| {
                vk::AccelerationStructureBuildGeometryInfoKHR::default()
                    .ty(vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL)
                    .flags(BLAS_FLAGS)
                    .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
                    .dst_acceleration_structure(b.accel)
                    .geometries(core::slice::from_ref(g))
                    .scratch_data(vk::DeviceOrHostAddressKHR {
                        device_address: scratch + b.scratch_offset,
                    })
            })
            .collect();
        let ranges: Vec<_> = self
            .builds
            .iter()
            .map(|b| {
                vk::AccelerationStructureBuildRangeInfoKHR::default().primitive_count(b.triangles)
            })
            .collect();
        let range_slices: Vec<&[vk::AccelerationStructureBuildRangeInfoKHR]> =
            ranges.iter().map(core::slice::from_ref).collect();
        let (first, count) = self.build_queries;
        // SAFETY: guaranteed by the caller; every pointer refers to the vectors above.
        unsafe {
            if count > 0 {
                device.cmd_reset_query_pool(cmd, self.queries, first, count);
            }
            self.loader
                .cmd_build_acceleration_structures(cmd, &infos, &range_slices);
            structures_written(device, cmd);
            if count > 0 {
                let built: Vec<_> = self.builds.iter().map(|b| b.accel).collect();
                self.loader.cmd_write_acceleration_structures_properties(
                    cmd,
                    &built,
                    vk::QueryType::ACCELERATION_STRUCTURE_COMPACTED_SIZE_KHR,
                    self.queries,
                    first,
                );
            }
        }
    }
}

/// A barrier after structures are built or copied: visible to later builds and copies
/// (the top level's, compacted-size queries) and to the shaders that trace them.
///
/// # Safety
///
/// `cmd` is recording outside a rendering scope.
unsafe fn structures_written(device: &engine_rhi::ash::Device, cmd: vk::CommandBuffer) {
    let barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ACCELERATION_STRUCTURE_BUILD_KHR)
        .src_access_mask(vk::AccessFlags2::ACCELERATION_STRUCTURE_WRITE_KHR)
        .dst_stage_mask(
            vk::PipelineStageFlags2::ACCELERATION_STRUCTURE_BUILD_KHR
                | vk::PipelineStageFlags2::COMPUTE_SHADER
                | vk::PipelineStageFlags2::FRAGMENT_SHADER
                | vk::PipelineStageFlags2::VERTEX_SHADER,
        )
        .dst_access_mask(vk::AccessFlags2::ACCELERATION_STRUCTURE_READ_KHR);
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().memory_barriers(core::slice::from_ref(&barrier)),
        );
    }
}
