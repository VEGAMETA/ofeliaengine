//! Forward PBR renderer with a directional light and a shadow map (ADR 0014), drawn
//! GPU-driven (ADR 0034).
//!
//! [`ForwardRenderer::add_passes`] turns the draw list into instances and batches (a batch
//! is a run of consecutive draws that share a mesh, drawn as one instanced draw) and
//! declares four passes in the frame's render graph:
//!
//! 1. **cull setup**: copies the batches' indirect draw commands, with no instances yet,
//!    from the upload ring into a graph buffer;
//! 2. **cull**: a compute pass tests every instance's world-space bounding box against the
//!    camera frustum, appends the visible ones to their batch's range of a visible list
//!    and counts them in the batch's command;
//! 3. **shadow**: depth-only rendering of every instance from the light, into a transient
//!    shadow map (with depth bias);
//! 4. **forward**: metallic-roughness PBR (GGX, height-correlated Smith, Schlick; after
//!    Filament's documentation) with 3×3 PCF shadows, into the caller's color and depth
//!    targets.
//!
//! Each drawing pass records a single `vkCmdDrawIndirect` for all batches, so beyond
//! writing 80 bytes per instance into the [`UploadRing`] the CPU cost of a frame does not
//! grow with the number of draws. Shaders read everything through device addresses and
//! find the shadow map through the bindless set (its index is assigned by the graph).

use crate::cull::Frustum;
use crate::graph::{
    Access, Buffer, BufferDesc, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc,
};
use crate::mesh::Mesh;
use crate::upload::UploadRing;
use engine_core::math::{Mat4, Vec3, look_at, vulkan_orthographic};
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, DepthBias, Device, GraphicsPipelineDesc, Pipeline, RhiError, SamplerKind,
    ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.vertexMain.spv");
const SHADOW_VERTEX: &[u8] = include_bytes!("../shaders/forward.shadowVertex.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.fragmentMain.spv");
const CULL: &[u8] = include_bytes!("../shaders/forward.cullMain.spv");

/// Threads per workgroup of `cullMain`.
const CULL_GROUP: u32 = 64;

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
/// batch, so keep the draws of a mesh together.
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

/// `Frame` in `forward.slang` (304 bytes).
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
    _pad: [u32; 2],
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

/// `Push` in `forward.slang` (48 bytes).
#[repr(C)]
struct Push {
    frame: u64,
    instances: u64,
    batches: u64,
    visible: u64,
    commands: u64,
    instance_count: u32,
    shadow_map: u32,
}

const _: () = assert!(core::mem::size_of::<FrameData>() == 304);
const _: () = assert!(core::mem::size_of::<InstanceData>() == 80);
const _: () = assert!(core::mem::size_of::<BatchData>() == 48);
const _: () = assert!(core::mem::size_of::<Push>() == 48);
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
    /// The forward pass's indirect draw commands, one `vk::DrawIndirectCommand` per batch
    /// in draw order. After the culling pass, each `instance_count` is the number of
    /// visible instances of its batch: read it back to count what was drawn. `None` when
    /// there was nothing to draw.
    pub commands: Option<Buffer>,
}

