//! Forward PBR renderer with a directional light and a shadow map (ADR 0014), drawn
//! GPU-driven (ADR 0034) with optional two-phase occlusion culling (ADR 0035).
//!
//! [`ForwardRenderer::add_passes`] turns the draw list into instances and batches (a batch
//! is a run of consecutive draws that share a mesh, drawn as one instanced draw) and
//! declares these passes in the frame's render graph:
//!
//! 1. **cull setup**: copies the batches' indirect draw commands, with no instances yet,
//!    from the upload ring into a graph buffer;
//! 2. **cull**: a compute pass tests every instance's world-space bounding box against the
//!    camera frustum, appends the visible ones to their batch's range of a visible list
//!    and counts them in the batch's command;
//! 3. **shadow**: depth-only rendering of every instance from the light, into a transient
//!    shadow map (with depth bias);
//! 4. **forward**: metallic-roughness PBR (GGX, height-correlated Smith, Schlick; after
//!    Filament's documentation) with 3×3 PCF shadows, into the view's color and depth
//!    targets.
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
//! Each drawing pass records a single `vkCmdDrawIndirect` for all batches, so beyond
//! writing 80 bytes per instance into the [`UploadRing`] the CPU cost of a frame does not
//! grow with the number of draws. Shaders read everything through device addresses and
//! find the shadow map through the bindless set (its index is assigned by the graph).

use crate::cull::Frustum;
use crate::graph::{
    Access, Buffer, BufferDesc, BufferState, FrameGraph, ImportedBuffer, LoadOp, PassBuilder,
    ShaderStages, StoreOp, Texture, TextureDesc,
};
use crate::hiz::{self, DepthPyramid, PyramidLayout};
use crate::mesh::Mesh;
use crate::upload::UploadRing;
use core::cell::Cell;
use engine_core::math::{Mat4, Vec3, look_at, vulkan_orthographic};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, DepthBias, Device, GraphicsPipelineDesc, MemoryLocation,
    Pipeline, RhiError, SamplerKind, ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.vertexMain.spv");
const SHADOW_VERTEX: &[u8] = include_bytes!("../shaders/forward.shadowVertex.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.fragmentMain.spv");
const CULL: &[u8] = include_bytes!("../shaders/forward.cullMain.spv");

/// Threads per workgroup of `cullMain`.
const CULL_GROUP: u32 = 64;

/// `PHASE_*` in `forward.slang`.
const PHASE_ALL: u32 = 0;
const PHASE_EARLY: u32 = 1;
const PHASE_LATE: u32 = 2;

/// Surface parameters (metallic-roughness).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Material {
    /// Linear base color.
    pub base_color: Vec3,
    /// 0 dielectric … 1 metal.
    pub metallic: f32,
    /// Perceptual roughness, 0 (mirror) … 1.
    pub roughness: f32,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            base_color: Vec3::splat(0.8),
            metallic: 0.0,
            roughness: 0.5,
        }
    }
}

/// One object to draw.
///
/// Consecutive items with the same mesh (the same `&Mesh`) are drawn as one instanced
/// batch, so keep the draws of a mesh together. With occlusion culling, an item is known
/// by its index in the list, so also keep the order stable from frame to frame.
#[derive(Debug, Clone, Copy)]
pub struct DrawItem<'a> {
    /// Its vertices.
    pub mesh: &'a Mesh,
    /// Object to world transform (affine: the bottom row is ignored).
    pub world_from_object: Mat4,
    /// Its surface.
    pub material: Material,
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

/// `Instance` in `forward.slang` (80 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct InstanceData {
    rows: [[f32; 4]; 3],
    base_color_metallic: [f32; 4],
    roughness: f32,
    batch: u32,
    _pad: [u32; 2],
}

/// `Batch` in `forward.slang` (48 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct BatchData {
    vertices: u64,
    vertex_count: u32,
    first: u32,
    center: [f32; 4],
    extents: [f32; 4],
}

/// `Push` in `forward.slang` (72 bytes).
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
    instance_count: u32,
    texture: u32,
    phase: u32,
    _pad: u32,
}

