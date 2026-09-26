//! Minimal forward mesh rendering: vertices pulled through buffer device addresses, one
//! directional light. The first step of the 3D renderer (CLAUDE.md §17), used to prove the
//! path from the render graph to pixels; materials, PBR and shadows build on it.

use crate::graph::PassContext;
use engine_core::math::{Mat3, Mat4, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Device, GraphicsPipelineDesc, MemoryLocation, Pipeline,
    RhiError, ShaderStageDesc, spirv_words,
};

const VERTEX_SPIRV: &[u8] = include_bytes!("../shaders/mesh.vertexMain.spv");
const FRAGMENT_SPIRV: &[u8] = include_bytes!("../shaders/mesh.fragmentMain.spv");

/// A vertex as the shaders read it (32 bytes, 16-byte aligned fields).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct Vertex {
    /// Position (w unused).
    pub position: [f32; 4],
    /// Normal (w unused).
    pub normal: [f32; 4],
}

impl Vertex {
    /// A vertex from a position and a normal.
    pub fn new(position: Vec3, normal: Vec3) -> Self {
        Self {
            position: position.extend(1.0).to_array(),
            normal: normal.extend(0.0).to_array(),
        }
    }
}

/// A unit cube centred on the origin: 36 vertices, counter-clockwise front faces.
pub fn cube() -> Vec<Vertex> {
    let faces: [(Vec3, Vec3, Vec3); 6] = [
        // (normal, u axis, v axis): u × v = normal, so the quad winds counter-clockwise.
        (Vec3::X, Vec3::NEG_Z, Vec3::Y),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::X, Vec3::NEG_Z),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y),
    ];
    let mut vertices = Vec::with_capacity(36);
    for (n, u, v) in faces {
        let c = n * 0.5;
        let corners = [
            c - u * 0.5 - v * 0.5,
            c + u * 0.5 - v * 0.5,
            c + u * 0.5 + v * 0.5,
            c - u * 0.5 + v * 0.5,
        ];
        for i in [0, 1, 2, 0, 2, 3] {
            vertices.push(Vertex::new(corners[i], n));
        }
    }
    vertices
}

/// A right-handed perspective projection for Vulkan: depth 0..1 (near..far) and the Y
/// flip of Vulkan's clip space folded in, so +Y is up on screen.
pub fn perspective(fov_y_radians: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    engine_core::math::vulkan_perspective(fov_y_radians, aspect, near, far)
}

/// Vertices on the GPU.
#[derive(Debug)]
pub struct Mesh {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    vertex_count: u32,
    bounds: (Vec3, Vec3),
}

impl Mesh {
    /// Uploads `vertices` to memory the GPU reads directly (device-local and host-visible
    /// when available). Staged uploads come with asset streaming.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        vertices: &[Vertex],
    ) -> Result<Self, RhiError> {
        let bytes = core::mem::size_of_val(vertices);
        let (buffer, mut allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes.max(16) as u64)
                .usage(
                    vk::BufferUsageFlags::STORAGE_BUFFER
                        | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
                ),
            MemoryLocation::Dynamic,
        )?;
        if let Some(target) = allocation.mapped_slice_mut() {
            // SAFETY: `Vertex` is `repr(C)` plain floats; the slice covers `bytes` bytes.
            let source =
                unsafe { core::slice::from_raw_parts(vertices.as_ptr().cast::<u8>(), bytes) };
            target[..bytes].copy_from_slice(source);
        }
        // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
        let address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for v in vertices {
            let p = Vec3::new(v.position[0], v.position[1], v.position[2]);
            min = min.min(p);
            max = max.max(p);
        }
        if vertices.is_empty() {
            (min, max) = (Vec3::ZERO, Vec3::ZERO);
        }
        Ok(Self {
            buffer,
            allocation: Some(allocation),
            address,
            vertex_count: vertices.len() as u32,
            bounds: (min, max),
        })
    }

    /// Device address of the vertices.
    pub fn address(&self) -> vk::DeviceAddress {
        self.address
    }

    /// Number of vertices (three per triangle).
    pub fn vertex_count(&self) -> u32 {
        self.vertex_count
    }

    /// Object-space bounding box: `(min, max)`.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        self.bounds
    }

    /// Frees the buffer.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the mesh.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// The forward mesh pipeline for one pair of attachment formats.
#[derive(Debug)]
pub struct MeshRenderer {
    pipeline: Pipeline,
}

/// Push constants of `mesh.slang` (120 bytes).
#[repr(C)]
struct Push {
    clip_from_object: [f32; 16],
    normal_rows: [[f32; 4]; 3],
    vertices: u64,
}

impl MeshRenderer {
    /// Creates the pipeline for rendering into `color_format` with a `depth_format`
    /// depth buffer.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX_SPIRV).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT_SPIRV).ok_or_else(invalid)?;
        let formats = [color_format];
        let desc = GraphicsPipelineDesc {
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
        };
        let pipeline = Pipeline::graphics(
            device,
            bindless.pipeline_layout(),
            &desc,
            vk::PipelineCache::null(),
        )?;
        Ok(Self { pipeline })
    }

    /// Draws `mesh` with `clip_from_world * world_from_object` inside a pass that renders
    /// to this renderer's formats. Binds the pipeline, the bindless set and the viewport.
    pub fn draw(
        &self,
        ctx: &PassContext<'_>,
        bindless: &Bindless,
        mesh: &Mesh,
        clip_from_world: Mat4,
        world_from_object: Mat4,
    ) {
        let area = ctx.render_area();
        let normal = Mat3::from_mat4(world_from_object).inverse().transpose();
        let push = Push {
            clip_from_object: (clip_from_world * world_from_object).to_cols_array(),
            normal_rows: [
                normal.row(0).extend(0.0).to_array(),
                normal.row(1).extend(0.0).to_array(),
                normal.row(2).extend(0.0).to_array(),
            ],
            vertices: mesh.address,
        };
        // SAFETY: `Push` is plain `repr(C)` data without padding (16 + 12 floats + u64).
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&raw const push).cast::<u8>(),
                core::mem::size_of::<Push>(),
            )
        };
        let d = ctx.device;
        // SAFETY: recording inside a pass with matching attachments (caller).
        unsafe {
            self.pipeline.bind(ctx.cmd);
            bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
            bindless.push_constants(ctx.cmd, bytes);
            d.cmd_set_viewport(
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
            d.cmd_set_scissor(ctx.cmd, 0, &[area]);
            d.cmd_draw(ctx.cmd, mesh.vertex_count, 1, 0, 0);
        }
    }
}
