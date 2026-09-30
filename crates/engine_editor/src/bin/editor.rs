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
//! - HDR (ADR 0038): while Windows runs the display in HDR, the editor presents in scRGB
//!   (`--hdr hdr10` for HDR10, `--hdr off` to stay in SDR), with the UI at Windows' SDR
//!   white level and the 3D view rolled off towards the display's peak. It follows HDR
//!   being turned on or off, the SDR brightness slider and moves to other monitors while
//!   running.

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
    use engine_platform::display::DisplayInfo;
    use engine_platform::time::HighResTimer;
    use engine_platform::window::{
        MouseButton, WindowDesc, WindowEvent, WindowEventKind, WindowSystem,
    };
    use engine_render::forward::{
        Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
    };
    use engine_render::graph::{ImportedTexture, LoadOp, RenderGraph, TextureDesc, TextureState};
    use engine_render::material::{MaterialDesc, MaterialTable};
    use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
    use engine_render::post::{Composite, HDR_FORMAT, OutputEncoding, OutputLevels, Tonemap};
    use engine_render::texture_pool::TexturePool;
    use engine_render::upload::UploadRing;
    use engine_render::viewport::{UpdateMode, ViewportDesc, ViewportSize, Viewports};
    use engine_render2d::{Canvas, GlyphTexture, Order, Quad, QuadMode, Renderer2d, View2d};
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, HdrMetadata, Instance,
        InstanceDesc, OutputColor, PresentMode, RhiError, SamplerKind, Surface, Swapchain,
        SwapchainDesc, creation_stats,
    };
    use engine_ui::{Theme, Ui};
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    type Error = Box<dyn std::error::Error>;

    /// Frames the CPU may record ahead of the GPU.
    const IN_FLIGHT: usize = 2;
    /// The viewport's texture format in SDR: sRGB, written by the tone map. HDR output
    /// keeps the tone-mapped view in `HDR_FORMAT`.
    const VIEW_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
    /// Peak luminance assumed for an HDR display that reports none: DisplayHDR 400, the
    /// lowest certified tier (a low guess compresses highlights more; a high one clips).
    const FALLBACK_PEAK_NITS: f32 = 400.0;
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
        /// Output while the display runs in HDR; `Sdr` stays in SDR.
        hdr: OutputColor,
    }

    impl Options {
        fn parse() -> Self {
            let mut options = Self {
                frames: None,
                present: PresentMode::Fifo,
                fps: FpsCap::Monitor,
                validate: cfg!(debug_assertions),
                no_throttle: false,
                hdr: OutputColor::ScRgb,
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
                    "--hdr" => {
                        options.hdr = match args.next().as_deref() {
                            Some("off") => OutputColor::Sdr,
                            Some("hdr10") => OutputColor::Hdr10,
                            _ => OutputColor::ScRgb,
                        }
                    }
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

    fn material(r: f32, g: f32, b: f32, roughness: f32) -> MaterialDesc {
        MaterialDesc::solid([r, g, b], 0.0, roughness)
    }

    /// The demo scene's objects: transform and material.
    fn demo_objects() -> Vec<(Mat4, MaterialDesc)> {
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
            MaterialDesc::solid([0.2, 0.45, 0.9], 0.3, 0.35),
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
        /// GPU time of whole frames (every pass), in microseconds, and how many frames
        /// had timings.
        gpu_us: f64,
        gpu_frames: u32,
        since: Option<Instant>,
    }

    /// What the main loop keeps from window events.
    struct WindowState {
        /// Client size in pixels.
        size: (u32, u32),
        /// UI scale (DPI / 96).
        scale: f32,
        minimized: bool,
        close: bool,
        /// The size changed since the swapchain was made.
        resized: bool,
        /// The monitor showing the window, once the window thread reported it.
        display: Option<DisplayInfo>,
        display_changed: bool,
    }

    impl WindowState {
        fn new(size: (u32, u32)) -> Self {
            Self {
                size,
                scale: 1.0,
                minimized: false,
                close: false,
                resized: false,
                display: None,
                display_changed: false,
            }
        }

        fn handle(&mut self, event: WindowEvent, input: &mut InputCollector) {
            match event.kind {
                WindowEventKind::CloseRequested => self.close = true,
                WindowEventKind::Resized { width, height } => {
                    if width > 0 && height > 0 {
                        self.size = (width, height);
                        self.resized = true;
                    }
                }
                WindowEventKind::ScaleChanged { dpi } => self.scale = dpi as f32 / 96.0,
                WindowEventKind::Minimized => self.minimized = true,
                WindowEventKind::Restored => self.minimized = false,
                WindowEventKind::Display(info) => {
                    self.display = Some(info);
                    self.display_changed = true;
                }
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
            }
        }
    }

    /// The output for `display`: `choice` (an HDR encoding, or SDR to stay in SDR) while
    /// the display runs in HDR, at its SDR white level and peak; SDR otherwise.
    fn wanted_levels(choice: OutputColor, display: Option<&DisplayInfo>) -> OutputLevels {
        match display {
            Some(d) if d.hdr_enabled && choice.is_hdr() => OutputLevels {
                color: choice,
                white_nits: d.sdr_white_nits,
                peak_nits: if d.max_nits > 0.0 {
                    d.max_nits
                } else {
                    FALLBACK_PEAK_NITS
                },
            },
            _ => OutputLevels::SDR,
        }
    }

    /// What `swapchain` delivers of `wanted`: SDR when it fell back to SDR.
    fn actual_levels(swapchain: &Swapchain, wanted: OutputLevels) -> OutputLevels {
        if swapchain.color().is_hdr() {
            OutputLevels {
                color: swapchain.color(),
                ..wanted
            }
        } else {
            OutputLevels::SDR
        }
    }

    /// The display's range as HDR metadata: the output is tone mapped for it already.
    fn hdr_metadata(levels: &OutputLevels, display: Option<&DisplayInfo>) -> HdrMetadata {
        let full_frame = display.map_or(0.0, |d| d.max_full_frame_nits);
        HdrMetadata {
            max_nits: levels.peak_nits,
            min_nits: display.map_or(0.0, |d| d.min_nits),
            max_content_nits: levels.peak_nits,
            max_frame_average_nits: if full_frame > 0.0 {
                full_frame.min(levels.peak_nits)
            } else {
                levels.peak_nits
            },
        }
    }

    /// Appends a short description of `levels` ("SDR", "HDR scRGB, white 80 nits, peak
    /// 344 nits").
    fn describe(levels: &OutputLevels, out: &mut String) {
        let name = match levels.color {
            OutputColor::Sdr => {
                out.push_str("SDR");
                return;
            }
            OutputColor::ScRgb => "HDR scRGB",
            OutputColor::Hdr10 => "HDR10",
        };
        let _ = write!(
            out,
            "{name}, white {:.0} nits, peak {:.0} nits",
            levels.white_nits, levels.peak_nits
        );
    }

    /// Everything that depends on how the swapchain reaches the display (ADR 0038):
    /// rebuilt when its format changes (HDR on or off), adjusted when only the levels
    /// change (the SDR brightness slider).
    struct OutputPipelines {
        levels: OutputLevels,
        /// The swapchain format the pipelines were made for.
        swapchain_format: vk::Format,
        /// The 3D view's texture format: sRGB in SDR, `HDR_FORMAT` in HDR.
        view_format: vk::Format,
        /// Draws the UI into the swapchain, or into an `HDR_FORMAT` image for HDR10.
        renderer2d: Renderer2d,
        /// Tone maps the 3D view into its texture.
        view_tonemap: Tonemap,
        /// HDR10: encodes the composed image into the swapchain.
        encode: Option<Tonemap>,
    }

    impl OutputPipelines {
        fn new(
            device: &Device,
            allocator: &mut Allocator,
            bindless: &Bindless,
            swapchain: &Swapchain,
            levels: OutputLevels,
        ) -> Result<Self, Error> {
            let hdr10 = swapchain.color() == OutputColor::Hdr10;
            let view_format = if swapchain.color().is_hdr() {
                HDR_FORMAT
            } else {
                VIEW_FORMAT
            };
            let ui_format = if hdr10 {
                HDR_FORMAT
            } else {
                swapchain.format()
            };
            let renderer2d = Renderer2d::new(
                device,
                allocator,
                bindless,
                ui_format,
                QuadMode::for_device(device),
            )?;
            let view_tonemap = Tonemap::new(device, bindless, view_format)?;
            let encode = if hdr10 {
                Some(Tonemap::new(device, bindless, swapchain.format())?)
            } else {
                None
            };
            let mut output = Self {
                levels,
                swapchain_format: swapchain.format(),
                view_format,
                renderer2d,
                view_tonemap,
                encode,
            };
            output.set_levels(levels);
            Ok(output)
        }

        /// Whether the pipelines were made for `swapchain`'s format.
        fn fits(&self, swapchain: &Swapchain) -> bool {
            self.swapchain_format == swapchain.format()
        }

        fn set_levels(&mut self, levels: OutputLevels) {
            self.levels = levels;
            // scRGB: the UI writes the swapchain, SDR white at Windows' SDR white level.
            // HDR10: the UI is composed in units of reference white; the encode pass
            // converts.
            self.renderer2d
                .set_white(match levels.swapchain_encoding() {
                    OutputEncoding::Linear { white } => white,
                    OutputEncoding::Pq { .. } => 1.0,
                });
        }

        /// # Safety
        ///
        /// The GPU no longer uses the pipelines.
        unsafe fn destroy(mut self, allocator: &mut Allocator) {
            // SAFETY: guaranteed by the caller.
            unsafe { self.renderer2d.destroy(allocator) };
        }
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
        let mut device = Device::new(&instance, &adapter)?;
        // The pipelines of the last run (ADR 0055): the renderers start in milliseconds
        // instead of compiling for a second.
        let cache_path =
            pipeline_cache_dir().map(|dir| dir.join(device.pipeline_cache_file_name()));
        let cache_load = cache_path
            .as_deref()
            .map(|path| device.load_pipeline_cache(path));
        let device = device;
        let adapter_name = device.adapter().name().to_owned();
        let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd)?;
        if !surface.supports_queue(&device, device.graphics)? {
            return Err("the graphics queue cannot present to the window".into());
        }
        // The window thread reports the display (HDR on or off, SDR white level, refresh
        // rate) right after creating the window. Creating the device took longer, so the
        // first swapchain usually starts in the right mode.
        let mut input = InputCollector::new();
        let mut win = WindowState::new((1600, 900));
        windows.drain_events(|event| win.handle(event, &mut input));
        (win.resized, win.display_changed) = (false, false);
        let mut wanted = wanted_levels(options.hdr, win.display.as_ref());
        let mut swapchain = Swapchain::new(
            &device,
            &surface,
            &SwapchainDesc {
                width: win.size.0,
                height: win.size.1,
                present_mode: options.present,
                srgb: true,
                color: wanted.color,
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
        let mut output = OutputPipelines::new(
            &device,
            &mut allocator,
            &bindless,
            &swapchain,
            actual_levels(&swapchain, wanted),
        )?;
        if output.levels.color.is_hdr() {
            swapchain.set_hdr_metadata(&hdr_metadata(&output.levels, win.display.as_ref()));
        }
        let mut geometry = GeometryPool::new(
            &device,
            &mut allocator,
            GeometryCapacity::default(),
            IN_FLIGHT,
        )?;
        let mesh = geometry.add_mesh(&cube().as_data())?;
        let mut textures = TexturePool::new(&device, 64 << 20, IN_FLIGHT);
        let mut materials = MaterialTable::new(IN_FLIGHT);
        let mut ring = UploadRing::new(&device, &mut allocator, IN_FLIGHT, 16 << 20)?;
        let mut frames = Frames::new(&device, device.graphics.family, IN_FLIGHT)?;
        let mut monitor_hz = win.display.and_then(|d| d.refresh_hz);
        let latency_settings = |monitor_hz: Option<u32>| LatencySettings {
            mode: LowLatency::On,
            fps_limit: options.fps.limit(monitor_hz),
            max_queued_presents: 1,
        };
        let mut latency = Latency::new(&device, &swapchain, latency_settings(monitor_hz))?;
        let mut graph = RenderGraph::new(&device, IN_FLIGHT);
        let mut viewports = Viewports::new(&device, IN_FLIGHT);
        let pipelines_ready = started.elapsed();
        // Saved now as well as at exit: a crash does not cost the next start its cache.
        save_pipeline_cache(&device, cache_path.as_deref());

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
        let mut draws: Vec<DrawItem> = objects
            .iter()
            .map(|(world_from_object, material)| DrawItem {
                mesh,
                material: materials.add(*material),
                world_from_object: *world_from_object,
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
            format: output.view_format,
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
        let mut status = String::with_capacity(256);
        let _ = write!(status, "engine 0.0.0  ·  Vulkan  ·  {adapter_name}");

        let idle = HighResTimer::new()?;
        let mut recreate = false;
        let mut camera_drag = false;
        let mut view_size = (0u32, 0u32);
        let mut stats = Stats::default();
        let mut first_frame: Option<Duration> = None;
        let mut total_frames = 0u64;
        // `--frames` runs report the average GPU time of every pass (after a warm-up).
        let mut pass_totals: Vec<(&'static str, f64, u32)> = Vec::with_capacity(32);

        'main: loop {
            engine_profiler::zone!("editor::frame");
            // The limiter and latency control sleep here, before input is read. While
            // minimized nothing is presented: only events are watched.
            if !win.minimized {
                latency.begin_frame(&swapchain, &frames)?;
            }
            windows.drain_events(|event| win.handle(event, &mut input));
            if win.close {
                break 'main;
            }
            if win.minimized {
                idle.sleep(Duration::from_millis(10))?;
                continue;
            }
            latency.mark(&swapchain, Marker::InputSample);
            recreate |= core::mem::take(&mut win.resized);
            // The display changed: the refresh rate for the frame cap; HDR on or off, the
            // SDR white level or the peak for the output.
            if core::mem::take(&mut win.display_changed) {
                let hz = win.display.and_then(|d| d.refresh_hz);
                if hz != monitor_hz {
                    monitor_hz = hz;
                    latency.set_settings(&swapchain, latency_settings(monitor_hz))?;
                }
                let next = wanted_levels(options.hdr, win.display.as_ref());
                // Also retried while the surface does not offer the HDR output yet (it may
                // only list HDR formats once the display has switched).
                if next.color != wanted.color || next.color != swapchain.color() {
                    recreate = true;
                }
                wanted = next;
            }
            if recreate {
                frames.wait_idle()?;
                swapchain.recreate_with_color(
                    &device,
                    &surface,
                    win.size.0,
                    win.size.1,
                    wanted.color,
                )?;
                // Reflex settings belong to the swapchain.
                latency.apply(&swapchain)?;
                recreate = false;
            }
            // Pipelines follow the swapchain's format, levels the display.
            let levels = actual_levels(&swapchain, wanted);
            let mut changed = first_frame.is_none() || levels != output.levels;
            if !output.fits(&swapchain) {
                frames.wait_idle()?;
                let old = core::mem::replace(
                    &mut output,
                    OutputPipelines::new(&device, &mut allocator, &bindless, &swapchain, levels)?,
                );
                // SAFETY: the device is idle.
                unsafe { old.destroy(&mut allocator) };
                if let Some(desc) = viewports.desc_mut(view) {
                    desc.format = output.view_format;
                }
                changed = true;
            }
            if changed {
                output.set_levels(levels);
                if levels.color.is_hdr() {
                    swapchain.set_hdr_metadata(&hdr_metadata(&levels, win.display.as_ref()));
                }
                let mut line = String::from("output: ");
                describe(&levels, &mut line);
                println!("{line} ({:?})", swapchain.format());
            }
            let extent = swapchain.extent();
            let scale = win.scale;

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
                let geo = geometry.begin_frame(&mut fg, &device, &mut allocator, frame.number)?;
                textures.begin_frame(&mut fg, &mut allocator, &bindless, frame.number)?;
                let mats = materials.begin_frame(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &textures,
                    frame.number,
                )?;
                {
                    let (forward, tonemap, bindless, draws) =
                        (&forward, &output.view_tonemap, &bindless, &draws);
                    // HDR: the view keeps highlights up to the display's peak.
                    let peak = output.levels.peak();
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
                                &geo,
                                &mats,
                                draws,
                                &ForwardView {
                                    camera: render_camera,
                                    light,
                                    color: hdr,
                                    depth,
                                    clear,
                                    occlusion: None,
                                    temporal: None,
                                    lights: &[],
                                    environment: None,
                                },
                            );
                            tonemap.add_composite_pass(
                                fg,
                                bindless,
                                hdr,
                                vf.color,
                                Composite {
                                    peak,
                                    ..Composite::default()
                                },
                            );
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
                // HDR10 composes in linear light first: blending PQ values is not.
                let ui_target = if output.encode.is_some() {
                    fg.create_texture(
                        "ui hdr",
                        TextureDesc::new_2d(HDR_FORMAT, extent.width, extent.height),
                    )
                } else {
                    backbuffer
                };
                output.renderer2d.add_pass(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &canvas,
                    &View2d::pixels(extent.width, extent.height),
                    ui_target,
                    Some(palette.window),
                )?;
                if let Some(encode) = &output.encode {
                    encode.add_composite_pass(
                        &mut fg,
                        &bindless,
                        ui_target,
                        backbuffer,
                        Composite {
                            sampler: SamplerKind::NearestClamp,
                            tonemap: false,
                            encoding: output.levels.swapchain_encoding(),
                            ..Composite::default()
                        },
                    );
                }
                fg.execute_with_bindless(&mut allocator, Some(&bindless))?;
            }
            stats.record += record_start.elapsed();
            let passes = graph.gpu_pass_times();
            if !passes.is_empty() {
                stats.gpu_us += passes.iter().map(|p| f64::from(p.gpu_us)).sum::<f64>();
                stats.gpu_frames += 1;
                if options.frames.is_some() && total_frames >= 200 {
                    for (i, pass) in passes.iter().enumerate() {
                        match pass_totals.get_mut(i) {
                            Some(total) if total.0 == pass.name => {
                                total.1 += f64::from(pass.gpu_us);
                                total.2 += 1;
                            }
                            // The passes changed (HDR on or off): start over.
                            _ => {
                                pass_totals.truncate(i);
                                pass_totals.push((pass.name, f64::from(pass.gpu_us), 1));
                            }
                        }
                    }
                }
            }

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
                let stats = creation_stats();
                println!(
                    "pipelines: {} created, {} from the cache, {} compiled; cache file: {}",
                    stats.created,
                    stats.cache_hits,
                    stats.cache_misses,
                    cache_load
                        .as_ref()
                        .map_or(String::from("none (no LOCALAPPDATA)"), |load| format!(
                            "{load:?}"
                        )),
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
                    "{:.0} FPS  ·  UI {:.0} µs  ·  paint {:.0} µs  ·  record {:.0} µs  ·  GPU {:.0} µs  ·  {}×{}  ·  {adapter_name}  ·  ",
                    n / elapsed.as_secs_f64(),
                    per(stats.ui),
                    per(stats.paint),
                    per(stats.record),
                    stats.gpu_us / f64::from(stats.gpu_frames.max(1)),
                    extent.width,
                    extent.height,
                );
                describe(&output.levels, &mut status);
                if options.frames.is_some() {
                    println!(
                        "{status}  ·  monitor {:?} Hz, cap {:?}",
                        monitor_hz,
                        latency.settings().fps_limit.map(|f| f.round())
                    );
                }
                stats = Stats::default();
            }
            if options.frames.is_some_and(|n| total_frames >= n) {
                break 'main;
            }
        }

        frames.wait_idle()?;
        save_pipeline_cache(&device, cache_path.as_deref());
        if options.frames.is_some() {
            let mut line = String::from("GPU passes, average:");
            for (name, total, count) in &pass_totals {
                let _ = write!(line, " {name} {:.1} µs,", total / f64::from(*count));
            }
            println!("{}", line.trim_end_matches(','));
        }
        let present_mode = swapchain.present_mode();
        draws.clear();
        // SAFETY: the device is idle.
        unsafe {
            viewports.destroy_all(&mut allocator, &bindless);
            graph.destroy(&mut allocator, Some(&bindless));
            ring.destroy(&mut allocator);
            geometry.destroy(&mut allocator);
            materials.destroy(&mut allocator);
            textures.destroy(&mut allocator, &bindless);
            output.destroy(&mut allocator);
            glyphs
                .texture_mut()
                .destroy(&device, &mut allocator, &bindless, 0);
        }
        drop(frames);
        drop(forward);
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

    /// Where the editor keeps its pipeline caches (one per GPU):
    /// `%LOCALAPPDATA%\ofeliaengine\editor`.
    fn pipeline_cache_dir() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join("ofeliaengine").join("editor"))
    }

    /// Saves the pipeline cache, reporting a failure (the next start compiles again).
    fn save_pipeline_cache(device: &Device, path: Option<&Path>) {
        if let Some(path) = path
            && let Err(e) = device.save_pipeline_cache(path)
        {
            println!("pipeline cache not saved: {e}");
        }
    }
}
