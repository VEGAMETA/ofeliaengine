//! GPU tests: they need a Vulkan 1.3 implementation (a GPU driver, or Mesa lavapipe).
//!
//! Without one they print why and pass, unless `ENGINE_REQUIRE_GPU=1` is set, which the
//! GPU workflow (self-hosted runner with real GPUs) and development containers with
//! lavapipe do. Every test also asserts that the validation layer reported no errors
//! when it is installed.

// Test helpers outside `#[test]` functions print diagnostics and unwrap freely.
#![allow(clippy::print_stdout, clippy::unwrap_used)]

mod common;

use common::{TrianglePipeline, assert_no_validation_errors, find_memory, gpu};
use engine_rhi::ash::vk;

#[test]
fn adapters_are_listed_and_one_is_selected() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    for adapter in instance.adapters().unwrap() {
        println!(
            "adapter: {} missing={:?} optional: {}",
            adapter.summary(),
            adapter.missing_features(),
            adapter.caps.names().join(", ")
        );
        // A 1.4 driver has what 1.4 made core, extension by extension.
        if adapter.caps.vulkan_1_4 {
            let caps = adapter.caps;
            assert!(
                caps.maintenance5 && caps.maintenance6 && caps.push_descriptor,
                "{}",
                adapter.summary()
            );
        }
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

/// Renders the Slang triangle into a 64×64 offscreen image with dynamic rendering and
/// synchronization2, copies it to a host-visible buffer and checks pixels.
#[test]
fn offscreen_triangle() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    const SIZE: u32 = 64;
    let format = vk::Format::R8G8B8A8_UNORM;
    let triangle = TrianglePipeline::new(&device, format);

    // SAFETY: straightforward object creation and a single synchronous submission; every
    // object is destroyed before the device, after the queue is idle.
    let pixels = unsafe {
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
                    .subresource_range(engine_rhi::color_range()),
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

        let mut frames = engine_rhi::Frames::new(&device, device.graphics.family, 1).unwrap();
        let frame = frames.begin().unwrap();
        let cmd = frame.cmd;
        triangle.record(
            &device,
            cmd,
            image,
            view,
            vk::Extent2D {
                width: SIZE,
                height: SIZE,
            },
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
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
        // No swapchain image to wait for, no binary semaphore to signal; the timeline
        // tracks completion.
        frames.submit(device.graphics, frame, false, None).unwrap();
        frames.wait_idle().unwrap();

        let mapped = d
            .map_memory(buffer_memory, 0, buffer_size, vk::MemoryMapFlags::empty())
            .unwrap();
        let pixels = std::slice::from_raw_parts(mapped.cast::<u8>(), buffer_size as usize).to_vec();
        d.unmap_memory(buffer_memory);

        drop(frames);
        d.destroy_buffer(buffer, None);
        d.free_memory(buffer_memory, None);
        d.destroy_image_view(view, None);
        d.destroy_image(image, None);
        d.free_memory(image_memory, None);
        pixels
    };
    triangle.destroy(&device);

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
    drop(device);
    assert_no_validation_errors(&instance);
}
