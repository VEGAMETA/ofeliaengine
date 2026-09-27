//! Forward PBR renderer with a directional light and a shadow map (ADR 0014), drawn
//! GPU-driven (ADR 0034) with optional two-phase occlusion culling (ADR 0035), shading
//! glTF materials from a [`MaterialTable`] (ADR 0040).
//!
//! [`ForwardRenderer::add_passes`] turns the draw list into instances and batches and
//! declares these passes in the frame's render graph. A batch is a run of consecutive
//! draws that share a mesh of the [`GeometryPool`] and a pipeline state, drawn as one
//! indexed, instanced draw. There are eight states: opaque or masked (alpha-tested)
//! materials, single- or double-sided, drawn with a mirrored transform or not; batches
//! are ordered by state, opaque first, so every state is one indirect draw call.
//!
//! 1. **cull setup**: copies the batches' indirect draw commands, with no instances yet,
//!    from the upload ring into a graph buffer;
//! 2. **cull**: a compute pass tests every instance's world-space bounding box against the
//!    camera frustum, appends the visible ones to their batch's range of a visible list
//!    and counts them in the batch's command;
//! 3. **shadow**: depth-only rendering of every instance from the light, into a transient
//!    shadow map (with depth bias); masked materials test their alpha;
//! 4. **forward**: metallic-roughness PBR (GGX, height-correlated Smith, Schlick; after
//!    Filament's documentation) with normal maps, occlusion, emission, geometric specular
//!    antialiasing and 3×3 PCF shadows, into the view's color and depth targets.
//!
//! With an [`Occlusion`] state in the [`ForwardView`], culling and drawing run in two
//! phases: **cull early** keeps the instances that were visible in the view's previous
//! frame and **forward early** draws them; **hiz** reduces that depth into a pyramid of
//! farthest depths; **cull late** tests every instance against the frustum and the
//! pyramid, records what is visible for the next frame, and **forward late** draws the
//! visible instances the early phase missed. Objects hidden behind others are not drawn,
//! and nothing visible is ever missing: a wrong guess from the previous frame only costs
//! time.
//!
//! Each drawing pass records one `vkCmdDrawIndexedIndirect` per pipeline state in use, so
//! beyond writing 64 bytes per instance into the [`UploadRing`] the CPU cost of a frame
//! does not grow with the number of draws. Shaders read everything through device
//! addresses and find the shadow map and material textures through the bindless set.
//!
//! [`GeometryPool`]: crate::mesh::GeometryPool
//! [`MaterialTable`]: crate::material::MaterialTable

use crate::cull::Frustum;
use crate::graph::{
    Access, Buffer, BufferDesc, BufferState, FrameGraph, ImportedBuffer, LoadOp, PassBuilder,
    ShaderStages, StoreOp, Texture, TextureDesc,
};
use crate::hiz::{self, DepthPyramid, PyramidLayout};
use crate::material::{MaterialFrame, MaterialId, MaterialKind};
use crate::mesh::{GeometryFrame, MeshId};
use crate::upload::UploadRing;
use core::cell::{Cell, RefCell};
use engine_core::math::{Mat4, Vec3, look_at, vulkan_orthographic};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, DepthBias, Device, GraphicsPipelineDesc, MemoryLocation,
    Pipeline, RhiError, SamplerKind, ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.vertexMain.spv");
const SHADOW_VERTEX: &[u8] = include_bytes!("../shaders/forward.shadowVertex.spv");
const SHADOW_VERTEX_MASKED: &[u8] = include_bytes!("../shaders/forward.shadowVertexMasked.spv");
const SHADOW_FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.shadowFragmentMasked.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.fragmentMain.spv");
const FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.fragmentMasked.spv");
const CULL: &[u8] = include_bytes!("../shaders/forward.cullMain.spv");

/// Threads per workgroup of `cullMain`.
const CULL_GROUP: u32 = 64;

/// `PHASE_*` in `forward.slang`.
const PHASE_ALL: u32 = 0;
const PHASE_EARLY: u32 = 1;
const PHASE_LATE: u32 = 2;

/// Pipeline states of batches, drawn in this order: bit 2 masked, bit 1 double-sided,
/// bit 0 mirrored (negative determinant, so front faces wind clockwise on screen).
const STATES: usize = 8;
const MASKED: usize = 4;
const DOUBLE_SIDED: usize = 2;
const MIRRORED: usize = 1;

