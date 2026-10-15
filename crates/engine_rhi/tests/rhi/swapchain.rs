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
            ..SwapchainDesc::default()
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
        ..WindowDesc::default()
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

/// Presents eight frames into a swapchain made with `choice` for `window`, recreating it
/// when it is out of date, and returns it.
#[cfg(windows)]
fn present_with(
    device: &engine_rhi::Device,
    surface: &Surface,
    frames: &mut Frames,
    choice: engine_rhi::FullScreenExclusive,
) -> Swapchain {
    let mut swapchain = Swapchain::new(
        device,
        surface,
        &SwapchainDesc {
            width: 64,
            height: 64,
            present_mode: PresentMode::Fifo,
            full_screen_exclusive: choice,
            ..SwapchainDesc::default()
        },
    )
    .unwrap();
    let triangle = TrianglePipeline::new(device, swapchain.format());
    let mut presented = 0;
    while presented < 8 {
        let frame = frames.begin().unwrap();
        let acquired = match swapchain.acquire(frame.image_available, 5_000_000_000) {
            Ok(acquired) => acquired,
            Err(e) if e.is_out_of_date() => {
                // The frame's slot is given back unused.
                frames.submit(device.graphics, frame, false, None).unwrap();
                frames.wait_idle().unwrap();
                let e = swapchain.extent();
                swapchain
                    .recreate(device, surface, e.width, e.height)
                    .unwrap();
                continue;
            }
            Err(e) => panic!("{e}"),
        };
        let index = acquired.index as usize;
        // SAFETY: the frame's command buffer is recording; the image is acquired.
        unsafe {
            triangle.record(
                device,
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
        match swapchain.present(device.graphics, acquired.index) {
            Ok(_) => {}
            Err(e) if e.is_out_of_date() => {}
            Err(e) => panic!("{e}"),
        }
        presented += 1;
    }
    frames.wait_idle().unwrap();
    triangle.destroy(device);
    swapchain
}

/// A hidden window's surface, an instance with validation and the tests' device, or
/// `None` without a desktop or Vulkan.
#[cfg(windows)]
fn window_device(
    mode: engine_platform::window::WindowMode,
) -> Option<(
    engine_platform::window::WindowSystem,
    engine_platform::window::WindowHandle,
    engine_rhi::Instance,
    engine_rhi::Device,
)> {
    use engine_platform::window::{WindowDesc, WindowSystem};
    use engine_rhi::{Device, Instance, InstanceDesc};
    let windows = WindowSystem::start(64).ok()?;
    let window = windows
        .create_window(&WindowDesc {
            title: String::from("engine_rhi full screen test"),
            width: 64,
            height: 64,
            visible: false,
            mode,
            ..WindowDesc::default()
        })
        .ok()?;
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("engine_rhi full screen test"),
        validation: true,
        debug_utils: true,
        surface: true,
        ..InstanceDesc::default()
    })
    .ok()?;
    let adapter = instance.select_adapter(&common::preference()).ok()?;
    let device = Device::new(&instance, &adapter).ok()?;
    println!(
        "{}: VK_EXT_full_screen_exclusive enabled: {}",
        adapter.summary(),
        device.full_screen_exclusive_enabled()
    );
    Some((windows, window, instance, device))
}

/// The choices that leave the monitor to Windows (ADR 0099) make swapchains that present,
/// under validation. A small hidden window: no driver takes a monitor for it.
#[cfg(windows)]
#[test]
fn swapchains_that_leave_the_monitor_to_windows_present() {
    use engine_platform::window::WindowMode;
    use engine_rhi::FullScreenExclusive;
    let Some((_windows, window, instance, device)) = window_device(WindowMode::Windowed) else {
        println!("skipping: no desktop or no Vulkan");
        return;
    };
    let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
    let mut frames = Frames::new(&device, device.graphics.family, 2).unwrap();
    for choice in [
        FullScreenExclusive::Default,
        FullScreenExclusive::Allowed,
        FullScreenExclusive::Disallowed,
    ] {
        let swapchain = present_with(&device, &surface, &mut frames, choice);
        println!("{choice:?}: {:?}", swapchain.extent());
        assert!(!swapchain.exclusive() && swapchain.exclusive_refused().is_none());
    }
    drop(frames);
    drop(surface);
    drop(device);
    assert_no_validation_errors(&instance);
}

/// An exclusive swapchain for a borderless window covering its monitor presents whether
/// the driver gives it the monitor or not; refused, it is made again as a borderless
/// window's. Ignored: a driver may take the monitor for a moment even for a hidden window
/// (NVIDIA's does); run it by hand (`-- --ignored`).
#[cfg(windows)]
#[test]
#[ignore = "may take the monitor for a moment: run by hand"]
fn an_exclusive_swapchain_presents_held_or_refused() {
    use engine_platform::{display, window::WindowMode};
    use engine_rhi::FullScreenExclusive;
    let Some((_windows, window, instance, device)) = window_device(WindowMode::FullScreen) else {
        println!("skipping: no desktop or no Vulkan");
        return;
    };
    let monitor = display::window_monitor(window.hwnd).unwrap();
    let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
    let mut frames = Frames::new(&device, device.graphics.family, 2).unwrap();
    let swapchain = present_with(
        &device,
        &surface,
        &mut frames,
        FullScreenExclusive::Exclusive {
            monitor: monitor.handle,
        },
    );
    println!(
        "{:?}, exclusive {}, refused {:?}",
        swapchain.extent(),
        swapchain.exclusive(),
        swapchain.exclusive_refused()
    );
    assert_eq!(
        swapchain.extent(),
        vk::Extent2D {
            width: monitor.rect.width() as u32,
            height: monitor.rect.height() as u32,
        },
        "the window covers its monitor"
    );
    assert!(swapchain.exclusive() != swapchain.exclusive_refused().is_some());
    drop(swapchain);
    drop(frames);
    drop(surface);
    drop(device);
    assert_no_validation_errors(&instance);
}
