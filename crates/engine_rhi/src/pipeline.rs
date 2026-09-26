//! Graphics and compute pipelines (ADR 0013).
//!
//! Every pipeline uses the [`crate::Bindless`] pipeline layout. Graphics pipelines use
//! dynamic rendering (attachment formats instead of render passes), dynamic viewport and
//! scissor, and **no vertex input state**: vertices are pulled from buffers through
//! buffer device addresses (the GPU-driven path, CLAUDE.md §6).

use crate::device::Device;
use crate::error::RhiError;
use ash::vk;
use core::ffi::CStr;

/// A SPIR-V entry point.
#[derive(Debug, Clone, Copy)]
pub struct ShaderStageDesc<'a> {
    /// SPIR-V words.
    pub spirv: &'a [u32],
    /// Entry point name in the module.
    pub entry: &'a CStr,
}

/// Color blending of all color attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    /// Overwrite.
    Opaque,
    /// `src * a + dst * (1 - a)`.
    Alpha,
    /// `src + dst`.
    Additive,
    /// Premultiplied alpha: `src + dst * (1 - src.a)`. A source with `a = 0` adds, so
    /// alpha-blended and additive draws share one pipeline.
    Premultiplied,
    /// Multiply with a premultiplied source: `src * dst + dst * (1 - src.a)` for color
    /// (`dst * lerp(1, color, a)`); destination alpha is kept.
    Multiply,
}

/// Depth bias for shadow maps: `constant` units plus `slope` × the polygon's depth slope,
/// at most `clamp` (0 = no clamp).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthBias {
    /// Constant factor.
    pub constant: f32,
    /// Slope factor.
    pub slope: f32,
    /// Largest bias (0 disables the clamp).
    pub clamp: f32,
}

/// Fixed-function state and formats of a graphics pipeline.
#[derive(Debug, Clone, Copy)]
pub struct GraphicsPipelineDesc<'a> {
    /// Vertex (or mesh) stage.
    pub vertex: ShaderStageDesc<'a>,
    /// Fragment stage, if any (depth-only passes have none).
    pub fragment: Option<ShaderStageDesc<'a>>,
    /// Formats of the color attachments, in location order.
    pub color_formats: &'a [vk::Format],
    /// Depth attachment format, or `UNDEFINED` for none.
    pub depth_format: vk::Format,
    /// Primitive topology.
    pub topology: vk::PrimitiveTopology,
    /// Faces to cull.
    pub cull_mode: vk::CullModeFlags,
    /// Winding of front faces (counter-clockwise with the engine's projection).
    pub front_face: vk::FrontFace,
    /// Depth test enabled.
    pub depth_test: bool,
    /// Depth writes enabled.
    pub depth_write: bool,
    /// Depth comparison (`GREATER_OR_EQUAL` for reversed Z).
    pub depth_compare: vk::CompareOp,
    /// Color blending.
    pub blend: Blend,
    /// Depth bias (shadow maps), if any.
    pub depth_bias: Option<DepthBias>,
    /// Samples per pixel.
    pub samples: vk::SampleCountFlags,
}

impl<'a> GraphicsPipelineDesc<'a> {
    /// Opaque triangles with back-face culling, depth test and write (`LESS`), no MSAA.
    pub fn new(vertex: ShaderStageDesc<'a>, fragment: Option<ShaderStageDesc<'a>>) -> Self {
        Self {
            vertex,
            fragment,
            color_formats: &[],
            depth_format: vk::Format::UNDEFINED,
            topology: vk::PrimitiveTopology::TRIANGLE_LIST,
            cull_mode: vk::CullModeFlags::BACK,
            front_face: vk::FrontFace::COUNTER_CLOCKWISE,
            depth_test: true,
            depth_write: true,
            depth_compare: vk::CompareOp::LESS,
            blend: Blend::Opaque,
            depth_bias: None,
            samples: vk::SampleCountFlags::TYPE_1,
        }
    }
}

/// A pipeline and the device that owns it. Destroyed on drop (the GPU must be done
/// with it).
pub struct Pipeline {
    device: ash::Device,
    raw: vk::Pipeline,
    bind_point: vk::PipelineBindPoint,
}

impl core::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pipeline")
            .field("raw", &self.raw)
            .field("bind_point", &self.bind_point)
            .finish()
    }
}

struct Modules<'d> {
    device: &'d ash::Device,
    list: [vk::ShaderModule; 2],
}

impl Modules<'_> {
    fn create(&mut self, slot: usize, spirv: &[u32]) -> Result<vk::ShaderModule, RhiError> {
        // SAFETY: the words are a SPIR-V module (validated by the driver/layer).
        let module = unsafe {
            self.device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(spirv), None)
        }
        .map_err(RhiError::vk("vkCreateShaderModule"))?;
        self.list[slot] = module;
        Ok(module)
    }
}