/// One object to draw.
///
/// Consecutive items with the same mesh and the same kind of material (opaque or masked,
/// single- or double-sided) are drawn as one instanced batch, so keep the draws of a mesh
/// together; a mirrored transform also starts a batch of its own. With occlusion culling,
/// an item is known by its index in the list, so also keep the order stable from frame to
/// frame. Items of removed meshes draw nothing; items of removed materials draw with the
/// default material.
#[derive(Debug, Clone, Copy)]
pub struct DrawItem {
    /// Its mesh in the geometry pool.
    pub mesh: MeshId,
    /// Its material in the material table.
    pub material: MaterialId,
    /// Object to world transform (affine: the bottom row is ignored). A negative
    /// determinant mirrors the object; its faces keep their orientation (glTF).
    pub world_from_object: Mat4,
}

/// The directional light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectionalLight {
    /// Direction the light travels (from the light into the scene).
    pub direction: Vec3,
    /// Linear color × intensity.
    pub color: Vec3,
    /// Constant ambient term (linear), multiplied by the base color.
    pub ambient: Vec3,
}

/// The camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Clip from world (projection × view).
    pub clip_from_world: Mat4,
    /// World-space position (for specular).
    pub position: Vec3,
}

/// What one [`ForwardRenderer::add_passes`] call renders, and where.
#[derive(Debug, Clone, Copy)]
pub struct ForwardView<'a> {
    /// The camera.
    pub camera: Camera,
    /// The light.
    pub light: DirectionalLight,
    /// Color target.
    pub color: Texture,
    /// Depth target, the size of `color`.
    pub depth: Texture,
    /// Clear color, or `None` to draw over the target's contents.
    pub clear: Option<[f32; 4]>,
    /// This view's occlusion-culling state, or `None` for frustum culling only.
    pub occlusion: Option<&'a Occlusion>,
}

/// Occlusion-culling state of one view, kept across frames (ADR 0035): whether each
/// instance was visible in the view's previous frame.
///
/// Instances are known by their index in the draw list, so keep its order stable. A
/// changed order, a camera cut or more instances than [`Self::capacity`] only cost time
/// (more instances wait for the late phase, or are drawn early for nothing); what is
/// visible is always drawn. Use one state per view, and call [`Self::reset`] after a cut.
#[derive(Debug)]
pub struct Occlusion {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    capacity: u32,
    /// The flags must be cleared before their next use.
    stale: Cell<bool>,
}

impl Occlusion {
    /// State for up to `capacity` instances (4 bytes each, in device memory).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        capacity: u32,
    ) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(u64::from(capacity.max(1)) * 4)
                .usage(
                    vk::BufferUsageFlags::STORAGE_BUFFER
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
            capacity,
            stale: Cell::new(true),
        })
    }

    /// Instances whose visibility is remembered.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Forgets which instances were visible (after a camera cut or a new scene): the next
    /// frame tests every instance in its late phase.
    pub fn reset(&self) {
        self.stale.set(true);
    }

    /// Frees the buffer.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// `Frame` in `forward.slang` (384 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct FrameData {
    clip_from_world: [f32; 16],
    light_clip_from_world: [f32; 16],
    camera_position: [f32; 4],
    to_light: [f32; 4],
    light_color: [f32; 4],
    ambient: [f32; 4],
    planes: [[f32; 4]; 6],
    shadow_sampler: u32,
    cull: u32,
    visibility_capacity: u32,
    hiz_levels: u32,
    depth_size: [u32; 2],
    _pad: [u32; 2],
    hiz_offsets: [u32; hiz::MAX_LEVELS],
}

/// `Instance` in `forward.slang` (64 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct InstanceData {
    rows: [[f32; 4]; 3],
    material: u32,
    batch: u32,
    _pad: [u32; 2],
}

/// `Batch` in `forward.slang` (48 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct BatchData {
    first: u32,
    _pad: [u32; 3],
    center: [f32; 4],
    extents: [f32; 4],
}

/// `Push` in `forward.slang` (96 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct Push {
    frame: u64,
    instances: u64,
    batches: u64,
    visible: u64,
    commands: u64,
    visibility: u64,
    pyramid: u64,
    positions: u64,
    attributes: u64,
    materials: u64,
    instance_count: u32,
    texture: u32,
    phase: u32,
    _pad: u32,
}

const _: () = assert!(core::mem::size_of::<FrameData>() == 384);
const _: () = assert!(core::mem::size_of::<InstanceData>() == 64);
const _: () = assert!(core::mem::size_of::<BatchData>() == 48);
const _: () = assert!(core::mem::size_of::<Push>() == 96);
const COMMAND_BYTES: u64 = core::mem::size_of::<vk::DrawIndexedIndirectCommand>() as u64;
const _: () = assert!(COMMAND_BYTES == 20);

