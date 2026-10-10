//! Swapchain and frames-in-flight test on a headless surface (`VK_EXT_headless_surface`):
//! the full acquire → render → present loop without a window, including a resize.
//! Skips like `gpu.rs` when no Vulkan implementation or no headless surface support.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use crate::common;

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

/// A window's monitor is driven by one of the system's adapters
/// (`Surface::display_adapter_luid`, matched against Vulkan's `deviceLUID`), and every
/// other adapter presents to it across adapters: what `engine_render::present` draws the
/// frame apart for. Needs a desktop (skips without one).
#[cfg(windows)]
#[test]
fn the_monitor_belongs_to_one_adapter_and_the_others_present_across() {
    use engine_platform::window::{WindowDesc, WindowSystem};
    use engine_rhi::{Instance, InstanceDesc};
    let Ok(windows) = WindowSystem::start(64) else {
        println!("skipping: no window system");
        return;
    };
    let Ok(window) = windows.create_window(&WindowDesc {
        title: String::from("engine_rhi monitor test"),
        width: 64,
        height: 64,
        resizable: false,
        visible: false,
    }) else {
        println!("skipping: no window");
        return;
    };
    let Ok(instance) = Instance::new(&InstanceDesc {
        app_name: String::from("engine_rhi monitor test"),
        surface: true,
        ..InstanceDesc::default()
    }) else {
        println!("skipping: no Vulkan");
        return;
    };
    let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
    let Some(display) = surface.display_adapter_luid() else {
        println!("skipping: Windows did not name the monitor's adapter");
        return;
    };
    let adapters = instance.adapters().unwrap();
    let mut owners = 0;
    for adapter in &adapters {
        let across = surface.presents_across_adapters(adapter);
        println!(
            "{}: LUID {:?}, presents across adapters: {across}",
            adapter.summary(),
            adapter.luid
        );
        if adapter.luid == Some(display) {
            owners += 1;
            assert!(!across, "the monitor's own adapter presents directly");
        } else if adapter.luid.is_some() {
            assert!(across, "another adapter presents across");
        }
    }
    // A software rasterizer or a display-only adapter may be missing from Vulkan's list.
    assert!(owners <= 1, "one adapter drives the monitor: {display:?}");
    if owners == 0 {
        println!("the monitor's adapter ({display:?}) is not a Vulkan adapter here");
    }
}
