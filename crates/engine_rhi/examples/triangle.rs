//! A window with the bootstrap Slang triangle: `engine_platform` window thread +
//! `engine_rhi` swapchain and frames in flight.
//!
//! `cargo run -p engine_rhi --example triangle --release [fifo|mailbox|immediate|relaxed] [frames-in-flight]`
//!
//! Resize, minimize, drag: rendering continues while the window is dragged (the window
//! lives on its own thread) and the swapchain follows the size. The title shows FPS.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

#[cfg(windows)]
#[path = "../tests/common/mod.rs"]
mod common;

#[cfg(not(windows))]
fn main() {
    println!("This example runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_platform::time::HighResTimer;
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Device, Frames, Instance, InstanceDesc, PresentMode, RhiError, Surface,
        Swapchain, SwapchainDesc,
    };
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let present_mode = match args.next().as_deref() {
        Some("mailbox") => PresentMode::Mailbox,
        Some("immediate") => PresentMode::Immediate,
        Some("relaxed") => PresentMode::FifoRelaxed,
        _ => PresentMode::Fifo,
    };
    let in_flight = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);

    let mut windows = WindowSystem::start(4096).unwrap();
    let window = windows
        .create_window(&WindowDesc {
            title: String::from("engine triangle"),
            ..WindowDesc::default()
        })
        .unwrap();

    let instance = Instance::new(&InstanceDesc::default()).unwrap();
    let adapter = instance
        .select_adapter(&AdapterPreference::HighPerformance)
        .unwrap();
    println!("adapter: {}", adapter.summary());
    let device = Device::new(&instance, &adapter).unwrap();
    let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
    assert!(
        surface.supports_queue(&device, device.graphics).unwrap(),
        "graphics queue cannot present"
    );
    println!(
        "present modes: {:?}",
        Swapchain::supported_present_modes(&device, &surface).unwrap()
    );

    let mut size = (1280u32, 720u32);
    let mut swapchain = Swapchain::new(
        &device,
        &surface,
        &SwapchainDesc {
            width: size.0,
            height: size.1,
            present_mode,
            srgb: true,
        },
    )
    .unwrap();
    println!("swapchain: {swapchain:?}, frames in flight: {in_flight}");
    let triangle = common::TrianglePipeline::new(&device, swapchain.format());
    let mut frames = Frames::new(&device, device.graphics.family, in_flight).unwrap();
    let idle_timer = HighResTimer::new().unwrap();

    let mut minimized = false;
    let mut needs_recreate = false;
    let mut frame_count = 0u32;
    let mut last_title = Instant::now();
    'main: loop {
        let mut close = false;
        windows.drain_events(|event| match event.kind {
            WindowEventKind::CloseRequested => close = true,
            WindowEventKind::Resized { width, height } => {
                if width > 0 && height > 0 {
                    size = (width, height);
                    needs_recreate = true;
                }
            }
            WindowEventKind::Minimized => minimized = true,
            WindowEventKind::Restored => minimized = false,
            _ => {}
        });
        if close {
            break 'main;
        }
        if minimized {
            idle_timer.sleep(Duration::from_millis(10)).unwrap();
            continue;
        }
        if needs_recreate {
            frames.wait_idle().unwrap();
            swapchain
                .recreate(&device, &surface, size.0, size.1)
                .unwrap();
            needs_recreate = false;
        }

        let frame = frames.begin().unwrap();
        let acquired = match swapchain.acquire(frame.image_available, u64::MAX) {
            Ok(acquired) => acquired,
            Err(RhiError::Vulkan {
                result: vk::Result::ERROR_OUT_OF_DATE_KHR,
                ..
            }) => {
                needs_recreate = true;
                continue;
            }
            Err(e) => panic!("{e}"),
        };
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
        match swapchain.present(device.graphics, acquired.index) {
            Ok(suboptimal) => needs_recreate |= suboptimal || acquired.suboptimal,
            Err(RhiError::Vulkan {
                result: vk::Result::ERROR_OUT_OF_DATE_KHR,
                ..
            }) => needs_recreate = true,
            Err(e) => panic!("{e}"),
        }
        engine_profiler::frame_mark();

        frame_count += 1;
        if last_title.elapsed() >= Duration::from_secs(1) {
            let fps = f64::from(frame_count) / last_title.elapsed().as_secs_f64();
            windows
                .set_title(
                    window.id,
                    &format!(
                        "engine triangle — {fps:.0} FPS, {:?}, {}x{}",
                        swapchain.present_mode(),
                        swapchain.extent().width,
                        swapchain.extent().height
                    ),
                )
                .unwrap();
            frame_count = 0;
            last_title = Instant::now();
        }
    }

    frames.wait_idle().unwrap();
    drop(frames);
    triangle.destroy(&device);
    drop(swapchain);
    drop(surface);
    windows.destroy_window(window.id).unwrap();
    let report = instance.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
}