/// Shadow map settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowSettings {
    /// Width and height in texels.
    pub resolution: u32,
    /// Depth format.
    pub format: vk::Format,
}

impl Default for ShadowSettings {
    fn default() -> Self {
        Self {
            resolution: 2048,
            format: vk::Format::D32_SFLOAT,
        }
    }
}

/// What [`ForwardRenderer::add_passes`] declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardPasses {
    /// Instances (one per draw item), all tested by the culling pass.
    pub instances: u32,
    /// Batches: runs of consecutive draws that share a mesh and a pipeline state.
    pub batches: u32,
    /// Whether culling ran in two phases (occlusion culling).
    pub occlusion: bool,
    /// The forward pass's indirect draw commands, one `vk::DrawIndexedIndirectCommand` (20
    /// bytes) per batch, ordered by pipeline state and within a state in draw order; with
    /// occlusion culling, the early phase's, then the late phase's.
    /// After culling, each `instance_count` is the number of instances its phase drew of
    /// its batch: read it back to count what was drawn. `None` when there was nothing to
    /// draw.
    pub commands: Option<Buffer>,
}

/// A batch while the draw list is turned into batches.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// First draw.
    first: u32,
    len: u32,
    /// Pipeline state (`MASKED | DOUBLE_SIDED | MIRRORED` bits).
    state: usize,
    /// Position among the batches ordered by state.
    order: u32,
}

/// Per-frame working memory, kept to avoid allocating every frame.
#[derive(Debug, Default)]
struct Scratch {
    /// Material slot of every draw.
    slots: Vec<u32>,
    runs: Vec<Run>,
    /// Sort keys of the runs: state, then distance from the camera, then run index.
    keys: Vec<(u64, u32)>,
}

/// The forward PBR pipelines.
#[derive(Debug)]
pub struct ForwardRenderer {
    /// Forward shading: opaque, masked.
    main: [Pipeline; 2],
    /// Shadow map: opaque (depth only), masked.
    shadow: [Pipeline; 2],
    cull: Pipeline,
    pyramid: DepthPyramid,
    shadow_settings: ShadowSettings,
    frustum_culling: bool,
    scratch: RefCell<Scratch>,
}

fn push_bytes(push: &Push) -> &[u8] {
    // SAFETY: `Push` is plain `repr(C)` data without padding (asserted size).
    unsafe {
        core::slice::from_raw_parts(
            (push as *const Push).cast::<u8>(),
            core::mem::size_of::<Push>(),
        )
    }
}

