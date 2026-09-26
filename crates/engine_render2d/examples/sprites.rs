//! 2D demo in a window: thousands of bouncing, y-sorted sprites with four textures (one
//! batch thanks to bindless), additive glows, and a pixel-space overlay of SDF shapes
//! (rounded panel, outline, capsules, discs) with a clipped region.
//!
//! `cargo run -p engine_render2d --example sprites --release [fifo|mailbox|immediate] [sprites] [frames-to-run]`
//!
//! The title shows FPS, CPU time of recording the canvases and of preparing them (sort,
//! batch, upload), and the GPU time of the world pass. With `frames-to-run`, it exits after
//! that many frames, prints a summary and fails on validation errors (debug builds enable
//! validation): the GPU workflow uses this as a smoke test.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

#[cfg(not(windows))]
fn main() {
    println!("This example runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_core::math::{Vec2, Vec4};
    use engine_platform::time::HighResTimer;
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_render::graph::{ImportedTexture, RenderGraph, TextureDesc, TextureState};
    use engine_render::texture::GpuTexture;
    use engine_render::upload::UploadRing;
    use engine_render2d::{
        Blend2d, Camera2d, Canvas, ClipRect, Order, Quad, QuadMode, Renderer2d, View2d,
    };
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc,
        PresentMode, RhiError, Surface, Swapchain, SwapchainDesc,
    };
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let present_mode = match args.next().as_deref() {
        Some("mailbox") => PresentMode::Mailbox,
        Some("immediate") => PresentMode::Immediate,
        _ => PresentMode::Fifo,
    };
    let sprite_count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let frames_to_run: Option<u64> = args.next().and_then(|s| s.parse().ok());
    let in_flight = 2;

    let mut windows = WindowSystem::start(4096).unwrap();
    let window = windows
        .create_window(&WindowDesc {
            title: String::from("engine sprites"),
            ..WindowDesc::default()
        })
        .unwrap();
    let instance = Instance::new(&InstanceDesc::default()).unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    println!("adapter: {}", adapter.summary());
    let device = Device::new(&instance, &adapter).unwrap();
    let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
    assert!(surface.supports_queue(&device, device.graphics).unwrap());

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
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut renderer = Renderer2d::new(
        &device,
        &mut allocator,
        &bindless,
        swapchain.format(),
        QuadMode::for_device(&device),
    )
    .unwrap();
    let ring_bytes = (sprite_count as u64 * 2 + 4096) * 72;
    let mut ring = UploadRing::new(&device, &mut allocator, in_flight, ring_bytes).unwrap();
    let mut frames = Frames::new(&device, device.graphics.family, in_flight).unwrap();
    let mut graph = RenderGraph::new(&device, in_flight);
    let idle_timer = HighResTimer::new().unwrap();

    // Four 32×32 soft-disc textures in different colors.
    let format = vk::Format::R8G8B8A8_SRGB;
    let mut textures: Vec<GpuTexture> = (0..4)
        .map(|_| GpuTexture::new(&device, &mut allocator, &bindless, format, 32, 32).unwrap())
        .collect();
    let tints: [[u8; 3]; 4] = [
        [255, 120, 90],
        [90, 200, 255],
        [140, 255, 120],
        [255, 220, 90],
    ];
    let pixels: Vec<Vec<u8>> = tints
        .iter()
        .map(|tint| {
            let mut data = Vec::with_capacity(32 * 32 * 4);
            for y in 0..32 {
                for x in 0..32 {
                    let r = Vec2::new(x as f32 - 15.5, y as f32 - 15.5).length() / 16.0;
                    let a = ((1.0 - r) * 3.0).clamp(0.0, 1.0);
                    let shade = 1.0 - 0.5 * r;
                    data.extend_from_slice(&[
                        (f32::from(tint[0]) * shade) as u8,
                        (f32::from(tint[1]) * shade) as u8,
                        (f32::from(tint[2]) * shade) as u8,
                        (a * 255.0) as u8,
                    ]);
                }
            }
            data
        })
        .collect();
    let texture_indices: Vec<u32> = textures.iter().map(GpuTexture::sampled_index).collect();
    let mut uploaded = false;

    // Sprites bounce inside a 160 × 90 world box.
    let half_world = Vec2::new(80.0, 45.0);
    let mut positions: Vec<Vec2> = (0..sprite_count)
        .map(|i| {
            Vec2::new(
                ((i as f32 * 0.618_034).fract() * 2.0 - 1.0) * half_world.x,
                ((i as f32 * 0.414_214).fract() * 2.0 - 1.0) * half_world.y,
            )
        })
        .collect();
    let mut velocities: Vec<Vec2> = (0..sprite_count)
        .map(|i| {
            let angle = i as f32 * 2.399_963;
            Vec2::new(angle.cos(), angle.sin()) * (5.0 + (i % 17) as f32)
        })
        .collect();
    let mut world = Canvas::new();
    let mut overlay = Canvas::new();

    let start = Instant::now();
    let (mut total_frames, mut minimized, mut needs_recreate) = (0u64, false, false);
    let (mut frame_count, mut cpu_record, mut cpu_prepare) = (0u32, Duration::ZERO, Duration::ZERO);
    let mut last_title = Instant::now();
    let mut last_frame = Instant::now();
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
        if frames_to_run.is_some() && start.elapsed() > Duration::from_secs(120) {
            println!(
                "timed out after {total_frames} frames (window minimized or presentation blocked)"
            );
            std::process::exit(2);
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
        let extent = swapchain.extent();
        let dt = last_frame.elapsed().as_secs_f32().min(0.05);
        last_frame = Instant::now();
        let time = start.elapsed().as_secs_f32();

        // Simulate and record.
        let recording = Instant::now();
        world.clear();
        for (i, (p, v)) in positions.iter_mut().zip(&mut velocities).enumerate() {
            *p += *v * dt;
            if p.x.abs() > half_world.x {
                v.x = -v.x;
                p.x = p.x.clamp(-half_world.x, half_world.x);
            }
            if p.y.abs() > half_world.y {
                v.y = -v.y;
                p.y = p.y.clamp(-half_world.y, half_world.y);
            }
            world.draw(
                &Quad::sprite(texture_indices[i % 4], *p, Vec2::splat(2.0))
                    .order(Order::y_sorted(0, p.y - 1.0)),
            );
        }
        for k in 0..8 {
            let angle = time * 0.5 + k as f32 * core::f32::consts::TAU / 8.0;
            let center = Vec2::new(angle.cos(), angle.sin()) * 30.0;
            world.draw(
                &Quad::circle(center, 12.0)
                    .color(Vec4::new(0.25, 0.12, 0.05, 1.0))
                    .blend(Blend2d::Additive)
                    .order(Order::layer(1)),
            );
        }
        let (w, h) = (extent.width as f32, extent.height as f32);
        overlay.clear();
        overlay.draw(
            &Quad::rounded_rect(Vec2::new(170.0, 90.0), Vec2::new(300.0, 140.0), 16.0)
                .color(Vec4::new(0.02, 0.02, 0.03, 0.75)),
        );
        overlay.draw(
            &Quad::rounded_rect(Vec2::new(170.0, 90.0), Vec2::new(300.0, 140.0), 16.0)
                .color(Vec4::new(0.8, 0.8, 0.9, 1.0))
                .outline(2.0),
        );
        overlay.set_clip(Some(ClipRect {
            x: 40,
            y: 40,
            width: 260,
            height: 100,
        }));
        for k in 0..12 {
            let x = 30.0 + ((time * 60.0 + k as f32 * 30.0) % 300.0);
            overlay.draw(
                &Quad::line(Vec2::new(x, 60.0), Vec2::new(x + 20.0, 120.0), 6.0)
                    .color(Vec4::new(0.3, 0.7, 1.0, 1.0)),
            );
        }
        overlay.set_clip(None);
        for k in 0..5 {
            overlay.draw(
                &Quad::circle(Vec2::new(w - 40.0 - k as f32 * 36.0, h - 40.0), 14.0)
                    .color(Vec4::new(1.0, 0.4 + 0.1 * k as f32, 0.2, 1.0)),
            );
        }
        let recorded = recording.elapsed();

        let preparing = Instant::now();
        world.prepare();
        overlay.prepare();
        {
            let mut fg = graph.begin(&frame);
            let desc = TextureDesc::new_2d(swapchain.format(), extent.width, extent.height);
            let backbuffer = fg.import_texture(
                "backbuffer",
                ImportedTexture {
                    image: swapchain.images()[index],
                    view: swapchain.views()[index],
                    desc,
                    initial: TextureState::acquired_swapchain_image(),
                    final_state: Some(TextureState::present()),
                    sampled_index: None,
                    storage_index: None,
                },
            );
            ring.begin(&frame);
            if !uploaded {
                for (texture, data) in textures.iter().zip(&pixels) {
                    texture.upload(&mut fg, &mut ring, data).unwrap();
                }
                uploaded = true;
            }
            let camera = Camera2d {
                center: Vec2::ZERO,
                pixels_per_unit: (w / (2.0 * half_world.x)).min(h / (2.0 * half_world.y)),
                rotation: 0.0,
            };
            renderer
                .add_pass(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &world,
                    &camera.view(extent.width, extent.height),
                    backbuffer,
                    Some([0.01, 0.01, 0.02, 1.0]),
                )
                .unwrap();
            renderer
                .add_pass(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &overlay,
                    &View2d::pixels(extent.width, extent.height),
                    backbuffer,
                    None,
                )
                .unwrap();
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        let prepared = preparing.elapsed();
        cpu_record += recorded;
        cpu_prepare += prepared;
        total_frames += 1;

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
        if frames_to_run.is_some_and(|n| total_frames >= n) {
            break 'main;
        }

        frame_count += 1;
        if last_title.elapsed() >= Duration::from_secs(1) {
            let seconds = last_title.elapsed().as_secs_f64();
            let gpu_world = graph
                .gpu_pass_times()
                .iter()
                .find(|t| t.name == "2d")
                .map_or(0.0, |t| t.gpu_us);
            windows
                .set_title(
                    window.id,
                    &format!(
                        "engine sprites — {:.0} FPS, {} quads, record {:.0} µs, prepare+graph {:.0} µs, world GPU {:.0} µs, {:?}, {}x{}",
                        f64::from(frame_count) / seconds,
                        world.len() + overlay.len(),
                        cpu_record.as_secs_f64() * 1e6 / f64::from(frame_count),
                        cpu_prepare.as_secs_f64() * 1e6 / f64::from(frame_count),
                        gpu_world,
                        swapchain.present_mode(),
                        extent.width,
                        extent.height
                    ),
                )
                .unwrap();
            frame_count = 0;
            cpu_record = Duration::ZERO;
            cpu_prepare = Duration::ZERO;
            last_title = Instant::now();
        }
    }

    frames.wait_idle().unwrap();
    println!(
        "GPU pass times: {:?}; world {:?}; overlay {:?}",
        graph.gpu_pass_times(),
        world.stats(),
        overlay.stats()
    );
    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        renderer.destroy(&mut allocator);
        for texture in &mut textures {
            texture.destroy(&device, &mut allocator, &bindless, frames.submitted());
        }
    }
    drop(frames);
    drop(renderer);
    drop(bindless);
    drop(allocator);
    drop(swapchain);
    drop(surface);
    windows.destroy_window(window.id).unwrap();
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{total_frames} frames in {elapsed:.2} s ({:.0} FPS)",
        total_frames as f64 / elapsed
    );
    let report = instance.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    if report.errors > 0 {
        println!("{:#?}", report.first_errors);
        std::process::exit(1);
    }
}
