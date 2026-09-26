//! Forward PBR renderer with a directional light and a shadow map (ADR 0014).
//!
//! [`ForwardRenderer::add_passes`] declares two passes in the frame's render graph:
//!
//! 1. **shadow**: depth-only rendering of every draw from the light, into a transient
//!    shadow map (with depth bias);
//! 2. **forward**: metallic-roughness PBR (GGX, height-correlated Smith, Schlick; after
//!    Filament's documentation) with 3×3 PCF shadows, into the caller's color and depth
//!    targets.
//!
//! Per-frame constants and the draw list go into the [`UploadRing`]; shaders read them
//! through device addresses and find the shadow map through the bindless set (its index
//! is assigned by the graph).

use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc};
use crate::mesh::Mesh;
use crate::upload::UploadRing;
use engine_core::math::{Mat3, Mat4, Vec3, Vec4, look_at, vulkan_orthographic};
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, DepthBias, Device, GraphicsPipelineDesc, Pipeline, RhiError, SamplerKind,
    ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.vertexMain.spv");
const SHADOW_VERTEX: &[u8] = include_bytes!("../shaders/forward.shadowVertex.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.fragmentMain.spv");

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
#[derive(Debug, Clone, Copy)]
pub struct DrawItem<'a> {
    /// Its vertices.
    pub mesh: &'a Mesh,
    /// Object to world transform.
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

/// `Frame` in `forward.slang` (208 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct FrameData {
    clip_from_world: [f32; 16],
    light_clip_from_world: [f32; 16],
    camera_position: [f32; 4],
    to_light: [f32; 4],
    light_color: [f32; 4],
    ambient: [f32; 4],
    shadow_sampler: u32,
    _pad: [u32; 3],
}

/// `Draw` in `forward.slang` (160 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct DrawData {
    world_from_object: [f32; 16],
    normal_rows: [[f32; 4]; 3],
    base_color: [f32; 4],
    metallic_roughness: [f32; 4],
    vertices: u64,
    _pad: [u32; 2],
}

/// `Push` in `forward.slang` (24 bytes).
#[repr(C)]
struct Push {
    frame: u64,
    draws: u64,
    draw_index: u32,
    shadow_map: u32,
}

const _: () = assert!(core::mem::size_of::<FrameData>() == 208);
const _: () = assert!(core::mem::size_of::<DrawData>() == 160);
const _: () = assert!(core::mem::size_of::<Push>() == 24);

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

