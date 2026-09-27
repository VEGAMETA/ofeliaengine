//! PBR cubes with shadows in a window, through the whole stack: `engine_platform` window
//! thread → swapchain and frames in flight → render graph (swapchain imported, shadow map
//! and depth transient, present transition derived) → bindless set (the shadow map's
//! index assigned by the graph) → upload ring (frame constants and draw list) → vertex
//! pulling.
//!
//! `cargo run -p engine_render --example cubes --release [fifo|mailbox|immediate] [frames-in-flight] [frames-to-run]`
//!
//! The title shows FPS, the CPU time of declaring + compiling + recording the graph, and
//! the GPU time of the scene pass (from the graph's timestamps). With `frames-to-run`, it
//! exits after that many frames, prints a summary and fails on validation errors (debug
//! builds enable validation): the GPU workflow uses this as a smoke test.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

#[cfg(not(windows))]
fn main() {
    println!("This example runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_core::math::{Mat4, Quat, Vec3, look_at};
    use engine_platform::time::HighResTimer;
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_render::forward::{
        Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
    };
    use engine_render::graph::{ImportedTexture, RenderGraph, TextureDesc, TextureState};
    use engine_render::material::{MaterialDesc, MaterialTable};
    use engine_render::mesh::{GeometryCapacity, GeometryPool, cube, perspective};
    use engine_render::post::{HDR_FORMAT, Tonemap};
    use engine_render::texture_pool::TexturePool;
    use engine_render::upload::UploadRing;
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc,
        OutputColor, PresentMode, RhiError, Surface, Swapchain, SwapchainDesc,
    };
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let present_mode = match args.next().as_deref() {
        Some("mailbox") => PresentMode::Mailbox,
        Some("immediate") => PresentMode::Immediate,
        _ => PresentMode::Fifo,
    };
    let in_flight = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let frames_to_run: Option<u64> = args.next().and_then(|s| s.parse().ok());
    let (mut total_frames, mut total_cpu_graph) = (0u64, Duration::ZERO);

    let mut windows = WindowSystem::start(4096).unwrap();
    let window = windows
        .create_window(&WindowDesc {
            title: String::from("engine cubes"),
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
            color: OutputColor::Sdr,
            low_latency: false,
        },
    )
    .unwrap();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    // The scene renders in HDR; tonemapping writes the swapchain.
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let tonemap = Tonemap::new(&device, &bindless, swapchain.format()).unwrap();
    let mut geometry = GeometryPool::new(
        &device,
        &mut allocator,
        GeometryCapacity::default(),
        in_flight,
    )
    .unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 64 << 20, in_flight);
    let mut materials = MaterialTable::new(in_flight);
    // One material per cube of the 5 × 5 grid: color by position, alternating metals.
    let mut palette = Vec::with_capacity(25);
    for x in -2..=2i32 {
        for z in -2..=2i32 {
            palette.push(materials.add(MaterialDesc::solid(
                [
                    0.3 + 0.15 * (x + 2) as f32,
                    0.3,
                    0.3 + 0.15 * (z + 2) as f32,
                ],
                if (x + z) % 2 == 0 { 1.0 } else { 0.0 },
                0.25 + 0.15 * (z + 2) as f32,
            )));
        }
    }
    let mut ring = UploadRing::new(&device, &mut allocator, in_flight, 1 << 20).unwrap();
    // Reused every frame: steady-state frames allocate nothing.
    let mut draws: Vec<DrawItem> = Vec::with_capacity(26);
    let mut frames = Frames::new(&device, device.graphics.family, in_flight).unwrap();
    let mut graph = RenderGraph::new(&device, in_flight);
    let idle_timer = HighResTimer::new().unwrap();

    let start = Instant::now();
    let (mut minimized, mut needs_recreate) = (false, false);
    let (mut frame_count, mut cpu_graph) = (0u32, Duration::ZERO);
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
        if frames_to_run.is_some() && start.elapsed() > Duration::from_secs(120) {
            // Smoke-test mode must end even if the window never becomes visible (a locked
            // session keeps it minimized).
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

        let time = start.elapsed().as_secs_f32();
        let eye = Vec3::new(9.0 * (time * 0.2).cos(), 5.0, 9.0 * (time * 0.2).sin());
        let clip_from_world = perspective(
            50f32.to_radians(),
            extent.width as f32 / extent.height.max(1) as f32,
            0.1,
            200.0,
        ) * look_at(eye, Vec3::ZERO, Vec3::Y);
        draws.clear();
        draws.push(DrawItem {
            mesh,
            material: materials.default_material(),
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::new(14.0, 0.2, 14.0),
                Quat::IDENTITY,
                Vec3::new(0.0, -0.6, 0.0),
            ),
        });
        for x in -2..=2 {
            for z in -2..=2 {
                let angle = time * (1.0 + 0.1 * (x * 5 + z) as f32);
                draws.push(DrawItem {
                    mesh,
                    material: palette[((x + 2) * 5 + z + 2) as usize],
                    world_from_object: Mat4::from_rotation_translation(
                        Quat::from_rotation_y(angle) * Quat::from_rotation_x(angle * 0.5),
                        Vec3::new(x as f32 * 2.0, 0.8, z as f32 * 2.0),
                    ),
                });
            }
        }
        let light = DirectionalLight {
            direction: Vec3::new(-0.4, -1.0, -0.3),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.08),
        };

        let recording = Instant::now();
        {
            let mut fg = graph.begin(&frame);
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame.number)
                .unwrap();
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame.number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame.number)
                .unwrap();
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
            let depth = fg.create_texture(
                "depth",
                TextureDesc::new_2d(depth_format, extent.width, extent.height),
            );
            let hdr = fg.create_texture(
                "hdr",
                TextureDesc::new_2d(HDR_FORMAT, extent.width, extent.height),
            );
            ring.begin(&frame);
            renderer.add_passes(
                &mut fg,
                &mut ring,
                &bindless,
                &geo,
                &mats,
                &draws,
                &ForwardView {
                    camera: Camera {
                        clip_from_world,
                        position: eye,
                    },
                    light,
                    color: hdr,
                    depth,
                    clear: Some([0.02, 0.02, 0.05, 1.0]),
                    occlusion: None,
                },
            );
            tonemap.add_pass(&mut fg, &bindless, hdr, backbuffer, 1.0);
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        cpu_graph += recording.elapsed();
        total_cpu_graph += recording.elapsed();
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
            let gpu_scene = graph
                .gpu_pass_times()
                .iter()
                .find(|t| t.name == "forward")
                .map_or(0.0, |t| t.gpu_us);
            windows
                .set_title(
                    window.id,
                    &format!(
                        "engine cubes — {:.0} FPS, graph CPU {:.1} µs, forward GPU {:.1} µs, {:?}, {}x{}",
                        f64::from(frame_count) / seconds,
                        cpu_graph.as_secs_f64() * 1e6 / f64::from(frame_count),
                        gpu_scene,
                        swapchain.present_mode(),
                        extent.width,
                        extent.height
                    ),
                )
                .unwrap();
            frame_count = 0;
            cpu_graph = Duration::ZERO;
            last_title = Instant::now();
        }
    }

    frames.wait_idle().unwrap();
    println!("GPU pass times: {:?}", graph.gpu_pass_times());
    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
    }
    drop(frames);
    drop(renderer);
    drop(tonemap);
    drop(bindless);
    drop(allocator);
    drop(swapchain);
    drop(surface);
    windows.destroy_window(window.id).unwrap();
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{total_frames} frames in {elapsed:.2} s ({:.0} FPS), graph CPU {:.1} µs per frame",
        total_frames as f64 / elapsed,
        total_cpu_graph.as_secs_f64() * 1e6 / total_frames.max(1) as f64
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