impl ForwardRenderer {
    /// Creates the pipelines for rendering into `color_format` with a `depth_format`
    /// depth buffer.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
        shadow_settings: ShadowSettings,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let shadow_vertex = spirv_words(SHADOW_VERTEX).ok_or_else(invalid)?;
        let shadow_vertex_masked = spirv_words(SHADOW_VERTEX_MASKED).ok_or_else(invalid)?;
        let shadow_fragment_masked = spirv_words(SHADOW_FRAGMENT_MASKED).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT).ok_or_else(invalid)?;
        let fragment_masked = spirv_words(FRAGMENT_MASKED).ok_or_else(invalid)?;
        let formats = [color_format];
        // Cull mode and front face come with each batch's pipeline state.
        let main = |fragment: &[u32], entry| {
            Pipeline::graphics(
                device,
                bindless.pipeline_layout(),
                &GraphicsPipelineDesc {
                    color_formats: &formats,
                    depth_format,
                    dynamic_culling: true,
                    ..GraphicsPipelineDesc::new(
                        ShaderStageDesc {
                            spirv: &vertex,
                            entry: c"vertexMain",
                        },
                        Some(ShaderStageDesc {
                            spirv: fragment,
                            entry,
                        }),
                    )
                },
                vk::PipelineCache::null(),
            )
        };
        let shadow = |vertex: &[u32], vertex_entry, fragment: Option<ShaderStageDesc<'_>>| {
            Pipeline::graphics(
                device,
                bindless.pipeline_layout(),
                &GraphicsPipelineDesc {
                    depth_format: shadow_settings.format,
                    dynamic_culling: true,
                    depth_bias: Some(DepthBias {
                        constant: 1.0,
                        slope: 1.5,
                        clamp: 0.0,
                    }),
                    ..GraphicsPipelineDesc::new(
                        ShaderStageDesc {
                            spirv: vertex,
                            entry: vertex_entry,
                        },
                        fragment,
                    )
                },
                vk::PipelineCache::null(),
            )
        };
        let main = [
            main(&fragment, c"fragmentMain")?,
            main(&fragment_masked, c"fragmentMasked")?,
        ];
        let shadow = [
            shadow(&shadow_vertex, c"shadowVertex", None)?,
            shadow(
                &shadow_vertex_masked,
                c"shadowVertexMasked",
                Some(ShaderStageDesc {
                    spirv: &shadow_fragment_masked,
                    entry: c"shadowFragmentMasked",
                }),
            )?,
        ];
        let compute = |spirv: &[u8], entry| -> Result<Pipeline, RhiError> {
            let words = spirv_words(spirv).ok_or_else(invalid)?;
            Pipeline::compute(
                device,
                bindless.pipeline_layout(),
                ShaderStageDesc {
                    spirv: &words,
                    entry,
                },
                vk::PipelineCache::null(),
            )
        };
        Ok(Self {
            main,
            shadow,
            cull: compute(CULL, c"cullMain")?,
            pyramid: DepthPyramid::new(device, bindless)?,
            shadow_settings,
            frustum_culling: true,
            scratch: RefCell::new(Scratch::default()),
        })
    }

    /// Turns GPU culling on (the default) or off. Off, every instance counts as visible,
    /// with or without an [`Occlusion`] state: for comparisons and debugging.
    pub fn set_frustum_culling(&mut self, enabled: bool) {
        self.frustum_culling = enabled;
    }

    /// Whether GPU culling is on.
    pub fn frustum_culling(&self) -> bool {
        self.frustum_culling
    }

    /// Declares the culling, shadow and forward passes for `draws` into the view's
    /// targets, drawing from `geometry` (this frame's [`GeometryPool`] buffers) and
    /// shading with `materials` (this frame's [`MaterialTable`]).
    ///
    /// Instances, batches, draw commands and frame constants are written into `ring` now;
    /// the passes record when the graph executes, which must be with this `bindless` set
    /// ([`FrameGraph::execute_with_bindless`]) so the shadow map and the depth target get
    /// indices. Returns `None`, declaring nothing, when `ring` is full.
    ///
    /// [`GeometryPool`]: crate::mesh::GeometryPool
    /// [`MaterialTable`]: crate::material::MaterialTable
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        draws: &[DrawItem],
        view: &ForwardView<'g>,
    ) -> Option<ForwardPasses> {
        engine_profiler::zone!("render::forward::add_passes");
        let (color, depth) = (view.color, view.depth);
        let load = view.clear.map_or(LoadOp::Load, LoadOp::Clear);
        let depth_clear = LoadOp::Clear([1.0, 0.0, 0.0, 0.0]);
        if draws.is_empty() {
            fg.add_pass("forward")
                .color(color, load, StoreOp::Store)
                .depth(depth, depth_clear, StoreOp::DontCare)
                .execute(|_| {});
            return Some(ForwardPasses {
                instances: 0,
                batches: 0,
                occlusion: false,
                commands: None,
            });
        }
        let instance_count = draws.len() as u32;
        let mut scratch = self.scratch.borrow_mut();
        let Scratch { slots, runs, keys } = &mut *scratch;
        // Batches in draw order, counted per pipeline state, then ordered by state and,
        // within a state, front to back (by the first instance's box), so the depth test
        // rejects hidden fragments before shading.
        let mut counts = [0u32; STATES];
        batches(draws, materials, slots, runs, &mut counts);
        let mut starts = [0u32; STATES];
        let mut next = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = next;
            next += count;
        }
        keys.clear();
        keys.extend(runs.iter().enumerate().map(|(k, run)| {
            let draw = &draws[run.first as usize];
            let (center, extents) = local_box(geometry, draw.mesh);
            let (center, _) = crate::cull::transform_box(&draw.world_from_object, center, extents);
            let distance = center.distance(view.camera.position);
            (
                ((run.state as u64) << 32) | u64::from(distance.to_bits()),
                k as u32,
            )
        }));
        keys.sort_unstable();
        for (order, &(_, k)) in keys.iter().enumerate() {
            runs[k as usize].order = order as u32;
        }
        let batch_count = runs.len() as u32;
        let depth_desc = fg.texture_desc(depth);
        let occlusion = view
            .occlusion
            .filter(|_| self.frustum_culling && depth_desc.width > 0 && depth_desc.height > 0);
        let phases: u32 = if occlusion.is_some() { 2 } else { 1 };

        // Instances in draw order, and the scene's bounding box for the light on the way.
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        let instances = {
            let (out, staged) = ring.alloc::<InstanceData>(draws.len())?;
            for run in runs.iter() {
                let (center, extents) = local_box(geometry, draws[run.first as usize].mesh);
                let range = run.first as usize..(run.first + run.len) as usize;
                let items = draws[range.clone()].iter().zip(&slots[range.clone()]);
                for ((item, &material), slot) in items.zip(&mut out[range]) {
                    let (world_center, world_extents) =
                        crate::cull::transform_box(&item.world_from_object, center, extents);
                    min = min.min(world_center - world_extents);
                    max = max.max(world_center + world_extents);
                    let rows = item.world_from_object.transpose();
                    slot.write(InstanceData {
                        rows: [
                            rows.x_axis.to_array(),
                            rows.y_axis.to_array(),
                            rows.z_axis.to_array(),
                        ],
                        material,
                        batch: run.order,
                        _pad: [0; 2],
                    });
                }
            }
            staged.address
        };
        let batch_table = {
            let (out, staged) = ring.alloc::<BatchData>(batch_count as usize)?;
            for run in runs.iter() {
                let (center, extents) = local_box(geometry, draws[run.first as usize].mesh);
                out[run.order as usize].write(BatchData {
                    first: run.first,
                    _pad: [0; 3],
                    center: center.extend(0.0).to_array(),
                    extents: extents.extend(0.0).to_array(),
                });
            }
            staged.address
        };
        // Draw commands, in state order: the forward pass's for each phase, with no
        // instances until culling counts them (the late phase's visible list follows the
        // early one's), then the shadow pass's, with every instance.
        let commands = {
            let per_phase = batch_count as usize;
            let (out, staged) =
                ring.alloc::<vk::DrawIndexedIndirectCommand>((phases as usize + 1) * per_phase)?;
            for run in runs.iter() {
                // A removed mesh draws nothing.
                let mesh = geometry.mesh(draws[run.first as usize].mesh);
                let command = vk::DrawIndexedIndirectCommand {
                    index_count: mesh.map_or(0, |m| m.index_count),
                    instance_count: 0,
                    first_index: mesh.map_or(0, |m| m.first_index),
                    vertex_offset: mesh.map_or(0, |m| m.first_vertex as i32),
                    first_instance: run.first,
                };
                let b = run.order as usize;
                for phase in 0..phases as usize {
                    out[phase * per_phase + b].write(vk::DrawIndexedIndirectCommand {
                        first_instance: command.first_instance + phase as u32 * instance_count,
                        ..command
                    });
                }
                out[phases as usize * per_phase + b].write(vk::DrawIndexedIndirectCommand {
                    instance_count: run.len,
                    ..command
                });
            }
            staged
        };
        drop(scratch);
        let states = States { starts, counts };
        let pyramid_layout = PyramidLayout::new(depth_desc.width, depth_desc.height);
        let frame = FrameData {
            clip_from_world: view.camera.clip_from_world.to_cols_array(),
            light_clip_from_world: fit_light(min, max, view.light.direction).to_cols_array(),
            camera_position: view.camera.position.extend(1.0).to_array(),
            to_light: (-view.light.direction.normalize_or(Vec3::NEG_Y))
                .extend(0.0)
                .to_array(),
            light_color: view.light.color.extend(0.0).to_array(),
            ambient: view.light.ambient.extend(0.0).to_array(),
            planes: Frustum::from_clip_from_world(view.camera.clip_from_world)
                .planes
                .map(|p| p.to_array()),
            shadow_sampler: Bindless::sampler(SamplerKind::ShadowCompare),
            cull: u32::from(self.frustum_culling),
            visibility_capacity: occlusion.map_or(0, Occlusion::capacity),
            hiz_levels: pyramid_layout.levels,
            depth_size: [depth_desc.width, depth_desc.height],
            _pad: [0; 2],
            hiz_offsets: pyramid_layout.offsets,
        };
        let frame = ring.push(&[frame])?;
        let base = Push {
            frame,
            instances,
            batches: batch_table,
            visible: 0,
            commands: 0,
            visibility: occlusion.map_or(0, |o| o.address),
            pyramid: 0,
            positions: geometry.position_address(),
            attributes: geometry.attribute_address(),
            materials: materials.address(),
            instance_count,
            texture: 0,
            phase: PHASE_ALL,
            _pad: 0,
        };

        let command_bytes = u64::from(batch_count) * COMMAND_BYTES;
        let culled = fg.create_buffer(
            "draw commands",
            BufferDesc {
                size: u64::from(phases) * command_bytes,
            },
        );
        let visible_bytes = u64::from(instance_count) * 4;
        let visible = fg.create_buffer(
            "visible instances",
            BufferDesc {
                size: u64::from(phases) * visible_bytes,
            },
        );
        let flags = occlusion.map(|o| {
            // Flags written by the previous frame's late phase, or to be cleared now.
            let stale = o.stale.replace(false);
            let written = BufferState {
                stages: vk::PipelineStageFlags2::COMPUTE_SHADER,
                access: vk::AccessFlags2::SHADER_STORAGE_READ
                    | vk::AccessFlags2::SHADER_STORAGE_WRITE,
            };
            let buffer = fg.import_buffer(
                "visibility flags",
                ImportedBuffer {
                    buffer: o.buffer,
                    size: u64::from(o.capacity.max(1)) * 4,
                    address: o.address,
                    initial: if stale { BufferState::idle() } else { written },
                    final_state: Some(written),
                },
            );
            (buffer, stale)
        });
        let mut setup = fg
            .add_pass("cull setup")
            .write(culled, Access::TransferWrite);
        if let Some((flags, true)) = flags {
            setup = setup.write(flags, Access::TransferWrite);
        }
        setup.execute(move |ctx| {
            let region = vk::BufferCopy {
                src_offset: commands.offset,
                dst_offset: 0,
                size: u64::from(phases) * command_bytes,
            };
            // SAFETY: recording outside a rendering scope; the ranges are in bounds.
            unsafe {
                ctx.device
                    .cmd_copy_buffer(ctx.cmd, commands.buffer, ctx.buffer(culled), &[region]);
                if let Some((flags, true)) = flags {
                    ctx.device
                        .cmd_fill_buffer(ctx.cmd, ctx.buffer(flags), 0, vk::WHOLE_SIZE, 0);
                }
            }
        });
        let flag_buffer = flags.map(|(buffer, _)| buffer);
        let dispatch_cull = move |ctx: &mut crate::graph::PassContext<'_>, push: Push| {
            // SAFETY: recording outside a rendering scope.
            unsafe {
                self.cull.bind(ctx.cmd);
                bindless.bind(ctx.cmd, vk::PipelineBindPoint::COMPUTE);
                bindless.push_constants(ctx.cmd, push_bytes(&push));
                ctx.device
                    .cmd_dispatch(ctx.cmd, instance_count.div_ceil(CULL_GROUP), 1, 1);
            }
        };
        let first_phase = if occlusion.is_some() {
            PHASE_EARLY
        } else {
            PHASE_ALL
        };
        let name = if occlusion.is_some() {
            "cull early"
        } else {
            "cull"
        };
        cull_pass(fg, name, culled, visible, flag_buffer, first_phase).execute(move |ctx| {
            let push = Push {
                visible: ctx.address(visible),
                commands: ctx.address(culled),
                phase: first_phase,
                ..base
            };
            dispatch_cull(ctx, push);
        });

        let resolution = self.shadow_settings.resolution;
        let shadow_map = fg.create_texture(
            "shadow map",
            TextureDesc::new_2d(self.shadow_settings.format, resolution, resolution),
        );
        let shadow_pipelines = &self.shadow;
        let geometry = *geometry;
        let materials = *materials;
        materials
            .read(geometry.read(fg.add_pass("shadow")))
            .depth(shadow_map, depth_clear, StoreOp::Store)
            .execute(move |ctx| {
                // SAFETY: recording inside the pass's rendering scope; the commands are in
                // the ring, which has indirect-buffer usage; the pass reads the pool and
                // the material table.
                unsafe {
                    states.draw(
                        ctx,
                        bindless,
                        &geometry,
                        shadow_pipelines,
                        &base,
                        true,
                        commands.buffer,
                        commands.offset + u64::from(phases) * command_bytes,
                    );
                }
            });
        let main_pipelines = &self.main;
        let draw_phase = move |ctx: &mut crate::graph::PassContext<'_>, phase: u32| {
            let push = Push {
                visible: ctx.address(visible),
                texture: ctx.sampled_index(shadow_map).unwrap_or(0),
                ..base
            };
            let offset = u64::from(phase.saturating_sub(1)) * command_bytes;
            // SAFETY: recording inside the pass's rendering scope; the pass reads the pool
            // and the material table.
            unsafe {
                states.draw(
                    ctx,
                    bindless,
                    &geometry,
                    main_pipelines,
                    &push,
                    false,
                    ctx.buffer(culled),
                    offset,
                );
            }
        };
        let Some(flags) = flag_buffer else {
            forward_pass(
                fg, "forward", &geometry, &materials, shadow_map, culled, visible,
            )
            .color(color, load, StoreOp::Store)
            .depth(depth, depth_clear, StoreOp::DontCare)
            .execute(move |ctx| draw_phase(ctx, PHASE_ALL));
            return Some(ForwardPasses {
                instances: instance_count,
                batches: batch_count,
                occlusion: false,
                commands: Some(culled),
            });
        };

        // Two phases: draw what was visible, build the pyramid from its depth, then test
        // everything against it and draw what the first phase missed.
        forward_pass(
            fg,
            "forward early",
            &geometry,
            &materials,
            shadow_map,
            culled,
            visible,
        )
        .color(color, load, StoreOp::Store)
        .depth(depth, depth_clear, StoreOp::Store)
        .execute(move |ctx| draw_phase(ctx, PHASE_EARLY));
        let pyramid = fg.create_buffer(
            "depth pyramid",
            BufferDesc {
                size: pyramid_layout.bytes(),
            },
        );
        self.pyramid.add_pass(fg, bindless, depth, pyramid);
        cull_pass(fg, "cull late", culled, visible, Some(flags), PHASE_LATE)
            .read(pyramid, Access::StorageRead(ShaderStages::COMPUTE))
            .execute(move |ctx| {
                let push = Push {
                    visible: ctx.address(visible) + visible_bytes,
                    commands: ctx.address(culled) + command_bytes,
                    pyramid: ctx.address(pyramid),
                    phase: PHASE_LATE,
                    ..base
                };
                dispatch_cull(ctx, push);
            });
        forward_pass(
            fg,
            "forward late",
            &geometry,
            &materials,
            shadow_map,
            culled,
            visible,
        )
        .color(color, LoadOp::Load, StoreOp::Store)
        .depth(depth, LoadOp::Load, StoreOp::DontCare)
        .execute(move |ctx| draw_phase(ctx, PHASE_LATE));
        Some(ForwardPasses {
            instances: instance_count,
            batches: batch_count,
            occlusion: true,
            commands: Some(culled),
        })
    }
}

