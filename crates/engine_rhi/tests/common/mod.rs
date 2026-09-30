//! Shared helpers for GPU tests.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::too_many_arguments,
    clippy::print_stdout,
    clippy::unwrap_used
)]

use engine_rhi::ash::{util::read_spv, vk};
use engine_rhi::{AdapterPreference, Device, Instance, InstanceDesc};
use std::io::Cursor;

/// Instance + device, or `None` (with a note) when no Vulkan 1.3 implementation is
/// available and `ENGINE_REQUIRE_GPU` is not `1`.
pub fn gpu(headless_surface: bool) -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_rhi tests"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface,
            sync_validation: false,
        })?;
        let adapter = instance.select_adapter(&preference())?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    match attempt() {
        Ok(pair) => Some(pair),
        Err(e) if !required => {
            println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

/// The adapter the tests use: `ENGINE_ADAPTER` if set, otherwise the fastest.
pub fn preference() -> AdapterPreference {
    AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance)
}

pub fn assert_no_validation_errors(instance: &Instance) {
    let report = instance.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
}

pub struct TrianglePipeline {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    modules: [vk::ShaderModule; 2],
}

impl TrianglePipeline {
    /// The bootstrap Slang triangle for color attachments of `format`, dynamic viewport and
    /// scissor, dynamic rendering.
    pub fn new(device: &Device, format: vk::Format) -> Self {
        let d = device.raw();
        let vs_code = read_spv(&mut Cursor::new(
            &include_bytes!("../../shaders/triangle.vert.spv")[..],
        ))
        .unwrap();
        let fs_code = read_spv(&mut Cursor::new(
            &include_bytes!("../../shaders/triangle.frag.spv")[..],
        ))
        .unwrap();
        // SAFETY: valid SPIR-V from the repository and plain create infos.
        unsafe {
            let vs = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vs_code), None)
                .unwrap();
            let fs = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&fs_code), None)
                .unwrap();
            let layout = d
                .create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None)
                .unwrap();
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vs)
                    .name(c"vertexMain"),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fs)
                    .name(c"fragmentMain"),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport_state = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let blend =
                vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
            let color_formats = [format];
            let mut rendering =
                vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&raster)
                .multisample_state(&multisample)
                .color_blend_state(&blend)
                .dynamic_state(&dynamic)
                .layout(layout)
                .push_next(&mut rendering);
            let pipeline = d
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
                .map_err(|(_, e)| e)
                .unwrap()[0];
            Self {
                pipeline,
                layout,
                modules: [vs, fs],
            }
        }
    }

    /// Records: transition `image` to color attachment, clear, draw the triangle,
    /// transition to `final_layout`.
    ///
    /// # Safety
    ///
    /// `cmd` is recording; `image`/`view` are a color image of the pipeline's format.
    pub unsafe fn record(
        &self,
        device: &Device,
        cmd: vk::CommandBuffer,
        image: vk::Image,
        view: vk::ImageView,
        extent: vk::Extent2D,
        final_layout: vk::ImageLayout,
        final_stage: vk::PipelineStageFlags2,
        final_access: vk::AccessFlags2,
    ) {
        let d = device.raw();
        let range = engine_rhi::color_range();
        // SAFETY: guaranteed by the caller.
        unsafe {
            let to_attachment = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .image(image)
                .subresource_range(range)];
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&to_attachment),
            );
            let attachments = [vk::RenderingAttachmentInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                })];
            let area = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            };
            d.cmd_begin_rendering(
                cmd,
                &vk::RenderingInfo::default()
                    .render_area(area)
                    .layer_count(1)
                    .color_attachments(&attachments),
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            d.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            d.cmd_set_scissor(cmd, 0, &[area]);
            d.cmd_draw(cmd, 3, 1, 0, 0);
            d.cmd_end_rendering(cmd);
            let to_final = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                .dst_stage_mask(final_stage)
                .dst_access_mask(final_access)
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(final_layout)
                .image(image)
                .subresource_range(range)];
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&to_final),
            );
        }
    }

    pub fn destroy(self, device: &Device) {
        // SAFETY: the device is idle when tests destroy their objects.
        unsafe {
            device.raw().destroy_pipeline(self.pipeline, None);
            device.raw().destroy_pipeline_layout(self.layout, None);
            for module in self.modules {
                device.raw().destroy_shader_module(module, None);
            }
        }
    }
}

pub fn find_memory(device: &Device, bits: u32, flags: vk::MemoryPropertyFlags) -> u32 {
    // SAFETY: valid physical device of a live instance.
    let props = unsafe {
        device
            .instance_raw()
            .get_physical_device_memory_properties(device.physical())
    };
    (0..props.memory_type_count)
        .find(|&i| {
            bits & (1 << i) != 0
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
        .unwrap()
}
