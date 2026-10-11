//! Viewports in a window: one system for 2D and 3D.
//!
//! - Left half: a 3D scene (forward PBR, shadows, HDR, tonemapped when composited) with a
//!   minimap drawn over it. The minimap is a child viewport rendered in 2D every second
//!   frame; the parent shows it through its bindless texture.
//! - Right half: 160 × 90 pixel art, integer scaled and letterboxed.
//! - Top-left corner: picture-in-picture of the scene from above, every third frame.
//!
//! The title shows FPS and the GPU time of each viewport (graph scopes).
//!
//! `cargo run -p engine_render2d --example viewports --release [frames-to-run]`
//!
//! With `frames-to-run`, it exits after that many frames and fails on validation errors
//! (debug builds enable validation): the GPU workflow uses this as a smoke test.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

#[cfg(not(windows))]
fn main() {
    println!("This example runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_core::math::{Mat4, Quat, Vec2, Vec3, Vec4, look_at};
    use engine_platform::time::HighResTimer;
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_render::forward::{
        Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
    };
    use engine_render::graph::{LoadOp, RenderGraph, TextureDesc};
    use engine_render::material::{MaterialDesc, MaterialTable};
    use engine_render::mesh::{GeometryCapacity, GeometryPool, cube, perspective};
    use engine_render::post::{HDR_FORMAT, Tonemap};
    use engine_render::present::PresentTarget;
    use engine_render::texture_pool::TexturePool;
    use engine_render::upload::UploadRing;
    use engine_render::viewport::{
        Placement, Scaling, UpdateMode, ViewportDesc, ViewportSize, Viewports,
    };
    use engine_render2d::{Canvas, Order, Quad, QuadMode, Renderer2d, View2d};
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc,
        OutputColor, PresentMode, RhiError, SamplerKind, Surface, Swapchain, SwapchainDesc,
    };
    use std::time::{Duration, Instant};

    let frames_to_run: Option<u64> = std::env::args().nth(1).and_then(|s| s.parse().ok());
    let in_flight = 2;

    let mut windows = WindowSystem::start(4096).unwrap();
    let window = windows
        .create_window(&WindowDesc {
            title: String::from("engine viewports"),
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
            present_mode: PresentMode::Fifo,
            srgb: true,
            color: OutputColor::Sdr,
            low_latency: false,
        },
    )
    .unwrap();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    let forward = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut renderer = Renderer2d::new(
        &device,
        &mut allocator,
        &bindless,
        HDR_FORMAT,
        QuadMode::for_device(&device),
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
    // One material per cube of the 5 × 5 grid.
    let mut palette = Vec::with_capacity(25);
    for x in -2..=2i32 {
        for z in -2..=2i32 {
            palette.push(materials.add(MaterialDesc::solid(
                [0.3 + 0.15 * (x + 2) as f32, 0.3, 0.8],
                if (x + z) % 2 == 0 { 1.0 } else { 0.0 },
                0.3 + 0.1 * (z + 2) as f32,
            )));
        }
    }
    let ground = materials.default_material();
    let mut ring = UploadRing::new(&device, &mut allocator, in_flight, 4 << 20).unwrap();
    let mut frames = Frames::new(&device, device.graphics.family, in_flight).unwrap();
    let mut graph = RenderGraph::new(&device, in_flight);
    let idle_timer = HighResTimer::new().unwrap();

    let mut viewports = Viewports::new(&device, in_flight);
    let scene = viewports.create(ViewportDesc {
        name: String::from("3D scene"),
        placement: Some(Placement {
            rect: [0.0, 0.0, 0.5, 1.0],
            order: 0,
        }),
        clear: Some([0.02, 0.02, 0.05, 1.0]),
        ..ViewportDesc::default()
    });
    let minimap = viewports.create(ViewportDesc {
        name: String::from("minimap"),
        size: ViewportSize::Fixed {
            width: 160,
            height: 160,
        },
        parent: Some(scene),
        placement: None,
        update: UpdateMode::Every { n: 2, offset: 0 },
        clear: Some([0.05, 0.08, 0.05, 1.0]),
        ..ViewportDesc::default()
    });
    let pixel_art = viewports.create(ViewportDesc {
        name: String::from("pixel art"),
        size: ViewportSize::Fixed {
            width: 160,
            height: 90,
        },
        placement: Some(Placement {
            rect: [0.5, 0.0, 0.5, 1.0],
            order: 1,
        }),
        scaling: Scaling::PixelPerfect,
        tonemap: false,
        clear: Some([0.2, 0.25, 0.4, 1.0]),
        ..ViewportDesc::default()
    });
    let top_view = viewports.create(ViewportDesc {
        name: String::from("top view"),
        size: ViewportSize::Placement { scale: 1.0 },
        placement: Some(Placement {
            rect: [0.01, 0.01, 0.16, 0.24],
            order: 2,
        }),
        update: UpdateMode::Every { n: 3, offset: 1 },
        clear: Some([0.0, 0.0, 0.0, 1.0]),
        ..ViewportDesc::default()
    });

    let mut draws: Vec<DrawItem> = Vec::with_capacity(26);
    let mut positions: Vec<Vec3> = Vec::with_capacity(25);
    let mut minimap_canvas = Canvas::new();
    let mut overlay_canvas = Canvas::new();
    let mut pixel_canvas = Canvas::new();

    let start = Instant::now();
    let (mut total_frames, mut minimized, mut needs_recreate) = (0u64, false, false);
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
        bindless.collect(frame.number.saturating_sub(in_flight as u64));
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

        // The 3D scene.
        draws.clear();
        positions.clear();
        draws.push(DrawItem {
            mesh,
            material: ground,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::new(14.0, 0.2, 14.0),
                Quat::IDENTITY,
                Vec3::new(0.0, -0.6, 0.0),
            ),
        });
        for x in -2..=2 {
            for z in -2..=2 {
                let angle = time * (1.0 + 0.1 * (x * 5 + z) as f32);
                let p = Vec3::new(
                    x as f32 * 2.0 + (time + z as f32).sin() * 0.3,
                    0.8,
                    z as f32 * 2.0,
                );
                positions.push(p);
                draws.push(DrawItem {
                    mesh,
                    material: palette[((x + 2) * 5 + z + 2) as usize],
                    world_from_object: Mat4::from_rotation_translation(
                        Quat::from_rotation_y(angle),
                        p,
                    ),
                });
            }
        }
        let light = DirectionalLight {
            direction: Vec3::new(-0.4, -1.0, -0.3),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.08),
        };
        let eye = Vec3::new(9.0 * (time * 0.2).cos(), 5.0, 9.0 * (time * 0.2).sin());

        // The minimap: cubes as dots, the camera as a line, top-down (x → right, z → down).
        let to_map = |p: Vec3| Vec2::new(80.0 + p.x * 12.0, 80.0 + p.z * 12.0);
        minimap_canvas.clear();
        for p in &positions {
            minimap_canvas
                .draw(&Quad::circle(to_map(*p), 5.0).color(Vec4::new(0.4, 0.6, 1.0, 1.0)));
        }
        minimap_canvas.draw(
            &Quad::line(to_map(eye), to_map(Vec3::ZERO), 2.0).color(Vec4::new(1.0, 0.8, 0.2, 1.0)),
        );
        minimap_canvas.draw(
            &Quad::rounded_rect(Vec2::splat(80.0), Vec2::splat(156.0), 12.0)
                .color(Vec4::ONE)
                .outline(2.0),
        );
        minimap_canvas.prepare();
        // Pixel art: a bouncing block and a ground line, in 160 × 90 pixels.
        pixel_canvas.clear();
        pixel_canvas.draw(
            &Quad::solid(Vec2::new(80.0, 80.0), Vec2::new(160.0, 20.0))
                .color(Vec4::new(0.3, 0.6, 0.3, 1.0)),
        );
        let bounce = (time * 3.0).sin().abs() * 30.0;
        for k in 0..5 {
            let x = 20.0 + k as f32 * 30.0;
            pixel_canvas.draw(
                &Quad::solid(
                    Vec2::new(x, 62.0 - bounce * (1.0 - 0.15 * k as f32)),
                    Vec2::splat(8.0),
                )
                .color(Vec4::new(1.0, 0.5 + 0.1 * k as f32, 0.2, 1.0))
                .order(Order::layer(1)),
            );
        }
        pixel_canvas.prepare();

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
            ring.begin(&frame);
            // The overlay needs the minimap's index, known once it has rendered.
            overlay_canvas.clear();
            if let Some(map) = viewports.texture_index(minimap) {
                overlay_canvas.draw(
                    &Quad::sprite(map, Vec2::new(100.0, 100.0), Vec2::splat(160.0))
                        .sampler(SamplerKind::LinearClamp)
                        .color(Vec4::new(1.0, 1.0, 1.0, 0.9)),
                );
            }
            overlay_canvas.prepare();
            let (draws_ref, forward_ref, renderer_ref, bindless_ref) =
                (&draws, &forward, &renderer, &bindless);
            let (minimap_ref, overlay_ref, pixel_ref) =
                (&minimap_canvas, &overlay_canvas, &pixel_canvas);
            let ring_ref = &mut ring;
            viewports
                .render(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &bindless,
                    (extent.width, extent.height),
                    frame.number,
                    |fg, vp| {
                        let clear = match vp.load {
                            LoadOp::Clear(c) => Some(c),
                            _ => None,
                        };
                        let aspect = vp.width as f32 / vp.height.max(1) as f32;
                        if vp.id == scene || vp.id == top_view {
                            let (camera_eye, up) = if vp.id == scene {
                                (eye, Vec3::Y)
                            } else {
                                (Vec3::new(0.0, 18.0, 0.01), Vec3::NEG_Z)
                            };
                            let camera = Camera {
                                clip_from_world: perspective(
                                    50f32.to_radians(),
                                    aspect,
                                    0.1,
                                    200.0,
                                ) * look_at(camera_eye, Vec3::ZERO, up),
                                position: camera_eye,
                            };
                            let depth = fg.create_texture(
                                "depth",
                                TextureDesc::new_2d(depth_format, vp.width, vp.height),
                            );
                            forward_ref.add_passes(
                                fg,
                                ring_ref,
                                bindless_ref,
                                &geo,
                                &mats,
                                draws_ref,
                                &ForwardView {
                                    camera,
                                    light,
                                    color: vp.color,
                                    depth,
                                    clear,
                                    occlusion: None,
                                    temporal: None,
                                    lights: &[],
                                    decals: &[],
                                    environment: None,
                                    reflections: Default::default(),
                                },
                            );
                            if vp.id == scene {
                                renderer_ref
                                    .add_pass(
                                        fg,
                                        ring_ref,
                                        bindless_ref,
                                        overlay_ref,
                                        &View2d::pixels(vp.width, vp.height),
                                        vp.color,
                                        None,
                                    )
                                    .unwrap();
                            }
                        } else {
                            let canvas = if vp.id == minimap {
                                minimap_ref
                            } else {
                                pixel_ref
                            };
                            renderer_ref
                                .add_pass(
                                    fg,
                                    ring_ref,
                                    bindless_ref,
                                    canvas,
                                    &View2d::pixels(vp.width, vp.height),
                                    vp.color,
                                    clear,
                                )
                                .unwrap();
                        }
                    },
                )
                .unwrap();
            // Straight into the swapchain image, or apart and copied when it presents
            // across adapters (`engine_render::present`).
            let present = PresentTarget::import(&mut fg, &swapchain, index as u32);
            let backbuffer = present.texture;
            viewports.composite(
                &mut fg,
                &tonemap,
                &bindless,
                backbuffer,
                (extent.width, extent.height),
                frame.number,
                [0.0, 0.0, 0.0, 1.0],
            );
            present.finish(&mut fg);
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
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
            let gpu = |id| viewports.gpu_time_us(&graph, id);
            windows
                .set_title(
                    window.id,
                    &format!(
                        "engine viewports — {:.0} FPS; GPU µs: 3D {:.0}, minimap {:.0}, pixel art {:.0}, top view {:.0}",
                        f64::from(frame_count) / seconds,
                        gpu(scene),
                        gpu(minimap),
                        gpu(pixel_art),
                        gpu(top_view),
                    ),
                )
                .unwrap();
            frame_count = 0;
            last_title = Instant::now();
        }
    }

    frames.wait_idle().unwrap();
    println!("GPU pass times: {:?}", graph.gpu_pass_times());
    for (name, id) in [
        ("3D scene", scene),
        ("minimap", minimap),
        ("pixel art", pixel_art),
        ("top view", top_view),
    ] {
        println!(
            "viewport {name}: {:?} px, GPU {:.1} µs in the last completed frame",
            viewports.extent(id),
            viewports.gpu_time_us(&graph, id)
        );
    }
    // SAFETY: the device is idle.
    unsafe {
        viewports.destroy_all(&mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        renderer.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
    }
    drop(frames);
    drop(forward);
    drop(renderer);
    drop(tonemap);
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