/// A culling pass of `phase` with its buffer accesses (`flags`: occlusion culling's
/// visibility flags, read early and rewritten late).
fn cull_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    culled: Buffer,
    visible: Buffer,
    flags: Option<Buffer>,
    phase: u32,
) -> PassBuilder<'a, 'g> {
    let pass = fg
        .add_pass(name)
        .read(culled, Access::StorageReadWrite(ShaderStages::COMPUTE))
        .write(visible, Access::StorageWrite(ShaderStages::COMPUTE));
    match flags {
        Some(flags) if phase == PHASE_LATE => {
            pass.read(flags, Access::StorageReadWrite(ShaderStages::COMPUTE))
        }
        Some(flags) => pass.read(flags, Access::StorageRead(ShaderStages::COMPUTE)),
        None => pass,
    }
}

/// A forward pass that draws the culled commands, without its attachments.
fn forward_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    geometry: &GeometryFrame<'_>,
    materials: &MaterialFrame<'_>,
    shadow_map: Texture,
    culled: Buffer,
    visible: Buffer,
) -> PassBuilder<'a, 'g> {
    materials
        .read(geometry.read(fg.add_pass(name)))
        .read(shadow_map, Access::Sampled(ShaderStages::FRAGMENT))
        .read(culled, Access::Indirect)
        .read(visible, Access::StorageRead(ShaderStages::VERTEX))
}

