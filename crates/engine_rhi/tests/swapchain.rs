//! Swapchain and frames-in-flight test on a headless surface (`VK_EXT_headless_surface`):
//! the full acquire → render → present loop without a window, including a resize.
//! Skips like `gpu.rs` when no Vulkan implementation or no headless surface support.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

mod common;

use common::{TrianglePipeline, assert_no_validation_errors, gpu};
use engine_rhi::ash::vk;
use engine_rhi::{Frames, OutputColor, PresentMode, Surface, Swapchain, SwapchainDesc};

#[test]
fn headless_present_loop_with_resize() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    let surface = match Surface::headless(&instance) {
        Ok(surface) => surface,
        Err(e) => {
            // Headless surfaces are a Mesa/SwiftShader feature; GPU drivers rarely have it.
            println!("skipping: no headless surface on this driver: {e}");
            return;
        }
    };
    assert!(surface.supports_queue(&device, device.graphics).unwrap());
    println!(
        "present modes: {:?}",
        Swapchain::supported_present_modes(&device, &surface).unwrap()
    );
    let mut swapchain = Swapchain::new(
        &device,
        &surface,
        &SwapchainDesc {
            width: 128,
            height: 96,
            present_mode: PresentMode::Mailbox,
            srgb: true,
            color: OutputColor::Sdr,
            low_latency: false,
        },
    )
    .unwrap();
    println!("swapchain: {swapchain:?}");
    let triangle = TrianglePipeline::new(&device, swapchain.format());
    let mut frames = Frames::new(&device, device.graphics.family, 2).unwrap();

    let mut presented = 0;
    for (width, height) in [(128, 96), (64, 64)] {
        if swapchain.extent() != (vk::Extent2D { width, height }) {
            frames.wait_idle().unwrap();
            swapchain
                .recreate(&device, &surface, width, height)
                .unwrap();
            println!("recreated: {swapchain:?}");
        }
        assert_eq!(swapchain.extent(), vk::Extent2D { width, height });
        for _ in 0..30 {
            let frame = frames.begin().unwrap();
            let acquired = swapchain
                .acquire(frame.image_available, 5_000_000_000)
                .unwrap();
            let index = acquired.index as usize;
            // SAFETY: the frame's command buffer is recording; the image is acquired.
            unsafe {
                triangle.record(
                    &device,
                    frame.cmd,
                    swapchain.images()[index],
                    swapchain.views()[index],
                    swapchain.extent(),
                    vk::ImageLayout::PRESENT_SRC_KHR,
                    vk::PipelineStageFlags2::NONE,
                    vk::AccessFlags2::NONE,
                );
            }
            frames
                .submit(
                    device.graphics,
                    frame,
                    true,
                    Some(swapchain.render_finished(acquired.index)),
                )
                .unwrap();
            swapchain.present(device.graphics, acquired.index).unwrap();
            presented += 1;
        }
    }
    frames.wait_idle().unwrap();
    assert_eq!(frames.submitted(), presented);
    drop(frames);
    triangle.destroy(&device);
    drop(swapchain);
    drop(surface);
    drop(device);
    assert_no_validation_errors(&instance);
}
