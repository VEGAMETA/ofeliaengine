//! The editor: the shell (the team's Godot layout) in a window, over a 3D viewport.
//!
//! ```text
//! cargo run -p engine_editor --release --bin editor -- [--frames N] [--present fifo|mailbox|immediate]
//! ```
//!
//! - Middle drag orbits the viewport camera, Shift + middle drag pans, right drag looks
//!   around, the wheel zooms (`docs/research/ux/viewport-navigation.md`).
//! - The scene is demo data until the panels are bound to the ECS.
//! - `--frames N` renders N frames and exits (smoke tests); the report on stdout includes
//!   the cold start time (CLAUDE.md §5: at most 2 s).
//! - Frames are capped just below the monitor's refresh rate by `engine_latency` (Reflex,
//!   Anti-Lag 2 or the generic limiter): drivers may override v-sync. `--fps N` sets
//!   another cap, `--fps 0` none.
//! - `--validate` turns on the Vulkan validation layer with synchronization validation
//!   (always on in debug builds); the report counts its errors.
//! - --no-throttle keeps Windows from slowing the editor while its window is not in
//!   front (for measurements; by default the editor saves power in the background).

#[cfg(not(windows))]
fn main() {
    #![allow(clippy::print_stdout)]
    println!("The editor runs on Windows only.");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windowed::run()
}

#[cfg(windows)]
mod windowed {
    #![allow(clippy::print_stdout)]

