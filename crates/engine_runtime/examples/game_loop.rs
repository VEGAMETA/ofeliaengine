//! A game frame loop on `engine_runtime` (ADR 0064): one ECS world simulated at 64 Hz,
//! extracted into a persistent GPU scene, interpolated and drawn every render frame by the
//! frame renderer (ADR 0072), with the camera entity following the mouse every render
//! frame (written in the presentation step, read by a `CameraView`).
//!
//! The world: a field of static props (most of a game's scene) and spinning hierarchies
//! (movers, about 5 %). Move the mouse over the window to turn the camera. At the end it
//! prints where a frame's CPU time went and how long input took to reach the GPU: from
//! the newest mouse event a frame used (its arrival on the window thread) to the moment
//! the frame's camera was written for the GPU. Input is sampled late: after the frame
//! has its slot and swapchain image, right before the ticks. Until the mouse moves the
//! input is synthetic (sampled at that point).
//!
//! ```text
//! cargo run -p engine_runtime --release --example game_loop -- [frames] [props] [--vsync]
//!     [--aa off|taau|fsr] [--quality native|quality|balanced|performance]
//! ```

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::too_many_lines
)]

/// The engine's global allocator (ADR 0086).
#[global_allocator]
static ALLOCATOR: engine_core::alloc::EngineAllocator = engine_core::alloc::EngineAllocator::new();

#[cfg(not(windows))]
fn main() {
    println!("game_loop: Windows only");
}

#[cfg(windows)]
fn main() {
    windowed::run();
}