/// Whether a transform mirrors (negative determinant of its linear part).
fn mirrored(m: &Mat4) -> bool {
    m.x_axis
        .truncate()
        .cross(m.y_axis.truncate())
        .dot(m.z_axis.truncate())
        < 0.0
}

/// A batch's pipeline state.
fn state(kind: MaterialKind, mirrored: bool) -> usize {
    (usize::from(kind.masked) * MASKED)
        | (usize::from(kind.double_sided) * DOUBLE_SIDED)
        | (usize::from(mirrored) * MIRRORED)
}

/// Fills `slots` with every draw's material slot and `runs` with the runs of consecutive
/// draws that share a mesh and a pipeline state; counts the runs per state.
fn batches(
    draws: &[DrawItem],
    materials: &MaterialFrame<'_>,
    slots: &mut Vec<u32>,
    runs: &mut Vec<Run>,
    counts: &mut [u32; STATES],
) {
    slots.clear();
    runs.clear();
    let mut mesh = None;
    for (i, draw) in draws.iter().enumerate() {
        let (slot, kind) = materials.resolve(draw.material);
        let state = state(kind, mirrored(&draw.world_from_object));
        slots.push(slot);
        match runs.last_mut() {
            Some(run) if run.state == state && mesh == Some(draw.mesh) => run.len += 1,
            _ => {
                runs.push(Run {
                    first: i as u32,
                    len: 1,
                    state,
                    order: 0,
                });
                counts[state] += 1;
                mesh = Some(draw.mesh);
            }
        }
    }
}