    use engine_core::math::{Mat4, Quat, Vec2, Vec3};
    use engine_editor::camera::OrbitCamera;
    use engine_editor::icons;
    use engine_editor::input::{Button, InputCollector};
    use engine_editor::paint::{UiText, paint};
    use engine_editor::shell::{
        Field, FileItem, FileKind, Palette, Section, ShellModel, TreeItem, build,
    };
    use engine_latency::{Latency, LatencySettings, LowLatency, Marker};
    use engine_platform::time::HighResTimer;
    use engine_platform::window::{
        MouseButton, WindowDesc, WindowEventKind, WindowSystem, refresh_rate,
    };
    use engine_render::forward::{
        Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Material, ShadowSettings,
    };
    use engine_render::graph::{ImportedTexture, LoadOp, RenderGraph, TextureDesc, TextureState};
    use engine_render::mesh::{Mesh, cube};
    use engine_render::post::{HDR_FORMAT, Tonemap};
    use engine_render::upload::UploadRing;
    use engine_render::viewport::{UpdateMode, ViewportDesc, ViewportSize, Viewports};
    use engine_render2d::{Canvas, GlyphTexture, Order, Quad, QuadMode, Renderer2d, View2d};
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc,
        PresentMode, RhiError, Surface, Swapchain, SwapchainDesc,
    };
    use engine_ui::{Theme, Ui};
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    type Error = Box<dyn std::error::Error>;

    /// Frames the CPU may record ahead of the GPU.
    const IN_FLIGHT: usize = 2;
    /// The viewport's texture format: sRGB, written by the tone map.
    const VIEW_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
    const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;

    /// Frame-rate cap.
    #[derive(Clone, Copy, PartialEq)]
    enum FpsCap {
        /// Just below the monitor's refresh rate (keeps variable refresh in range, and the
        /// editor from burning power when the driver ignores v-sync).
        Monitor,
        /// A fixed rate.
        Fixed(f64),
        /// No cap (measurements).
        Uncapped,
    }

    impl FpsCap {
        fn limit(self, monitor_hz: Option<u32>) -> Option<f64> {
            match self {
                Self::Monitor => monitor_hz.map(|hz| f64::from(hz) * 0.97),
                Self::Fixed(fps) => Some(fps),
                Self::Uncapped => None,
            }
        }
    }

    struct Options {
        frames: Option<u64>,
        present: PresentMode,
        fps: FpsCap,
        validate: bool,
        no_throttle: bool,
    }

    impl Options {
        fn parse() -> Self {
            let mut options = Self {
                frames: None,
                present: PresentMode::Fifo,
                fps: FpsCap::Monitor,
                validate: cfg!(debug_assertions),
                no_throttle: false,
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--frames" => options.frames = args.next().and_then(|n| n.parse().ok()),
                    "--present" => {
                        options.present = match args.next().as_deref() {
                            Some("mailbox") => PresentMode::Mailbox,
                            Some("immediate") => PresentMode::Immediate,
                            _ => PresentMode::Fifo,
                        }
                    }
                    "--fps" => {
                        options.fps = match args.next().and_then(|n| n.parse::<f64>().ok()) {
                            Some(fps) if fps > 0.0 => FpsCap::Fixed(fps),
                            Some(_) => FpsCap::Uncapped,
                            None => FpsCap::Monitor,
                        }
                    }
                    "--validate" => options.validate = true,
                    "--no-throttle" => options.no_throttle = true,
                    other => println!("ignoring unknown argument {other}"),
                }
            }
            options
        }
    }

    /// A node of the demo scene tree (until the tree comes from the ECS).
    struct Node {
        depth: u16,
        icon: &'static str,
        name: &'static str,
        kind: &'static str,
        expanded: Option<bool>,
    }

    fn node(
        depth: u16,
        icon: &'static str,
        name: &'static str,
        kind: &'static str,
        expanded: Option<bool>,
    ) -> Node {
        Node {
            depth,
            icon,
            name,
            kind,
            expanded,
        }
    }

    fn demo_tree() -> Vec<Node> {
        vec![
            node(0, icons::NODE_3D, "World", "Node3D", Some(true)),
            node(1, icons::SUN, "Sun", "DirectionalLight", None),
            node(
                1,
                icons::ENVIRONMENT,
                "Environment",
                "WorldEnvironment",
                None,
            ),
            node(1, icons::NODE_3D, "Level", "Node3D", Some(true)),
            node(2, icons::MESH_INSTANCE, "Ground", "MeshInstance", None),
            node(2, icons::NODE_3D, "Crates", "Node3D", Some(false)),
            node(3, icons::MESH_INSTANCE, "Crate", "MeshInstance", None),
            node(2, icons::MESH_INSTANCE, "Tower", "MeshInstance", None),
            node(1, icons::CHARACTER, "Player", "CharacterBody", Some(true)),
            node(2, icons::CAMERA, "Camera", "Camera3D", None),
            node(2, icons::MESH_INSTANCE, "Model", "MeshInstance", None),
            node(2, icons::COLLISION, "Collider", "CollisionShape", None),
            node(1, icons::CONTROL, "Интерфейс", "Control", Some(false)),
            node(2, icons::CONTROL, "Меню", "Control", None),
        ]
    }

    /// Rows of the tree that are visible (no collapsed ancestor), as indices into `tree`.
    fn visible_rows(tree: &[Node], rows: &mut Vec<usize>) {
        rows.clear();
        let mut hidden_below: Option<u16> = None;
        for (i, n) in tree.iter().enumerate() {
            if let Some(depth) = hidden_below {
                if n.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            rows.push(i);
            if n.expanded == Some(false) {
                hidden_below = Some(n.depth);
            }
        }
    }

    fn material(r: f32, g: f32, b: f32, roughness: f32) -> Material {
        Material {
            base_color: Vec3::new(r, g, b),
            metallic: 0.0,
            roughness,
        }
    }

    /// The demo scene's objects: transform and material.
    fn demo_objects() -> Vec<(Mat4, Material)> {
        let mut objects = vec![(
            Mat4::from_scale_rotation_translation(
                Vec3::new(24.0, 0.2, 24.0),
                Quat::IDENTITY,
                Vec3::new(0.0, -0.1, 0.0),
            ),
            material(0.42, 0.44, 0.40, 0.9),
        )];
        for i in 0..9 {
            let (x, z) = ((i % 3) as f32 * 1.6 - 5.0, (i / 3) as f32 * 1.6 - 3.0);
            objects.push((
                Mat4::from_scale_rotation_translation(
                    Vec3::splat(1.1),
                    Quat::from_rotation_y(i as f32 * 0.4),
                    Vec3::new(x, 0.55 + (i % 2) as f32 * 1.1, z),
                ),
                material(0.62, 0.44, 0.26, 0.7),
            ));
        }
        objects.push((
            Mat4::from_scale_rotation_translation(
                Vec3::new(2.0, 6.0, 2.0),
                Quat::IDENTITY,
                Vec3::new(4.0, 3.0, -4.0),
            ),
            material(0.55, 0.57, 0.62, 0.5),
        ));
        objects.push((
            Mat4::from_scale_rotation_translation(
                Vec3::new(0.7, 1.8, 0.7),
                Quat::from_rotation_y(0.8),
                Vec3::new(0.5, 0.9, 2.0),
            ),
            Material {
                base_color: Vec3::new(0.2, 0.45, 0.9),
                metallic: 0.3,
                roughness: 0.35,
            },
        ));
        objects
    }

    /// Frame times of the last reporting period.
    #[derive(Default)]
    struct Stats {
        frames: u32,
        ui: Duration,
        paint: Duration,
        record: Duration,
        since: Option<Instant>,
    }

    pub(crate) fn run() -> Result<(), Error> {
        let started = Instant::now();
        let options = Options::parse();
        // The editor keeps Windows 11's power throttling (team decision): in the
        // background it runs slower (lower clocks, a 15.6 ms timer; 234 -> 66 FPS on
        // ref-1) and saves power. Measurements that cannot bring the window to the front
        // turn it off with --no-throttle. The game runtime always turns it off (§5).
        if options.no_throttle {
            engine_platform::process::disable_power_throttling()?;
        }

        let mut windows = WindowSystem::start(4096)?;
        let window = windows.create_window(&WindowDesc {
            title: String::from("engine editor"),
            width: 1600,
            height: 900,
            ..WindowDesc::default()
        })?;
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine editor"),
            validation: options.validate,
            sync_validation: options.validate,
            ..InstanceDesc::default()
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        let adapter_name = device.adapter().name().to_owned();
        let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd)?;
        if !surface.supports_queue(&device, device.graphics)? {
            return Err("the graphics queue cannot present to the window".into());
        }
        let mut size = (1600u32, 900u32);
        let mut swapchain = Swapchain::new(
            &device,
            &surface,
            &SwapchainDesc {
                width: size.0,
                height: size.1,
                present_mode: options.present,
                srgb: true,
                // Lets `engine_latency` use Reflex on NVIDIA.
                low_latency: true,
            },
        )?;
        let device_ready = started.elapsed();

        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device)?;
        let forward = ForwardRenderer::new(
            &device,
            &bindless,
            HDR_FORMAT,
            DEPTH_FORMAT,
            ShadowSettings::default(),
        )?;
        let tonemap = Tonemap::new(&device, &bindless, VIEW_FORMAT)?;
        let mut renderer2d = Renderer2d::new(
            &device,
            &mut allocator,
            &bindless,
            swapchain.format(),
            QuadMode::for_device(&device),
        )?;
        let mut mesh = Mesh::new(&device, &mut allocator, &cube())?;
        let mut ring = UploadRing::new(&device, &mut allocator, IN_FLIGHT, 16 << 20)?;
        let mut frames = Frames::new(&device, device.graphics.family, IN_FLIGHT)?;
        let mut monitor_hz = refresh_rate(window.hwnd);
        let latency_settings = |monitor_hz: Option<u32>| LatencySettings {
            mode: LowLatency::On,
            fps_limit: options.fps.limit(monitor_hz),
            max_queued_presents: 1,
        };
        let mut latency = Latency::new(&device, &swapchain, latency_settings(monitor_hz))?;
        let mut graph = RenderGraph::new(&device, IN_FLIGHT);
        let mut viewports = Viewports::new(&device, IN_FLIGHT);
        let pipelines_ready = started.elapsed();

        let palette = Palette::default();
        let mut ui = Ui::new(Theme {
            text: palette.text,
            panel: palette.dock,
            ..Theme::default()
        });
        let mut text = UiText::new()?;
        let mut glyphs = GlyphTexture::new(&device, &mut allocator, &bindless, &text.atlas)?;
        let mut canvas = Canvas::new();
        let ui_ready = started.elapsed();

        // Scene and panels (demo data).
        let objects = demo_objects();
        let mut draws: Vec<DrawItem<'_>> = objects
            .iter()
            .map(|(world_from_object, material)| DrawItem {
                mesh: &mesh,
                world_from_object: *world_from_object,
                material: *material,
            })
            .collect();
        let light = DirectionalLight {
            direction: Vec3::new(-0.5, -1.0, -0.35),
            color: Vec3::splat(2.6),
            ambient: Vec3::new(0.12, 0.13, 0.16),
        };
        let mut camera = OrbitCamera {
            target: Vec3::new(0.0, 1.0, 0.0),
            ..OrbitCamera::default()
        };
        let view = viewports.create(ViewportDesc {
            name: String::from("scene"),
            size: ViewportSize::Fixed {
                width: 1,
                height: 1,
            },
            format: VIEW_FORMAT,
            update: UpdateMode::Always,
            placement: None,
            tonemap: false,
            clear: Some([0.16, 0.18, 0.22, 1.0]),
            ..ViewportDesc::default()
        });
        let mut tree = demo_tree();
        let mut rows: Vec<usize> = Vec::with_capacity(tree.len());
        let mut tree_items: Vec<TreeItem<'static>> = Vec::with_capacity(tree.len());
        let mut selected = 8usize;
        let transform = [
            Field {
                name: "Position",
                value: "0.50   0.90   2.00",
            },
            Field {
                name: "Rotation",
                value: "0°   45°   0°",
            },
            Field {
                name: "Scale",
                value: "1   1   1",
            },
        ];
        let body = [
            Field {
                name: "Speed",
                value: "6.5 m/s",
            },
            Field {
                name: "Mass",
                value: "72 kg",
            },
        ];
        let inspector = [
            Section {
                title: "Transform",
                fields: &transform,
            },
            Section {
                title: "Character Body",
                fields: &body,
            },
        ];
        let folders = [
            TreeItem {
                depth: 0,
                icon: icons::FOLDER_OPEN,
                name: "res://",
                kind: "",
                expanded: Some(true),
                selected: false,
            },
            TreeItem {
                depth: 1,
                icon: icons::FOLDER_OPEN,
                name: "levels",
                kind: "",
                expanded: None,
                selected: true,
            },
            TreeItem {
                depth: 1,
                icon: icons::FOLDER,
                name: "scripts",
                kind: "",
                expanded: None,
                selected: false,
            },
        ];
        let files = [
            FileItem {
                name: "main.scene",
                kind: FileKind::Scene,
            },
            FileItem {
                name: "level_01.scene",
                kind: FileKind::Scene,
            },
            FileItem {
                name: "player.luau",
                kind: FileKind::Script,
            },
            FileItem {
                name: "crate.gltf",
                kind: FileKind::Mesh,
            },
            FileItem {
                name: "wind.ogg",
                kind: FileKind::Audio,
            },
        ];
        let tools = [
            format!("{} Select", icons::TOOL_SELECT),
            format!("{} Move", icons::TOOL_MOVE),
            format!("{} Rotate", icons::TOOL_ROTATE),
            format!("{} Scale", icons::TOOL_SCALE),
        ];
        let tools = tools.each_ref().map(String::as_str);
        let (mut active_scene, mut active_tool, mut playing) = (0usize, 1usize, false);
        let mut status = String::with_capacity(160);
        let _ = write!(status, "engine 0.0.0  ·  Vulkan  ·  {adapter_name}");

        let mut input = InputCollector::new();
        let idle = HighResTimer::new()?;
        let (mut scale, mut minimized, mut recreate) = (1.0f32, false, false);
        let mut camera_drag = false;
        let mut view_size = (0u32, 0u32);
        let mut stats = Stats::default();
        let mut first_frame: Option<Duration> = None;
        let mut total_frames = 0u64;

        'main: loop {
            engine_profiler::zone!("editor::frame");
            // The limiter and latency control sleep here, before input is read. While
            // minimized nothing is presented: only events are watched.
            if !minimized {
                latency.begin_frame(&swapchain, &frames)?;
            }
            let mut close = false;
            windows.drain_events(|event| match event.kind {
                WindowEventKind::CloseRequested => close = true,
                WindowEventKind::Resized { width, height } => {
                    if width > 0 && height > 0 {
                        size = (width, height);
                        recreate = true;
                    }
                }
                WindowEventKind::ScaleChanged { dpi } => scale = dpi as f32 / 96.0,
                WindowEventKind::Minimized => minimized = true,
                WindowEventKind::Restored => minimized = false,
                WindowEventKind::Focused(false) => input.focus_lost(),
                WindowEventKind::CursorMoved { x, y } => input.cursor_moved(x as f32, y as f32),
                WindowEventKind::CursorLeft => input.cursor_left(),
                WindowEventKind::MouseButton { button, pressed } => {
                    let button = match button {
                        MouseButton::Left => Some(Button::Left),
                        MouseButton::Right => Some(Button::Right),
                        MouseButton::Middle => Some(Button::Middle),
                        _ => None,
                    };
                    if let Some(button) = button {
                        input.button(button, pressed);
                    }
                }
                WindowEventKind::Wheel { y, .. } => input.wheel(y),
                WindowEventKind::Key {
                    virtual_key,
                    pressed,
                    ..
                } => input.key(virtual_key, pressed),
                _ => {}
            });
            if close {
                break 'main;
            }
            if minimized {
                idle.sleep(Duration::from_millis(10))?;
                continue;
            }
            latency.mark(&swapchain, Marker::InputSample);
            if recreate {
                frames.wait_idle()?;
                swapchain.recreate(&device, &surface, size.0, size.1)?;
                // Reflex settings belong to the swapchain.
                latency.apply(&swapchain)?;
                recreate = false;
            }
            let extent = swapchain.extent();

            // The UI, in logical pixels.
            latency.mark(&swapchain, Marker::SimulationStart);
            let ui_start = Instant::now();
            visible_rows(&tree, &mut rows);
            tree_items.clear();
            tree_items.extend(rows.iter().map(|&i| {
                let n = &tree[i];
                TreeItem {
                    depth: n.depth,
                    icon: n.icon,
                    name: n.name,
                    kind: n.kind,
                    expanded: n.expanded,
                    selected: i == selected,
                }
            }));
            let model = ShellModel {
                scenes: &["main.scene", "level_01.scene*", "меню.scene"],
                active_scene,
                tools: &tools,
                active_tool,
                tree: &tree_items,
                selection: tree[selected].name,
                inspector: &inspector,
                path: "res://levels",
                folders: &folders,
                files: &files,
                bottom_tabs: &[
                    "Output",
                    "Debugger",
                    "Audio",
                    "Animation",
                    "Shader",
                    "Profiler",
                ],
                active_bottom: None,
                playing,
                status: &status,
            };
            let logical = [extent.width as f32 / scale, extent.height as f32 / scale];
            ui.begin_frame(input.ui_input(scale), logical);
            let response = build(&mut ui, &model, &palette);
            ui.end_frame(&mut text);
            stats.ui += ui_start.elapsed();

            // What the UI reported.
            if let Some(i) = response.scene_tab {
                active_scene = i;
            }
            if let Some(i) = response.tool {
                active_tool = i;
            }
            if let Some(&i) = response.tree_row.and_then(|row| rows.get(row)) {
                // A click on the selected row toggles it open or closed.
                if i == selected
                    && let Some(expanded) = tree[i].expanded.as_mut()
                {
                    *expanded = !*expanded;
                }
                selected = i;
            }
            if response.play {
                playing = true;
            }
            if response.stop {
                playing = false;
            }

            // The viewport's camera: drags that start in the viewport, the wheel over it.
            let view_rect = ui
                .rect_of(response.viewport)
                .map(|r| [r.pos[0], r.pos[1], r.size[0], r.size[1]])
                .unwrap_or([0.0, 0.0, 1.0, 1.0]);
            let over_view = input.cursor().is_some_and(|[x, y]| {
                let (x, y) = (x / scale, y / scale);
                x >= view_rect[0]
                    && y >= view_rect[1]
                    && x < view_rect[0] + view_rect[2]
                    && y < view_rect[1] + view_rect[3]
            });
            if (input.pressed(Button::Right) || input.pressed(Button::Middle)) && over_view {
                camera_drag = true;
            }
            if !input.held(Button::Right) && !input.held(Button::Middle) {
                camera_drag = false;
            }
            if camera_drag {
                let [dx, dy] = input.drag();
                if input.held(Button::Middle) {
                    if input.shift() {
                        camera.pan(dx, dy, view_rect[3] * scale);
                    } else {
                        camera.orbit(dx, dy);
                    }
                } else {
                    camera.look(dx, dy);
                }
            }
            if over_view {
                camera.zoom(input.wheel_notches());
            }
            input.next_frame();
            latency.mark(&swapchain, Marker::SimulationEnd);

            // The viewport follows its rectangle, in physical pixels.
            let wanted = (
                ((view_rect[2] * scale).round() as u32).max(1),
                ((view_rect[3] * scale).round() as u32).max(1),
            );
            if wanted != view_size {
                view_size = wanted;
                if let Some(desc) = viewports.desc_mut(view) {
                    desc.size = ViewportSize::Fixed {
                        width: wanted.0,
                        height: wanted.1,
                    };
                }
            }
            let render_camera = Camera {
                clip_from_world: camera
                    .clip_from_world(view_size.0 as f32 / view_size.1.max(1) as f32),
                position: camera.eye(),
            };

            let frame = frames.begin()?;
            bindless.collect(frame.number.saturating_sub(IN_FLIGHT as u64));
            let acquired = match swapchain.acquire(frame.image_available, u64::MAX) {
                Ok(acquired) => acquired,
                Err(RhiError::Vulkan {
                    result: vk::Result::ERROR_OUT_OF_DATE_KHR,
                    ..
                }) => {
                    recreate = true;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let index = acquired.index as usize;

            latency.mark(&swapchain, Marker::RenderSubmitStart);
            let record_start = Instant::now();
            {
                let mut fg = graph.begin(&frame);
                ring.begin(&frame);
                {
                    let (forward, tonemap, bindless, draws) =
                        (&forward, &tonemap, &bindless, &draws);
                    let ring = &mut ring;
                    viewports.render(
                        &mut fg,
                        &device,
                        &mut allocator,
                        bindless,
                        (extent.width, extent.height),
                        frame.number,
                        |fg, vf| {
                            let depth = fg.create_texture(
                                "viewport depth",
                                TextureDesc::new_2d(DEPTH_FORMAT, vf.width, vf.height),
                            );
                            let hdr = fg.create_texture(
                                "viewport hdr",
                                TextureDesc::new_2d(HDR_FORMAT, vf.width, vf.height),
                            );
                            let clear = match vf.load {
                                LoadOp::Clear(c) => Some(c),
                                _ => None,
                            };
                            forward.add_passes(
                                fg,
                                ring,
                                bindless,
                                draws,
                                &ForwardView {
                                    camera: render_camera,
                                    light,
                                    color: hdr,
                                    depth,
                                    clear,
                                    occlusion: None,
                                },
                            );
                            tonemap.add_pass(fg, bindless, hdr, vf.color, 1.0);
                        },
                    )?;
                }

                // The 3D view under the UI, then the UI.
                let paint_start = Instant::now();
                canvas.clear();
                if let Some(texture) = viewports.texture_index(view) {
                    let size = Vec2::new(view_rect[2], view_rect[3]) * scale;
                    let center = Vec2::new(view_rect[0], view_rect[1]) * scale + size * 0.5;
                    canvas.draw(&Quad::sprite(texture, center, size).order(Order::layer(-1)));
                }
                paint(&ui.last_frame(), &mut text, &glyphs, &mut canvas, scale);
                glyphs.sync(&mut fg, &mut ring, &mut text.atlas)?;
                canvas.prepare();
                stats.paint += paint_start.elapsed();

                let backbuffer = fg.import_texture(
                    "backbuffer",
                    ImportedTexture {
                        image: swapchain.images()[index],
                        view: swapchain.views()[index],
                        desc: TextureDesc::new_2d(swapchain.format(), extent.width, extent.height),
                        initial: TextureState::acquired_swapchain_image(),
                        final_state: Some(TextureState::present()),
                        sampled_index: None,
                        storage_index: None,
                    },
                );
                renderer2d.add_pass(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &canvas,
                    &View2d::pixels(extent.width, extent.height),
                    backbuffer,
                    Some(palette.window),
                )?;
                fg.execute_with_bindless(&mut allocator, Some(&bindless))?;
            }
            stats.record += record_start.elapsed();

            frames.submit_tagged(
                device.graphics,
                frame,
                true,
                Some(swapchain.render_finished(acquired.index)),
                latency.submit_present_id(),
            )?;
            latency.mark(&swapchain, Marker::RenderSubmitEnd);
            latency.mark(&swapchain, Marker::PresentStart);
            latency.before_present();
            let presented =
                swapchain.present_with_id(device.graphics, acquired.index, latency.present_id());
            latency.mark(&swapchain, Marker::PresentEnd);
            match presented {
                Ok(suboptimal) => recreate |= suboptimal || acquired.suboptimal,
                Err(RhiError::Vulkan {
                    result: vk::Result::ERROR_OUT_OF_DATE_KHR,
                    ..
                }) => recreate = true,
                Err(e) => return Err(e.into()),
            }
            engine_profiler::frame_mark();
            total_frames += 1;
            if first_frame.is_none() {
                let at = started.elapsed();
                first_frame = Some(at);
                println!(
                    "cold start: first frame presented after {:.0} ms (window + device {:.0} ms, pipelines {:.0} ms, fonts + UI {:.0} ms)",
                    at.as_secs_f64() * 1e3,
                    device_ready.as_secs_f64() * 1e3,
                    (pipelines_ready - device_ready).as_secs_f64() * 1e3,
                    (ui_ready - pipelines_ready).as_secs_f64() * 1e3,
                );
                println!(
                    "frame pacing: {:?}, cap {} (monitor {})",
                    latency.backend(),
                    latency
                        .settings()
                        .fps_limit
                        .map_or(String::from("none"), |fps| format!("{fps:.0} FPS")),
                    monitor_hz.map_or(String::from("unknown"), |hz| format!("{hz} Hz")),
                );
            }

            // Frame statistics in the status bar, twice a second (no allocation: the
            // string keeps its capacity).
            stats.frames += 1;
            let since = *stats.since.get_or_insert_with(Instant::now);
            let elapsed = since.elapsed();
            if elapsed >= Duration::from_millis(500) {
                let n = f64::from(stats.frames);
                let per = |d: Duration| d.as_secs_f64() * 1e6 / n;
                status.clear();
                let _ = write!(
                    status,
                    "{:.0} FPS  ·  UI {:.0} µs  ·  paint {:.0} µs  ·  record {:.0} µs  ·  {}×{}  ·  {adapter_name}",
                    n / elapsed.as_secs_f64(),
                    per(stats.ui),
                    per(stats.paint),
                    per(stats.record),
                    extent.width,
                    extent.height,
                );
                if options.frames.is_some() {
                    println!(
                        "{status}  ·  monitor {:?} Hz, cap {:?}",
                        monitor_hz,
                        latency.settings().fps_limit.map(|f| f.round())
                    );
                }
                stats = Stats::default();
                // The window may have moved to a monitor with another refresh rate.
                let hz = refresh_rate(window.hwnd);
                if hz != monitor_hz {
                    monitor_hz = hz;
                    latency.set_settings(&swapchain, latency_settings(monitor_hz))?;
                }
            }
            if options.frames.is_some_and(|n| total_frames >= n) {
                break 'main;
            }
        }

        frames.wait_idle()?;
        let present_mode = swapchain.present_mode();
        draws.clear();
        // SAFETY: the device is idle.
        unsafe {
            viewports.destroy_all(&mut allocator, &bindless);
            graph.destroy(&mut allocator, Some(&bindless));
            ring.destroy(&mut allocator);
            mesh.destroy(&mut allocator);
            renderer2d.destroy(&mut allocator);
            glyphs
                .texture_mut()
                .destroy(&device, &mut allocator, &bindless, 0);
        }
        drop(frames);
        drop(forward);
        drop(tonemap);
        drop(swapchain);
        drop(surface);
        windows.destroy_window(window.id)?;
        let report = instance.validation_report();
        let running = started.elapsed() - first_frame.unwrap_or_default();
        println!(
            "{total_frames} frames in {:.2} s ({:.0} FPS after the first); present mode {:?} (asked for {:?}); validation errors: {}",
            running.as_secs_f64(),
            total_frames.saturating_sub(1) as f64 / running.as_secs_f64().max(1e-9),
            present_mode,
            options.present,
            report.errors
        );
        if report.errors > 0 {
            return Err("validation errors".into());
        }
        Ok(())
    }
}