#[cfg(windows)]
mod windowed {
    use engine_core::math::{Mat4, Quat, Vec3};
    use engine_ecs::{GlobalTransform, QueryState, Transform, World, bytemuck, pod_component};
    use engine_frame::{
        Antialiasing, FrameContext, FrameRenderer, PrepareContext, Quality, RenderDefaults,
        SceneDraws, SceneInput, View, ViewTarget,
    };
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_render::forward::{DirectionalLight, ShadowSettings};
    use engine_render::gpu_scene::{GpuScene, InstanceId, SceneInstance};
    use engine_render::graph::{LoadOp, RenderGraph};
    use engine_render::material::{MaterialDesc, MaterialTable};
    use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
    use engine_render::present::PresentTarget;
    use engine_render::texture_pool::TexturePool;
    use engine_render::upload::UploadRing;
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc,
        OutputColor, PresentMode, RhiError, Surface, Swapchain, SwapchainDesc,
    };
    use engine_runtime::{Camera, CameraView, FrameStages, MeshRenderer, Runtime, look_rotation};
    use std::time::{Duration, Instant};

    /// Turns its entity about Y, radians per second.
    #[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct Spin(f32);
    pod_component!(Spin);

    /// What the frame samples before running: the mouse.
    #[derive(Debug, Clone, Copy, Default)]
    struct Input {
        /// Cursor x in the window, pixels.
        cursor_x: f32,
        /// When the newest cursor event arrived (synthetic input: when it was sampled).
        at: Option<Instant>,
    }

    fn median(v: &mut [f64]) -> f64 {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    }

    pub(crate) fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let vsync = args.iter().any(|a| a == "--vsync");
        let nums: Vec<usize> = args.iter().filter_map(|a| a.parse().ok()).collect();
        let frames_to_run = nums.first().copied().unwrap_or(2000);
        let option = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .map(String::as_str)
        };
        let props = nums.get(1).copied().unwrap_or(20_000);

        let mut windows = WindowSystem::start(4096).unwrap();
        let window = windows
            .create_window(&WindowDesc {
                title: String::from("game loop"),
                width: 1600,
                height: 900,
                resizable: false,
                visible: true,
            })
            .unwrap();
        let instance = Instance::new(&InstanceDesc::default()).unwrap();
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference).unwrap();
        let adapter_name = adapter.summary();
        let device = Device::new(&instance, &adapter).unwrap();
        let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
        let in_flight = 2;
        let mut swapchain = Swapchain::new(
            &device,
            &surface,
            &SwapchainDesc {
                width: 1600,
                height: 900,
                present_mode: if vsync {
                    PresentMode::Fifo
                } else {
                    PresentMode::Immediate
                },
                srgb: true,
                color: OutputColor::Sdr,
                low_latency: false,
            },
        )
        .unwrap();
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let depth_format = vk::Format::D32_SFLOAT;
        let mut frame_renderer = FrameRenderer::new(
            &device,
            &bindless,
            swapchain.format(),
            depth_format,
            ShadowSettings::default(),
            in_flight,
        )
        .unwrap();
        // The device class's measured defaults (ADR 0072), unless the command line says.
        let defaults = RenderDefaults::for_device(&device);
        let antialiasing = match option("--aa") {
            Some("off") => Antialiasing::OFF,
            Some("taau") => Antialiasing::TAAU,
            Some("fsr") => Antialiasing::FSR,
            _ => defaults.antialiasing,
        };
        let quality = match option("--quality") {
            Some("native") => Quality::Native,
            Some("quality") => Quality::Quality,
            Some("balanced") => Quality::Balanced,
            Some("performance") => Quality::Performance,
            _ => defaults.quality,
        };
        let mut geometry = GeometryPool::new(
            &device,
            &mut allocator,
            GeometryCapacity::default(),
            in_flight,
        )
        .unwrap();
        let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
        let mut textures = TexturePool::new(&device, 16 << 20, in_flight);
        let mut materials = MaterialTable::new(in_flight);
        let palette: Vec<_> = (0..16)
            .map(|k| {
                let h = k as f32 / 16.0;
                materials.add(MaterialDesc::solid(
                    [0.3 + 0.6 * h, 0.55, 0.9 - 0.6 * h],
                    0.0,
                    0.5,
                ))
            })
            .collect();
        let ground = materials.default_material();
        let mut ring = UploadRing::new(&device, &mut allocator, in_flight, 16 << 20).unwrap();
        let mut frames = Frames::new(&device, device.graphics.family, in_flight).unwrap();
        let mut graph = RenderGraph::new(&device, in_flight);

        // The world: props on a field, and spinning hierarchies among them.
        let mut world = World::new();
        let side = (props as f32).sqrt().ceil() as usize;
        for i in 0..props {
            let (x, z) = ((i % side) as f32, (i / side) as f32);
            let at = Vec3::new(x * 2.0 - side as f32, 0.4, z * 2.0 - side as f32);
            world.spawn((
                Transform::from_translation(at).with_scale(Vec3::splat(0.8)),
                GlobalTransform::IDENTITY,
                MeshRenderer::new(mesh, palette[i % palette.len()]),
            ));
        }
        let spinners = (props / 200).max(1);
        for s in 0..spinners {
            let angle = s as f32 / spinners as f32 * core::f32::consts::TAU;
            let root = world.spawn((
                Transform::from_xyz(angle.cos() * 12.0, 2.0, angle.sin() * 12.0),
                GlobalTransform::IDENTITY,
                MeshRenderer::new(mesh, palette[s % palette.len()]),
                Spin(0.5 + (s % 5) as f32 * 0.3),
            ));
            for c in 0..9 {
                let a = c as f32 * 0.7;
                let child = world.spawn((
                    Transform::from_xyz(a.cos() * 1.5, 0.3 * c as f32, a.sin() * 1.5)
                        .with_scale(Vec3::splat(0.4)),
                    GlobalTransform::IDENTITY,
                    MeshRenderer::new(mesh, palette[(s + c) % palette.len()]),
                ));
                world.set_parent(child, Some(root)).unwrap();
            }
        }
        let entities = world.len();
        let mut scene = GpuScene::new(&device, &bindless, entities as u32 + 16, in_flight).unwrap();
        scene.insert(SceneInstance {
            mesh,
            material: ground,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::new(side as f32 * 2.0 + 20.0, 0.2, side as f32 * 2.0 + 20.0),
                Quat::IDENTITY,
                Vec3::new(0.0, -0.1, 0.0),
            ),
        });
        // The player's camera: an entity, moved every rendered frame.
        let camera = world.spawn((
            Transform::IDENTITY,
            Camera::perspective(60f32.to_radians(), 0.1, 400.0)
                .with_antialiasing(antialiasing, quality),
        ));
        let mut cameras = CameraView::new(&mut world, defaults.view_settings());
        let mut view = View::new(&device, entities as u32 + 16);
        let mut runtime = Runtime::<InstanceId>::new(world, 64.0);
        let mut spins = QueryState::<(&mut Transform, &Spin)>::new(&mut runtime.world);

        let light = DirectionalLight {
            direction: Vec3::new(-0.4, -1.0, -0.3),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.08),
        };
        let mut input = Input::default();
        let mut composer = Vec::new();
        let mut last = Instant::now();
        let start = last;
        let (mut stages, mut latency, mut upload, mut record, mut frame_times) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut mouse_frames = 0usize;
        let mut mouse = false;
        let mut drawn = 0usize;
        'main: loop {
            // 1. Input: the newest cursor position, and when it arrived.
            let mut close = false;
            let mut moved = false;
            // Cursor events from before the loop (the window appearing under the cursor) are
            // not input to measure: one of them once made the median 637 ms.
            windows.drain_events(|e| match e.kind {
                WindowEventKind::CloseRequested => close = true,
                WindowEventKind::CursorMoved { x, .. } if e.time >= start => {
                    input.cursor_x = x as f32;
                    input.at = Some(e.time);
                    moved = true;
                }
                _ => {}
            });
            if close || drawn >= frames_to_run || start.elapsed() > Duration::from_secs(120) {
                break 'main;
            }
            // 2. Wait for a frame slot and an image first, then sample input again: the
            // input a frame uses is as late as it can be (CLAUDE.md §7).
            let frame = frames.begin().unwrap();
            let acquired = match swapchain.acquire(frame.image_available, u64::MAX) {
                Ok(a) => a,
                Err(RhiError::Vulkan {
                    result: vk::Result::ERROR_OUT_OF_DATE_KHR,
                    ..
                }) => {
                    frames.wait_idle().unwrap();
                    let e = swapchain.extent();
                    swapchain
                        .recreate(&device, &surface, e.width, e.height)
                        .unwrap();
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            windows.drain_events(|e| {
                if let WindowEventKind::CursorMoved { x, .. } = e.kind
                    && e.time >= start
                {
                    input.cursor_x = x as f32;
                    input.at = Some(e.time);
                    moved = true;
                }
            });
            let now = Instant::now();
            let mut fresh = moved;
            if moved {
                mouse = true;
                mouse_frames += 1;
            } else if !mouse {
                // Synthetic input until the mouse moves: a slow sweep, sampled now.
                input.cursor_x = 800.0 + 600.0 * (now - start).as_secs_f32().sin();
                input.at = Some(now);
                fresh = true;
            }
            let elapsed = (now - last).as_secs_f64();
            last = now;

            // 3. Ticks, then the per-frame camera, then interpolation.
            let frame_start = Instant::now();
            let s: FrameStages = runtime.frame(
                elapsed,
                &input,
                &mut scene,
                |world, _input, dt| {
                    spins.for_each(world, |(t, spin)| {
                        t.rotation *= Quat::from_rotation_y(spin.0 * dt);
                    });
                },
                |world, input, _alpha| {
                    // The camera follows the newest input every frame, not every tick.
                    let yaw = (input.cursor_x / 1600.0 - 0.5) * core::f32::consts::TAU;
                    let eye = Vec3::new(yaw.sin() * 40.0, 22.0, yaw.cos() * 40.0);
                    if let Some(t) = world.get_mut::<Transform>(camera) {
                        *t = Transform::from_translation(eye).with_rotation(look_rotation(
                            eye,
                            Vec3::ZERO,
                            Vec3::Y,
                        ));
                    }
                },
            );
            stages.push(s);

            // 4. Render.
            let extent = swapchain.extent();
            let rec = Instant::now();
            let main = cameras.resolve(&runtime.world).unwrap();
            frame_renderer
                .prepare(
                    &mut view,
                    &main.settings,
                    (extent.width, extent.height),
                    PrepareContext {
                        device: &device,
                        allocator: &mut allocator,
                        bindless: &bindless,
                        materials: &mut materials,
                        frame_number: frame.number,
                        skinning: None,
                    },
                )
                .unwrap();
            let mut composed = rec.elapsed().as_secs_f64() * 1e6;
            {
                let mut fg = graph.begin(&frame);
                ring.begin(&frame);
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame.number)
                    .unwrap();
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame.number)
                    .unwrap();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, frame.number)
                    .unwrap();
                let t = Instant::now();
                let scene_frame = scene
                    .add_passes(
                        &mut fg,
                        &device,
                        &mut allocator,
                        &mut ring,
                        &bindless,
                        &geo,
                        &mats,
                        frame.number,
                    )
                    .unwrap()
                    .unwrap();
                upload.push(t.elapsed().as_secs_f64() * 1e6);
                // Straight into the swapchain image, or apart and copied when it presents
                // across adapters (`engine_render::present`).
                let present = PresentTarget::import(&mut fg, &swapchain, acquired.index);
                let backbuffer = present.texture;
                // The camera is written for the GPU here: input → here is the latency.
                if let Some(at) = input.at.filter(|_| fresh) {
                    latency.push(at.elapsed().as_secs_f64() * 1e6);
                }
                let t = Instant::now();
                frame_renderer
                    .add_view(
                        &mut fg,
                        &mut FrameContext {
                            device: &device,
                            allocator: &mut allocator,
                            bindless: &bindless,
                            ring: &mut ring,
                            frame_number: frame.number,
                            dt: elapsed as f32,
                        },
                        &mut view,
                        &SceneInput {
                            geometry: &geo,
                            materials: &mats,
                            draws: SceneDraws::Scene(&scene_frame),
                            skinning: None,
                            light,
                            lights: &[],
                            decals: &[],
                            environment: None,
                            reflection_probes: None,
                        },
                        &main.camera,
                        ViewTarget {
                            texture: backbuffer,
                            rect: None,
                            load: LoadOp::DontCare,
                        },
                    )
                    .unwrap();
                composed += t.elapsed().as_secs_f64() * 1e6;
                present.finish(&mut fg);
                fg.execute_with_bindless(&mut allocator, Some(&bindless))
                    .unwrap();
            }
            frames
                .submit(
                    device.graphics,
                    frame,
                    true,
                    Some(swapchain.render_finished(acquired.index)),
                )
                .unwrap();
            let _ = swapchain.present(device.graphics, acquired.index);
            record.push(rec.elapsed().as_secs_f64() * 1e6);
            composer.push(composed);
            frame_times.push(frame_start.elapsed().as_secs_f64() * 1e6);
            drawn += 1;
        }
        frames.wait_idle().unwrap();
        let ticks: u32 = stages.iter().map(|s| s.ticks).sum();
        let column = |f: fn(&FrameStages) -> f64| {
            let mut v: Vec<f64> = stages.iter().map(f).collect();
            median(&mut v)
        };
        let ticked = |f: fn(&FrameStages) -> f64| {
            let mut v: Vec<f64> = stages.iter().filter(|s| s.ticks > 0).map(f).collect();
            median(&mut v)
        };
        println!(
            "### Game loop: {entities} entities ({spinners} spinning hierarchies of 10), 64 Hz ticks\n"
        );
        println!(
            "Adapter: {adapter_name}; {drawn} frames, {ticks} ticks; present mode {:?}; mouse input in {mouse_frames} frames\n",
            swapchain.present_mode()
        );
        println!("| stage | median µs |");
        println!("|---|---:|");
        println!(
            "| tick systems (frames with a tick) | {:.1} |",
            ticked(|s| s.simulate_us)
        );
        println!(
            "| transform propagation (frames with a tick) | {:.1} |",
            ticked(|s| s.propagate_us)
        );
        println!(
            "| extraction (frames with a tick) | {:.1} |",
            ticked(|s| s.extract_us)
        );
        println!("| per-frame camera | {:.1} |", column(|s| s.present_us));
        println!("| interpolation | {:.1} |", column(|s| s.interpolate_us));
        println!("| scene upload (CPU) | {:.1} |", median(&mut upload));
        println!(
            "| frame renderer: prepare + add_view (CPU, {}) | {:.1} |",
            antialiasing,
            median(&mut composer)
        );
        println!("| record + submit + present | {:.1} |", median(&mut record));
        println!("| whole frame | {:.1} |", median(&mut frame_times));
        println!(
            "| **input → camera written for the GPU** | **{:.0}** |",
            median(&mut latency)
        );
        println!();

        // SAFETY: the device is idle.
        unsafe {
            frame_renderer.destroy(&device, &mut allocator, &bindless);
            view.destroy(&mut allocator, &bindless);
            scene.destroy(&mut allocator);
            graph.destroy(&mut allocator, Some(&bindless));
            ring.destroy(&mut allocator);
            geometry.destroy(&mut allocator);
            materials.destroy(&mut allocator);
            textures.destroy(&mut allocator, &bindless);
        }
        drop(frames);
        drop(frame_renderer);
        drop(bindless);
        drop(allocator);
        drop(swapchain);
        drop(surface);
        windows.destroy_window(window.id).unwrap();
    }
}
