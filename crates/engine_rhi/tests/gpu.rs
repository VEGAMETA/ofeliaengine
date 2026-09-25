//! GPU tests: they need a Vulkan 1.3 implementation (a GPU driver, or Mesa lavapipe).
//!
//! Without one they print why and pass, unless `ENGINE_REQUIRE_GPU=1` is set, which the
//! GPU workflow (self-hosted runner with real GPUs) and development containers with
//! lavapipe do. Every test also asserts that the validation layer reported no errors
//! when it is installed.

// Test helpers outside `#[test]` functions print diagnostics and unwrap freely.
#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_rhi::ash::{util::read_spv, vk};
use engine_rhi::{AdapterPreference, Device, Instance, InstanceDesc};
use std::io::Cursor;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_rhi tests"),
            validation: true,
            debug_utils: true,
            surface: false,
        })?;
        let adapter = instance.select_adapter(&AdapterPreference::HighPerformance)?;
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

fn assert_no_validation_errors(instance: &Instance) {
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

#[test]
fn adapters_are_listed_and_one_is_selected() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    for adapter in instance.adapters().unwrap() {
        println!(
            "adapter: {} missing={:?} caps={:?}",
            adapter.summary(),
            adapter.missing_features(),
            adapter.caps
        );
    }
    println!("selected: {}", device.adapter().summary());
    println!(
        "queues: graphics={:?} async_compute={:?} transfer={:?}",
        device.graphics, device.async_compute, device.transfer
    );
    assert!(device.adapter().missing_features().is_empty());
    drop(device);
    assert_no_validation_errors(&instance);
}

fn find_memory(device: &Device, bits: u32, flags: vk::MemoryPropertyFlags) -> u32 {
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

/// Renders the Slang triangle into a 64×64 offscreen image with dynamic rendering and
/// synchronization2, copies it to a host-visible buffer and checks pixels.
#[test]
fn offscreen_triangle() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    const SIZE: u32 = 64;
    let format = vk::Format::R8G8B8A8_UNORM;

    // SAFETY: straightforward object creation and a single synchronous submission; every
    // object is destroyed before the device, after the queue is idle.
    unsafe {
        let image = d
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: SIZE,
                        height: SIZE,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                    ),
                None,
            )
            .unwrap();
        let requirements = d.get_image_memory_requirements(image);
        let image_memory = d
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(find_memory(
                        &device,
                        requirements.memory_type_bits,
                        vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    )),
                None,
            )
            .unwrap();
        d.bind_image_memory(image, image_memory, 0).unwrap();
        let view = d
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
            .unwrap();

        let buffer_size = u64::from(SIZE * SIZE * 4);
        let buffer = d
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(buffer_size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )
            .unwrap();
        let requirements = d.get_buffer_memory_requirements(buffer);
        let buffer_memory = d
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(find_memory(
                        &device,
                        requirements.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )),
                None,
            )
            .unwrap();
        d.bind_buffer_memory(buffer, buffer_memory, 0).unwrap();

        let vs_code = read_spv(&mut Cursor::new(
            &include_bytes!("../shaders/triangle.vert.spv")[..],
        ))
        .unwrap();
        let fs_code = read_spv(&mut Cursor::new(
            &include_bytes!("../shaders/triangle.frag.spv")[..],
        ))
        .unwrap();
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
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [format];
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);
        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
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
            .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
            .map_err(|(_, e)| e)
            .unwrap()[0];

        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmd = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
            .unwrap()[0];

        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .unwrap();
        let to_attachment = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
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
            extent: vk::Extent2D {
                width: SIZE,
                height: SIZE,
            },
        };
        d.cmd_begin_rendering(
            cmd,
            &vk::RenderingInfo::default()
                .render_area(area)
                .layer_count(1)
                .color_attachments(&attachments),
        );
        d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
        d.cmd_set_viewport(
            cmd,
            0,
            &[vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: SIZE as f32,
                height: SIZE as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
        d.cmd_set_scissor(cmd, 0, &[area]);
        d.cmd_draw(cmd, 3, 1, 0, 0);
        d.cmd_end_rendering(cmd);
        let to_transfer = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(image)
            .subresource_range(range)];
        d.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&to_transfer),
        );
        d.cmd_copy_image_to_buffer(
            cmd,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            buffer,
            &[vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width: SIZE,
                    height: SIZE,
                    depth: 1,
                })],
        );
        let to_host = [vk::BufferMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::HOST)
            .dst_access_mask(vk::AccessFlags2::HOST_READ)
            .buffer(buffer)
            .size(vk::WHOLE_SIZE)];
        d.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().buffer_memory_barriers(&to_host),
        );
        d.end_command_buffer(cmd).unwrap();

        // Timeline semaphore instead of a fence: the engine's frame sync primitive.
        let mut timeline_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let timeline = d
            .create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut timeline_info),
                None,
            )
            .unwrap();
        let command_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        let signal_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(timeline)
            .value(1)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        d.queue_submit2(
            device.graphics.raw,
            &[vk::SubmitInfo2::default()
                .command_buffer_infos(&command_infos)
                .signal_semaphore_infos(&signal_infos)],
            vk::Fence::null(),
        )
        .unwrap();
        let semaphores = [timeline];
        let values = [1u64];
        d.wait_semaphores(
            &vk::SemaphoreWaitInfo::default()
                .semaphores(&semaphores)
                .values(&values),
            5_000_000_000,
        )
        .unwrap();

        let mapped = d
            .map_memory(buffer_memory, 0, buffer_size, vk::MemoryMapFlags::empty())
            .unwrap();
        let pixels = std::slice::from_raw_parts(mapped.cast::<u8>(), buffer_size as usize).to_vec();
        d.unmap_memory(buffer_memory);

        d.destroy_semaphore(timeline, None);
        d.destroy_command_pool(pool, None);
        d.destroy_pipeline(pipeline, None);
        d.destroy_pipeline_layout(layout, None);
        d.destroy_shader_module(vs, None);
        d.destroy_shader_module(fs, None);
        d.destroy_buffer(buffer, None);
        d.free_memory(buffer_memory, None);
        d.destroy_image_view(view, None);
        d.destroy_image(image, None);
        d.free_memory(image_memory, None);

        let pixel = |x: u32, y: u32| {
            let i = ((y * SIZE + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // Corners are the clear color.
        for (x, y) in [(0, 0), (SIZE - 1, 0), (0, SIZE - 1), (SIZE - 1, SIZE - 1)] {
            assert_eq!(pixel(x, y), [0, 0, 0, 255], "corner ({x},{y})");
        }
        // The centre is inside the triangle: opaque and clearly not black.
        let centre = pixel(SIZE / 2, SIZE / 2);
        println!("centre pixel: {centre:?}");
        assert_eq!(centre[3], 255);
        assert!(u32::from(centre[0]) + u32::from(centre[1]) + u32::from(centre[2]) > 200);
        // Near the top vertex (red) red dominates; near the bottom-right (green), green.
        let top = pixel(SIZE / 2, SIZE / 4 + 2);
        let bottom_right = pixel(SIZE * 3 / 4 - 3, SIZE * 3 / 4 - 1);
        assert!(top[0] > top[1] && top[0] > top[2], "top {top:?}");
        assert!(
            bottom_right[1] > bottom_right[0] && bottom_right[1] > bottom_right[2],
            "bottom-right {bottom_right:?}"
        );
    }
    drop(device);
    assert_no_validation_errors(&instance);
}