/// The forward PBR pipelines.
#[derive(Debug)]
pub struct ForwardRenderer {
    main: Pipeline,
    shadow: Pipeline,
    shadow_settings: ShadowSettings,
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
        Ok(Self {
            main,
            shadow,
            shadow_settings,
        })
    }

    /// Declares the shadow and forward passes for `draws` into `color` and `depth`.
    ///
    /// Frame constants and the draw list are written into `ring` now; the passes record
    /// when the graph executes, which must be with this `bindless` set
    /// ([`FrameGraph::execute_with_bindless`]) so the shadow map gets an index.
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
    ) {
        let light_clip_from_world = fit_light(draws, light.direction);
        let frame = FrameData {
            clip_from_world: camera.clip_from_world.to_cols_array(),
            light_clip_from_world: light_clip_from_world.to_cols_array(),
            camera_position: camera.position.extend(1.0).to_array(),
            to_light: (-light.direction.normalize_or(Vec3::NEG_Y))
                .extend(0.0)
                .to_array(),
            light_color: light.color.extend(0.0).to_array(),
            ambient: light.ambient.extend(0.0).to_array(),
            shadow_sampler: Bindless::sampler(SamplerKind::ShadowCompare),
            _pad: [0; 3],
        };
        let frame_address = ring.push(&[frame]).unwrap_or(0);
        // The draw list: written element by element to avoid a temporary vector. Every
        // element is a multiple of the ring's alignment, so they are contiguous.
        let (mut draws_address, mut complete) = (0, true);
        for (i, item) in draws.iter().enumerate() {
            let normal = Mat3::from_mat4(item.world_from_object)
                .inverse()
                .transpose();
            let data = DrawData {
                world_from_object: item.world_from_object.to_cols_array(),
                normal_rows: [
                    normal.row(0).extend(0.0).to_array(),
                    normal.row(1).extend(0.0).to_array(),
                    normal.row(2).extend(0.0).to_array(),
                ],
                base_color: item.material.base_color.extend(1.0).to_array(),
                metallic_roughness: Vec4::new(
                    item.material.metallic,
                    item.material.roughness,
                    0.0,
                    0.0,
                )
                .to_array(),
                vertices: item.mesh.address(),
                _pad: [0; 2],
            };
            match ring.push(&[data]) {
                Some(address) if i == 0 => draws_address = address,
                Some(_) => {}
                None => complete = false,
            }
        }
        if frame_address == 0 || !complete {
            // The ring is full: skip the frame's 3D content rather than read garbage.
            return;
        }

        let resolution = self.shadow_settings.resolution;
        let shadow_map = fg.create_texture(
            "shadow map",
            TextureDesc::new_2d(self.shadow_settings.format, resolution, resolution),
        );
        let shadow_pipeline = &self.shadow;
        fg.add_pass("shadow")
            .depth(
                shadow_map,
                LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                StoreOp::Store,
            )
            .execute(move |ctx| {
                let area = ctx.render_area();
                // SAFETY: recording inside the pass's rendering scope.
                unsafe {
                    shadow_pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_viewport(ctx.device, ctx.cmd, area);
                    for (i, item) in draws.iter().enumerate() {
                        let push = Push {
                            frame: frame_address,
                            draws: draws_address,
                            draw_index: i as u32,
                            shadow_map: 0,
                        };
                        bindless.push_constants(ctx.cmd, push_bytes(&push));
                        ctx.device
                            .cmd_draw(ctx.cmd, item.mesh.vertex_count(), 1, 0, 0);
                    }
                }
            });
        let main_pipeline = &self.main;
        let load = clear.map_or(LoadOp::Load, LoadOp::Clear);
        fg.add_pass("forward")
            .read(shadow_map, Access::Sampled(ShaderStages::FRAGMENT))
            .color(color, load, StoreOp::Store)
            .depth(
                depth,
                LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                StoreOp::DontCare,
            )
            .execute(move |ctx| {
                let area = ctx.render_area();
                let shadow_index = ctx.sampled_index(shadow_map).unwrap_or(0);
                // SAFETY: recording inside the pass's rendering scope.
                unsafe {
                    main_pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_viewport(ctx.device, ctx.cmd, area);
                    for (i, item) in draws.iter().enumerate() {
                        let push = Push {
                            frame: frame_address,
                            draws: draws_address,
                            draw_index: i as u32,
                            shadow_map: shadow_index,
                        };
                        bindless.push_constants(ctx.cmd, push_bytes(&push));
                        ctx.device
                            .cmd_draw(ctx.cmd, item.mesh.vertex_count(), 1, 0, 0);
                    }
                }
            });
    }
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

/// An orthographic light projection that contains every draw's bounding sphere.
fn fit_light(draws: &[DrawItem<'_>], direction: Vec3) -> Mat4 {
    let direction = direction.normalize_or(Vec3::NEG_Y);
    // Bounding sphere of the scene from the draws' transformed bounding boxes.
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for item in draws {
        let (lo, hi) = item.mesh.bounds();
        for corner in 0..8 {
            let p = Vec3::new(
                if corner & 1 == 0 { lo.x } else { hi.x },
                if corner & 2 == 0 { lo.y } else { hi.y },
                if corner & 4 == 0 { lo.z } else { hi.z },
            );
            let world = item.world_from_object.transform_point3(p);
            min = min.min(world);
            max = max.max(world);
        }
    }
    if draws.is_empty() {
        (min, max) = (Vec3::splat(-1.0), Vec3::splat(1.0));
    }
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