const _: () = assert!(core::mem::size_of::<FrameData>() == 384);
const _: () = assert!(core::mem::size_of::<InstanceData>() == 80);
const _: () = assert!(core::mem::size_of::<BatchData>() == 48);
const _: () = assert!(core::mem::size_of::<Push>() == 72);
const COMMAND_BYTES: u64 = core::mem::size_of::<vk::DrawIndirectCommand>() as u64;
const _: () = assert!(COMMAND_BYTES == 16);

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
    /// Batches: runs of consecutive draws that share a mesh, one indirect draw each.
    pub batches: u32,
    /// Whether culling ran in two phases (occlusion culling).
    pub occlusion: bool,
    /// The forward pass's indirect draw commands, one `vk::DrawIndirectCommand` per batch
    /// in draw order; with occlusion culling, the early phase's, then the late phase's.
    /// After culling, each `instance_count` is the number of instances its phase drew of
    /// its batch: read it back to count what was drawn. `None` when there was nothing to
    /// draw.
    pub commands: Option<Buffer>,
}

/// The forward PBR pipelines.
#[derive(Debug)]
pub struct ForwardRenderer {
    main: Pipeline,
    shadow: Pipeline,
    cull: Pipeline,
    pyramid: DepthPyramid,
    shadow_settings: ShadowSettings,
    frustum_culling: bool,
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
        let fragment = spirv_words(FRAGMENT).ok_or_else(invalid)?;
        let formats = [color_format];
        let main = Pipeline::graphics(
            device,
            bindless.pipeline_layout(),
            &GraphicsPipelineDesc {
                color_formats: &formats,
                depth_format,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"vertexMain",
                    },
                    Some(ShaderStageDesc {
                        spirv: &fragment,
                        entry: c"fragmentMain",
                    }),
                )
            },
            vk::PipelineCache::null(),
        )?;
        let shadow = Pipeline::graphics(
            device,
            bindless.pipeline_layout(),
            &GraphicsPipelineDesc {
                depth_format: shadow_settings.format,
                // Render back faces into the shadow map: fewer self-shadowing artifacts on
                // closed meshes.
                cull_mode: vk::CullModeFlags::FRONT,
                depth_bias: Some(DepthBias {
                    constant: 1.0,
                    slope: 1.5,
                    clamp: 0.0,
                }),
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &shadow_vertex,
                        entry: c"shadowVertex",
                    },
                    None,
                )
            },
            vk::PipelineCache::null(),
        )?;
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
    /// targets.
    ///
    /// Instances, batches, draw commands and frame constants are written into `ring` now;
    /// the passes record when the graph executes, which must be with this `bindless` set
    /// ([`FrameGraph::execute_with_bindless`]) so the shadow map and the depth target get
    /// indices. Returns `None`, declaring nothing, when `ring` is full.
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        draws: &'g [DrawItem<'g>],
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
        let batch_count = batches(draws).count() as u32;
        let depth_desc = fg.texture_desc(depth);
        let occlusion = view
            .occlusion
            .filter(|_| self.frustum_culling && depth_desc.width > 0 && depth_desc.height > 0);
        let phases: u32 = if occlusion.is_some() { 2 } else { 1 };

        // Instances in draw order, and the scene's bounding box for the light on the way.
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        let instances = {
            let (slots, staged) = ring.alloc::<InstanceData>(draws.len())?;
            for (batch, (first, len)) in batches(draws).enumerate() {
                let (center, extents) = local_box(draws[first].mesh);
                let items = &draws[first..first + len];
                for (item, slot) in items.iter().zip(&mut slots[first..first + len]) {
                    let (world_center, world_extents) =
                        crate::cull::transform_box(&item.world_from_object, center, extents);
                    min = min.min(world_center - world_extents);
                    max = max.max(world_center + world_extents);
                    let rows = item.world_from_object.transpose();
                    let m = item.material;
                    slot.write(InstanceData {
                        rows: [
                            rows.x_axis.to_array(),
                            rows.y_axis.to_array(),
                            rows.z_axis.to_array(),
                        ],
                        base_color_metallic: m.base_color.extend(m.metallic).to_array(),
                        roughness: m.roughness,
                        batch: batch as u32,
                        _pad: [0; 2],
                    });
                }
            }
            staged.address
        };
        let batch_table = {
            let (slots, staged) = ring.alloc::<BatchData>(batch_count as usize)?;
            for ((first, _), slot) in batches(draws).zip(slots.iter_mut()) {
                let mesh = draws[first].mesh;
                let (center, extents) = local_box(mesh);
                slot.write(BatchData {
                    vertices: mesh.address(),
                    vertex_count: mesh.vertex_count(),
                    first: first as u32,
                    center: center.extend(0.0).to_array(),
                    extents: extents.extend(0.0).to_array(),
                });
            }
            staged.address
        };
        // Draw commands: the forward pass's for each phase, with no instances until
        // culling counts them (the late phase's visible list follows the early one's),
        // then the shadow pass's, with every instance.
        let commands = {
            let per_phase = batch_count as usize;
            let (slots, staged) =
                ring.alloc::<vk::DrawIndirectCommand>((phases as usize + 1) * per_phase)?;
            for (b, (first, len)) in batches(draws).enumerate() {
                let command = vk::DrawIndirectCommand {
                    vertex_count: draws[first].mesh.vertex_count(),
                    instance_count: 0,
                    first_vertex: 0,
                    first_instance: first as u32,
                };
                for phase in 0..phases as usize {
                    slots[phase * per_phase + b].write(vk::DrawIndirectCommand {
                        first_instance: command.first_instance + phase as u32 * instance_count,
                        ..command
                    });
                }
                slots[phases as usize * per_phase + b].write(vk::DrawIndirectCommand {
                    instance_count: len as u32,
                    ..command
                });
            }
            staged
        };
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
        let shadow_pipeline = &self.shadow;
        fg.add_pass("shadow")
            .depth(shadow_map, depth_clear, StoreOp::Store)
            .execute(move |ctx| {
                let area = ctx.render_area();
                // SAFETY: recording inside the pass's rendering scope; the commands are in
                // the ring, which has indirect-buffer usage.
                unsafe {
                    shadow_pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_viewport(ctx.device, ctx.cmd, area);
                    bindless.push_constants(ctx.cmd, push_bytes(&base));
                    ctx.device.cmd_draw_indirect(
                        ctx.cmd,
                        commands.buffer,
                        commands.offset + u64::from(phases) * command_bytes,
                        batch_count,
                        COMMAND_BYTES as u32,
                    );
                }
            });
        let main_pipeline = &self.main;
        let draw_phase = move |ctx: &mut crate::graph::PassContext<'_>, phase: u32| {
            let area = ctx.render_area();
            let push = Push {
                visible: ctx.address(visible),
                texture: ctx.sampled_index(shadow_map).unwrap_or(0),
                ..base
            };
            let offset = u64::from(phase.saturating_sub(1)) * command_bytes;
            // SAFETY: recording inside the pass's rendering scope.
            unsafe {
                main_pipeline.bind(ctx.cmd);
                bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                set_viewport(ctx.device, ctx.cmd, area);
                bindless.push_constants(ctx.cmd, push_bytes(&push));
                ctx.device.cmd_draw_indirect(
                    ctx.cmd,
                    ctx.buffer(culled),
                    offset,
                    batch_count,
                    COMMAND_BYTES as u32,
                );
            }
        };
        let Some(flags) = flag_buffer else {
            forward_pass(fg, "forward", shadow_map, culled, visible)
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
        forward_pass(fg, "forward early", shadow_map, culled, visible)
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
        forward_pass(fg, "forward late", shadow_map, culled, visible)
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
    shadow_map: Texture,
    culled: Buffer,
    visible: Buffer,
) -> PassBuilder<'a, 'g> {
    fg.add_pass(name)
        .read(shadow_map, Access::Sampled(ShaderStages::FRAGMENT))
        .read(culled, Access::Indirect)
        .read(visible, Access::StorageRead(ShaderStages::VERTEX))
}

/// Runs of consecutive draws that share a mesh, as `(first, len)`.
fn batches<'a>(draws: &'a [DrawItem<'a>]) -> impl Iterator<Item = (usize, usize)> + 'a {
    let mut start = 0;
    core::iter::from_fn(move || {
        let mesh = draws.get(start)?.mesh;
        let len = draws[start..]
            .iter()
            .take_while(|d| core::ptr::eq(d.mesh, mesh))
            .count();
        let run = (start, len);
        start += len;
        Some(run)
    })
}

/// A mesh's object-space bounding box as center and half extents.
fn local_box(mesh: &Mesh) -> (Vec3, Vec3) {
    let (lo, hi) = mesh.bounds();
    ((lo + hi) * 0.5, (hi - lo) * 0.5)
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