/// Where each pipeline state's batches are among the commands.
#[derive(Debug, Clone, Copy)]
struct States {
    starts: [u32; STATES],
    counts: [u32; STATES],
}

impl States {
    /// Records one indirect draw per pipeline state in use, from the commands at `offset`
    /// in `buffer` (one per batch, in state order): `pipelines` are the opaque and the
    /// masked one. The shadow pass renders back faces of single-sided geometry (fewer
    /// self-shadowing artifacts on closed meshes); the forward passes cull them.
    ///
    /// # Safety
    ///
    /// `ctx` records inside a rendering scope of a pass that reads the geometry pool and
    /// the material table; `buffer` holds the commands and has indirect-buffer usage.
    #[allow(clippy::too_many_arguments)]
    unsafe fn draw(
        &self,
        ctx: &crate::graph::PassContext<'_>,
        bindless: &Bindless,
        geometry: &GeometryFrame<'_>,
        pipelines: &[Pipeline; 2],
        push: &Push,
        shadow: bool,
        buffer: vk::Buffer,
        offset: u64,
    ) {
        let mut bound = None;
        for state in 0..STATES {
            let count = self.counts[state];
            if count == 0 {
                continue;
            }
            let masked = state & MASKED != 0;
            let cull = match (state & DOUBLE_SIDED != 0, shadow) {
                (true, _) => vk::CullModeFlags::NONE,
                (false, true) => vk::CullModeFlags::FRONT,
                (false, false) => vk::CullModeFlags::BACK,
            };
            let front = if state & MIRRORED != 0 {
                vk::FrontFace::CLOCKWISE
            } else {
                vk::FrontFace::COUNTER_CLOCKWISE
            };
            // SAFETY: guaranteed by the caller.
            unsafe {
                if bound != Some(masked) {
                    pipelines[usize::from(masked)].bind(ctx.cmd);
                    if bound.is_none() {
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        set_viewport(ctx.device, ctx.cmd, ctx.render_area());
                        bindless.push_constants(ctx.cmd, push_bytes(push));
                        geometry.bind_indices(ctx);
                    }
                    bound = Some(masked);
                }
                ctx.device.cmd_set_cull_mode(ctx.cmd, cull);
                ctx.device.cmd_set_front_face(ctx.cmd, front);
                ctx.device.cmd_draw_indexed_indirect(
                    ctx.cmd,
                    buffer,
                    offset + u64::from(self.starts[state]) * COMMAND_BYTES,
                    count,
                    COMMAND_BYTES as u32,
                );
            }
        }
    }
}

/// A mesh's object-space bounding box as center and half extents (empty for removed
/// meshes).
fn local_box(geometry: &GeometryFrame<'_>, mesh: MeshId) -> (Vec3, Vec3) {
    geometry.mesh(mesh).map_or((Vec3::ZERO, Vec3::ZERO), |m| {
        ((m.min + m.max) * 0.5, (m.max - m.min) * 0.5)
    })
}

/// # Safety
///
/// `cmd` is recording.
unsafe fn set_viewport(device: &engine_rhi::ash::Device, cmd: vk::CommandBuffer, area: vk::Rect2D) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_set_viewport(
            cmd,
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
        device.cmd_set_scissor(cmd, 0, &[area]);
    }
}

/// An orthographic light projection that contains the scene's bounding box.
fn fit_light(min: Vec3, max: Vec3, direction: Vec3) -> Mat4 {
    let direction = direction.normalize_or(Vec3::NEG_Y);
    let center = (min + max) * 0.5;
    let radius = ((max - min).length() * 0.5).max(1e-3);
    let up = if direction.y.abs() > 0.99 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let view = look_at(center - direction * radius * 2.0, center, up);
    vulkan_orthographic(-radius, radius, -radius, radius, radius * 0.5, radius * 3.5) * view
}
