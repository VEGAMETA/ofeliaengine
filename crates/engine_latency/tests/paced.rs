//! The paced presenter (ADR 0091) on a window of its own, under the validation layer:
//! frames handed over with a generated frame each are presented twice as often, the
//! generated frame first and the real one half a frame later once the spacing has its
//! window of intervals, and the swapchain comes back on stop.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

#[cfg(windows)]
#[test]
fn generated_frames_go_out_half_a_frame_before_real_ones() {
    use engine_latency::{PacedFrame, PacedPresenter};
    use engine_platform::window::{WindowDesc, WindowSystem};
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Device, Frames, Instance, InstanceDesc, OutputColor,
        PresentMode, Surface, Swapchain, SwapchainDesc,
    };
    use std::time::{Duration, Instant};

    let Ok(windows) = WindowSystem::start(64) else {
        println!("skipping: no window system");
        return;
    };
    let Ok(window) = windows.create_window(&WindowDesc {
        title: String::from("engine_latency paced presenter test"),
        width: 256,
        height: 256,
        resizable: false,
        visible: true,
        ..WindowDesc::default()
    }) else {
        println!("skipping: no window");
        return;
    };
    let Ok(instance) = Instance::new(&InstanceDesc {
        app_name: String::from("engine_latency paced test"),
        validation: true,
        debug_utils: true,
        surface: true,
        headless_surface: false,
        sync_validation: false,
    }) else {
        println!("skipping: no Vulkan");
        return;
    };
    let adapter = instance
        .select_adapter(
            &AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance),
        )
        .unwrap();
    println!("adapter: {}", adapter.summary());
    let device = Device::new(&instance, &adapter).unwrap();
    if device.present.is_none() {
        println!("skipping: one graphics queue only");
        return;
    }
    let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
    let modes = Swapchain::supported_present_modes(&device, &surface).unwrap();
    // Presents not held for vblank: the spacing measured is the presenter's.
    let present_mode = [PresentMode::Immediate, PresentMode::Mailbox]
        .into_iter()
        .find(|m| modes.contains(m))
        .unwrap_or(PresentMode::Fifo);
    let swapchain = Swapchain::new(
        &device,
        &surface,
        &SwapchainDesc {
            width: 256,
            height: 256,
            present_mode,
            srgb: true,
            color: OutputColor::Sdr,
            low_latency: false,
            ..SwapchainDesc::default()
        },
    )
    .unwrap();
    println!("swapchain: {swapchain:?}");
    let mut allocator = Allocator::new(&device);
    let mut frames = Frames::new(&device, device.graphics.family, 2).unwrap();
    let mut paced = match PacedPresenter::start(&device, &mut allocator, swapchain, None) {
        Ok(paced) => paced,
        Err(failed) => {
            println!("skipping: {}", failed.1);
            return;
        }
    };

    // 50 real frames a second, each with a generated frame.
    let interval = Duration::from_millis(20);
    let count = 40u64;
    let mut presented = Vec::new();
    let mut next = Instant::now();
    let mut sleeper = engine_platform::time::PreciseSleeper::new().unwrap();
    let d = device.raw();
    for k in 0..count {
        let frame = frames.begin().unwrap();
        let images = paced.next_images();
        let clear = |image: vk::Image, color: [f32; 4]| {
            let range = engine_rhi::color_range();
            let to_dst = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(image)
                .subresource_range(range)];
            let to_src = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(image)
                .subresource_range(range)];
            // SAFETY: recording the frame's command buffer; the image is the presenter's,
            // free for this frame (the submission waits for its last copies).
            unsafe {
                d.cmd_pipeline_barrier2(
                    frame.cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&to_dst),
                );
                d.cmd_clear_color_image(
                    frame.cmd,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &vk::ClearColorValue { float32: color },
                    &[range],
                );
                d.cmd_pipeline_barrier2(
                    frame.cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&to_src),
                );
            }
        };
        let shade = k as f32 / count as f32;
        clear(images.real.image, [shade, 0.2, 0.4, 1.0]);
        clear(images.generated.image, [0.4, shade, 0.2, 1.0]);
        let number = frame.number;
        frames
            .submit_waiting(device.graphics, frame, false, Some(images.wait), None, None)
            .unwrap();
        paced
            .present(PacedFrame {
                done: (frames.timeline(), number),
                generated: k > 0,
                present_id: None,
                reset: k == 0,
            })
            .unwrap();
        paced.drain_presented(|p| presented.push(p));
        next += interval;
        sleeper.sleep_until(next).unwrap();
    }
    frames.wait_idle().unwrap();
    let swapchain = paced.stop(&mut allocator).unwrap();
    println!("swapchain back: {swapchain:?}");
    drop(swapchain);
    drop(frames);
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);

    let generated = presented.iter().filter(|p| p.generated).count();
    let real = presented.len() - generated;
    println!("presented: {real} real, {generated} generated (of {count} frames)");
    // The last frames' reports may still have been in the ring at stop: count the rest.
    assert!(real as u64 >= count - 2 && generated as u64 >= count - 3);
    // Once the spacing has 10 intervals, the real frame follows the generated one by it.
    let mut gaps = Vec::new();
    for pair in presented.windows(2) {
        let (g, r) = (pair[0], pair[1]);
        if g.generated && !r.generated && g.frame == r.frame && g.frame > 12 {
            let gap = r.presented - g.presented;
            gaps.push((gap.as_secs_f64() * 1e3, r.spacing.as_secs_f64() * 1e3));
        }
    }
    println!("generated → real gaps and spacing (ms): {gaps:.2?}");
    assert!(gaps.len() >= 20);
    let mut errors: Vec<f64> = gaps
        .iter()
        .map(|(gap, spacing)| (gap - spacing).abs())
        .collect();
    errors.sort_by(f64::total_cmp);
    let median = errors[errors.len() / 2];
    println!("median |gap − spacing|: {median:.3} ms");
    assert!(median < 1.0, "{gaps:?}");
    // 20 ms frames: about 10 ms less the margins (the unit tests check the rule exactly).
    // A frame late under load lengthens the mean of the last 10 intervals, and the spacing
    // with it: 10.015 ms was read once in the gate's parallel tests.
    for (_, spacing) in &gaps {
        assert!((8.5..10.5).contains(spacing), "{gaps:?}");
    }
}