impl Drop for Modules<'_> {
    fn drop(&mut self) {
        for &module in &self.list {
            if module != vk::ShaderModule::null() {
                // SAFETY: modules are only needed until pipeline creation returns.
                unsafe { self.device.destroy_shader_module(module, None) };
            }
        }
    }
}

impl Pipeline {
    /// Creates a graphics pipeline with `layout` (normally
    /// [`crate::Bindless::pipeline_layout`]).
    pub fn graphics(
        device: &Device,
        layout: vk::PipelineLayout,
        desc: &GraphicsPipelineDesc<'_>,
        cache: vk::PipelineCache,
    ) -> Result<Self, RhiError> {
        engine_profiler::zone!("rhi::Pipeline::graphics");
        let d = device.raw();
        let mut modules = Modules {
            device: d,
            list: [vk::ShaderModule::null(); 2],
        };
        let mut stages = Vec::with_capacity(2);
        let vertex = modules.create(0, desc.vertex.spirv)?;
        stages.push(
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex)
                .name(desc.vertex.entry),
        );
        if let Some(fragment) = desc.fragment {
            let module = modules.create(1, fragment.spirv)?;
            stages.push(
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(module)
                    .name(fragment.entry),
            );
        }
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(desc.topology);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let mut raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(desc.cull_mode)
            .front_face(desc.front_face)
            .line_width(1.0);
        if let Some(bias) = desc.depth_bias {
            raster = raster
                .depth_bias_enable(true)
                .depth_bias_constant_factor(bias.constant)
                .depth_bias_slope_factor(bias.slope)
                .depth_bias_clamp(bias.clamp);
        }
        let multisample =
            vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(desc.samples);
        let depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(desc.depth_test)
            .depth_write_enable(desc.depth_write)
            .depth_compare_op(desc.depth_compare);
        let attachment = match desc.blend {
            Blend::Opaque => vk::PipelineColorBlendAttachmentState::default(),
            Blend::Alpha => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Additive => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Premultiplied => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Multiply => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::DST_COLOR)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ZERO)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                .alpha_blend_op(vk::BlendOp::ADD),
        }
        .color_write_mask(vk::ColorComponentFlags::RGBA);
        let attachments = vec![attachment; desc.color_formats.len()];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let mut rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(desc.color_formats)
            .depth_attachment_format(desc.depth_format);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .push_next(&mut rendering);
        // SAFETY: every pointer in `info` refers to locals alive for the call.
        let raw = unsafe { d.create_graphics_pipelines(cache, &[info], None) }.map_err(
            |(_, result)| RhiError::Vulkan {
                call: "vkCreateGraphicsPipelines",
                result,
            },
        )?[0];
        Ok(Self {
            device: d.clone(),
            raw,
            bind_point: vk::PipelineBindPoint::GRAPHICS,
        })
    }

    /// Creates a compute pipeline with `layout`.
    pub fn compute(
        device: &Device,
        layout: vk::PipelineLayout,
        stage: ShaderStageDesc<'_>,
        cache: vk::PipelineCache,
    ) -> Result<Self, RhiError> {
        engine_profiler::zone!("rhi::Pipeline::compute");
        let d = device.raw();
        let mut modules = Modules {
            device: d,
            list: [vk::ShaderModule::null(); 2],
        };
        let module = modules.create(0, stage.spirv)?;
        let info = vk::ComputePipelineCreateInfo::default()
            .stage(
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::COMPUTE)
                    .module(module)
                    .name(stage.entry),
            )
            .layout(layout);
        // SAFETY: as above.
        let raw = unsafe { d.create_compute_pipelines(cache, &[info], None) }.map_err(
            |(_, result)| RhiError::Vulkan {
                call: "vkCreateComputePipelines",
                result,
            },
        )?[0];
        Ok(Self {
            device: d.clone(),
            raw,
            bind_point: vk::PipelineBindPoint::COMPUTE,
        })
    }

    /// The Vulkan pipeline.
    pub fn raw(&self) -> vk::Pipeline {
        self.raw
    }

    /// Binds the pipeline.
    ///
    /// # Safety
    ///
    /// `cmd` is recording and belongs to this device.
    pub unsafe fn bind(&self, cmd: vk::CommandBuffer) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.device
                .cmd_bind_pipeline(cmd, self.bind_point, self.raw)
        };
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: owned pipeline; the GPU no longer uses it (type-level contract).
        unsafe { self.device.destroy_pipeline(self.raw, None) };
    }
}

/// Reads SPIR-V from bytes (for example `include_bytes!`), checking size and magic.
pub fn spirv_words(bytes: &[u8]) -> Option<Vec<u32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    (words.first() == Some(&0x0723_0203)).then_some(words)
}