/// The forward PBR pipelines.
#[derive(Debug)]
pub struct ForwardRenderer {
    main: Pipeline,
    shadow: Pipeline,
    cull: Pipeline,
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
        let cull = spirv_words(CULL).ok_or_else(invalid)?;
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
        let cull = Pipeline::compute(
            device,
            bindless.pipeline_layout(),
            ShaderStageDesc {
                spirv: &cull,
                entry: c"cullMain",
            },
            vk::PipelineCache::null(),
        )?;
        Ok(Self {
            main,
            shadow,
            cull,
            shadow_settings,
            frustum_culling: true,
        })
    }

    /// Turns GPU frustum culling on (the default) or off. Off, every instance counts as
    /// visible: for comparisons and debugging.
    pub fn set_frustum_culling(&mut self, enabled: bool) {
        self.frustum_culling = enabled;
    }

    /// Whether GPU frustum culling is on.
    pub fn frustum_culling(&self) -> bool {
        self.frustum_culling
    }

    /// Declares the culling, shadow and forward passes for `draws` into `color` and
    /// `depth`.
    ///
    /// Instances, batches, draw commands and frame constants are written into `ring` now;
    /// the passes record when the graph executes, which must be with this `bindless` set
    /// ([`FrameGraph::execute_with_bindless`]) so the shadow map gets an index. Returns
    /// `None`, declaring nothing, when `ring` is full.
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        draws: &'g [DrawItem<'g>],
        camera: &Camera,
        light: &DirectionalLight,
        color: Texture,
        depth: Texture,
        clear: Option<[f32; 4]>,
    ) -> Option<ForwardPasses> {
        engine_profiler::zone!("render::forward::add_passes");
        let load = clear.map_or(LoadOp::Load, LoadOp::Clear);
        let depth_clear = LoadOp::Clear([1.0, 0.0, 0.0, 0.0]);
        if draws.is_empty() {
            fg.add_pass("forward")
                .color(color, load, StoreOp::Store)
                .depth(depth, depth_clear, StoreOp::DontCare)
                .execute(|_| {});
            return Some(ForwardPasses {
                instances: 0,
                batches: 0,
                commands: None,
            });
        }
        let instance_count = draws.len() as u32;
        let batch_count = batches(draws).count() as u32;

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
        // Draw commands: the forward pass's, with no instances until culling counts them,
        // then the shadow pass's, with every instance.
        let commands = {
            let (slots, staged) =
                ring.alloc::<vk::DrawIndirectCommand>(2 * batch_count as usize)?;
            let (culled, all) = slots.split_at_mut(batch_count as usize);
            for (((first, len), culled), all) in batches(draws).zip(culled).zip(all) {
                let command = vk::DrawIndirectCommand {
                    vertex_count: draws[first].mesh.vertex_count(),
                    instance_count: 0,
                    first_vertex: 0,
                    first_instance: first as u32,
                };
                culled.write(command);
                all.write(vk::DrawIndirectCommand {
                    instance_count: len as u32,
                    ..command
                });
            }
            staged
        };
        let frame = FrameData {
            clip_from_world: camera.clip_from_world.to_cols_array(),
            light_clip_from_world: fit_light(min, max, light.direction).to_cols_array(),
            camera_position: camera.position.extend(1.0).to_array(),
            to_light: (-light.direction.normalize_or(Vec3::NEG_Y))
                .extend(0.0)
                .to_array(),
            light_color: light.color.extend(0.0).to_array(),
            ambient: light.ambient.extend(0.0).to_array(),
            planes: Frustum::from_clip_from_world(camera.clip_from_world)
                .planes
                .map(|p| p.to_array()),
            shadow_sampler: Bindless::sampler(SamplerKind::ShadowCompare),
            cull: u32::from(self.frustum_culling),
            _pad: [0; 2],
        };
        let frame = ring.push(&[frame])?;
        let push = move |visible, commands, shadow_map| Push {
            frame,
            instances,
            batches: batch_table,
            visible,
            commands,
            instance_count,
            shadow_map,
        };

        let command_bytes = u64::from(batch_count) * COMMAND_BYTES;
        let culled = fg.create_buffer(
            "draw commands",
            BufferDesc {
                size: command_bytes,
            },
        );
        let visible = fg.create_buffer(
            "visible instances",
            BufferDesc {
                size: u64::from(instance_count) * 4,
            },
        );
        fg.add_pass("cull setup")
            .write(culled, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::BufferCopy {
                    src_offset: commands.offset,
                    dst_offset: 0,
                    size: command_bytes,
                };
                // SAFETY: recording outside a rendering scope; the ranges are in bounds.
                unsafe {
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        commands.buffer,
                        ctx.buffer(culled),
                        &[region],
                    );
                }
            });
        let cull_pipeline = &self.cull;
        fg.add_pass("cull")
            .read(culled, Access::StorageReadWrite(ShaderStages::COMPUTE))
            .write(visible, Access::StorageWrite(ShaderStages::COMPUTE))
            .execute(move |ctx| {
                let push = push(ctx.address(visible), ctx.address(culled), 0);
                // SAFETY: recording outside a rendering scope.
                unsafe {
                    cull_pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::COMPUTE);
                    bindless.push_constants(ctx.cmd, push_bytes(&push));
                    ctx.device
                        .cmd_dispatch(ctx.cmd, instance_count.div_ceil(CULL_GROUP), 1, 1);
                }
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
                let push = push(0, 0, 0);
                // SAFETY: recording inside the pass's rendering scope; the commands are in
                // the ring, which has indirect-buffer usage.
                unsafe {
                    shadow_pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_viewport(ctx.device, ctx.cmd, area);
                    bindless.push_constants(ctx.cmd, push_bytes(&push));
                    ctx.device.cmd_draw_indirect(
                        ctx.cmd,
                        commands.buffer,
                        commands.offset + command_bytes,
                        batch_count,
                        COMMAND_BYTES as u32,
                    );
                }
            });
        let main_pipeline = &self.main;
        fg.add_pass("forward")
            .read(shadow_map, Access::Sampled(ShaderStages::FRAGMENT))
            .read(culled, Access::Indirect)
            .read(visible, Access::StorageRead(ShaderStages::VERTEX))
            .color(color, load, StoreOp::Store)
            .depth(depth, depth_clear, StoreOp::DontCare)
            .execute(move |ctx| {
                let area = ctx.render_area();
                let shadow_index = ctx.sampled_index(shadow_map).unwrap_or(0);
                let push = push(ctx.address(visible), 0, shadow_index);
                // SAFETY: recording inside the pass's rendering scope.
                unsafe {
                    main_pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_viewport(ctx.device, ctx.cmd, area);
                    bindless.push_constants(ctx.cmd, push_bytes(&push));
                    ctx.device.cmd_draw_indirect(
                        ctx.cmd,
                        ctx.buffer(culled),
                        0,
                        batch_count,
                        COMMAND_BYTES as u32,
                    );
                }
            });
        Some(ForwardPasses {
            instances: instance_count,
            batches: batch_count,
            commands: Some(culled),
        })
    }
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
