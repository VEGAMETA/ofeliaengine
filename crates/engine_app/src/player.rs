//! The player: a game's process, window, device, presentation, scene and frame loop.

use crate::game::{FrameParts, Game};
use crate::process::ProcessSetup;
use crate::report::{FrameReport, Startup, StartupPipelines};
use crate::settings::{PlayerSettings, Window};
use core::cell::RefCell;
use core::fmt;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat4, Quat, Vec3};
use engine_dlss::{DlssFrameGeneration, Streamline, StreamlineDesc};
use engine_ecs::{GlobalTransform, World};
use engine_frame::{
    Antialiasing, FrameContext, FrameGeneration, FrameRenderer, GeneratedTarget, Placement,
    PrepareContext, Quality, RenderDefaults, SceneDraws, SceneInput, UiCanvas, UiRenderer, View,
    ViewCamera, ViewTarget,
};
use engine_latency::{Latency, LatencySettings, Marker, PacedFrame, PacedPresenter, PresentImage};
use engine_platform::window::{
    WindowDesc, WindowEventKind, WindowHandle, WindowMode, WindowSystem,
};
use engine_render::budget::GpuBudget;
use engine_render::environment::{Environment, Sky};
use engine_render::feedback::{FeedbackSettings, TextureFeedback};
use engine_render::forward::{DirectionalLight, ShadowSettings};
use engine_render::gpu_scene::{GpuScene, InstanceId};
use engine_render::graph::{
    Access, FrameGraph, ImportedTexture, LoadOp, RenderGraph, TextureDesc, TextureState,
};
use engine_render::material::{MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId};
use engine_render::present::PresentTarget;
use engine_render::skinning::Skinning;
use engine_render::streaming::LodView;
use engine_render::texture_pool::{TextureId, TexturePool};
use engine_render::upload::UploadRing;
use engine_render2d::Canvas;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Frames, FullScreenExclusive, Instance,
    InstanceDesc, OutputColor, PresentMode, Surface, Swapchain, SwapchainDesc, VulkanHooks,
};
use engine_runtime::{Camera, CameraView, MeshRenderer, Runtime};
use engine_streaming::{SceneStreamSettings, SceneStreamer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// The depth buffer's format.
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
/// No sun: a world without a `Sun` entity has no sunlight and no ambient term.
const NO_SUN: DirectionalLight = DirectionalLight {
    direction: Vec3::NEG_Y,
    color: Vec3::ZERO,
    ambient: Vec3::ZERO,
};
/// The most UI canvases a frame draws.
pub const MAX_UI_CANVASES: usize = 16;

/// Why the player could not start or run a frame: what was being done, and the error.
#[derive(Debug)]
pub struct AppError {
    /// What was being done.
    pub what: &'static str,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl AppError {
    /// Maps an error: `.map_err(AppError::at("the swapchain"))`.
    pub fn at<E: std::error::Error + Send + Sync + 'static>(
        what: &'static str,
    ) -> impl FnOnce(E) -> Self {
        move |e| Self {
            what,
            source: Box::new(e),
        }
    }

    fn message(what: &'static str, text: String) -> Self {
        Self {
            what,
            source: text.into(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.what, self.source)
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Milliseconds since `start`.
fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

/// Microseconds since `start`.
fn us(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e6
}

/// The UI layer: canvases the game keeps in the player, drawn on every presented frame.
pub struct UiLayer {
    renderer: UiRenderer,
    /// The canvases, in drawing order: at most [`MAX_UI_CANVASES`] are drawn.
    pub canvases: Vec<(Canvas, Placement)>,
}

impl fmt::Debug for UiLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UiLayer")
            .field("canvases", &self.canvases.len())
            .finish_non_exhaustive()
    }
}

/// How the frame reaches the screen, as of the swapchain's last (re)creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// The present mode in use.
    pub present_mode: PresentMode,
    /// The monitor is driven by another GPU.
    pub across_adapters: bool,
    /// The frame is drawn into an image of its own and copied into the swapchain image.
    pub drawn_apart: bool,
    /// The swapchain holds its monitor for itself ([`Window::Exclusive`]).
    pub exclusive: bool,
    /// What the driver answered when it refused the monitor.
    pub exclusive_refused: Option<vk::Result>,
    /// The window's mode.
    pub window: Window,
}

/// What the pack's scene holds, for the game.
#[derive(Debug, Clone, Default)]
pub struct SceneInfo {
    /// The pack's meshes, in the pack's order.
    pub meshes: Vec<MeshId>,
    /// The pack's materials, in the pack's order.
    pub materials: Vec<MaterialId>,
    /// The pack's textures, in the pack's order.
    pub textures: Vec<TextureId>,
    /// The bounds of the pack's instances in the world: min and max corners (zero for an
    /// empty scene).
    pub bounds: (Vec3, Vec3),
}

/// The parts of the player a game sets its world up with, between [`Player::new`] and the
/// first frame (meshes and materials of its own, skinned meshes, entities).
pub struct Setup<'a> {
    /// The device.
    pub device: &'a Device,
    /// GPU memory.
    pub allocator: &'a mut Allocator,
    /// The bindless descriptors.
    pub bindless: &'a Bindless,
    /// Textures.
    pub textures: &'a mut TexturePool,
    /// Meshes.
    pub geometry: &'a mut GeometryPool,
    /// Materials.
    pub materials: &'a mut MaterialTable,
    /// Skinned meshes.
    pub skinning: &'a mut Skinning,
    /// The game's world.
    pub world: &'a mut World,
    /// The job system.
    pub jobs: &'a Arc<ThreadPool>,
    /// What the pack's scene holds.
    pub scene: &'a SceneInfo,
}

/// The game's process, window, device, presentation, scene and frame loop (ADR 0099).
///
/// ```no_run
/// use engine_app::{Game, Player, PlayerSettings};
/// use engine_ecs::World;
/// struct MyGame;
/// impl Game for MyGame {
///     fn tick(&mut self, world: &mut World, dt: f32) { /* the simulation */ }
/// }
/// let mut player = Player::new(&PlayerSettings::default()).unwrap();
/// player.run(&mut MyGame).unwrap();
/// ```
///
/// Its fields drop in this order: the scene's GPU objects (destroyed in `Drop` while the
/// device is idle), the renderer, the presentation, Streamline, the device, the window,
/// the instance.
pub struct Player {
    // The simulation.
    runtime: Runtime<InstanceId>,
    cameras: CameraView,
    scene_info: SceneInfo,
    // GPU objects destroyed in `Drop`.
    ui: Option<UiLayer>,
    view: Option<View>,
    scene: GpuScene,
    skinning: Skinning,
    feedback: Option<TextureFeedback>,
    environment: Environment,
    graph: RenderGraph,
    ring: UploadRing,
    geometry: GeometryPool,
    materials: MaterialTable,
    textures: TexturePool,
    streamer: Option<SceneStreamer>,
    budget: GpuBudget,
    frames: Frames,
    renderer: FrameRenderer,
    bindless: Bindless,
    allocator: Allocator,
    // Presentation.
    paced: Option<PacedPresenter>,
    swapchain: Option<Swapchain>,
    surface: Arc<Surface>,
    latency: Option<Latency>,
    dlss: Option<DlssFrameGeneration>,
    streamline: Option<Streamline>,
    hooks: Option<VulkanHooks>,
    device: Arc<Device>,
    jobs: Arc<ThreadPool>,
    windows: WindowSystem,
    window: WindowHandle,
    instance: Instance,
    _process: Option<ProcessSetup>,
    // State.
    settings: PlayerSettings,
    defaults: RenderDefaults,
    swapchain_format: vk::Format,
    presentation: Presentation,
    cache_path: Option<PathBuf>,
    startup: Startup,
    started: Instant,
    previous_frame: Option<Instant>,
    /// The game time of each frame, for benchmarks ([`Self::set_frame_time`]).
    frame_time: Option<f64>,
    /// The window's client size, as its last events said.
    client: (u32, u32),
    resized: bool,
    minimized: bool,
    closing: bool,
    first_present_ms: Option<f64>,
}

impl fmt::Debug for Player {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Player")
            .field("title", &self.settings.title)
            .field("adapter", &self.device.adapter().summary())
            .field("window", &self.settings.window)
            .finish_non_exhaustive()
    }
}

impl Player {
    /// Starts the game: the process set-up, the window, the device, the swapchain, the
    /// latency provider, the pipeline cache, the renderer with the device class's
    /// defaults applied whole, the pack's scene streamed and its instances as entities.
    ///
    /// What does not depend on each other overlaps: the Vulkan instance and device beside
    /// the window, the swapchain beside the renderer and the scene ([`Self::startup`]
    /// tells the phases).
    ///
    /// # Errors
    ///
    /// When the window, the device, the swapchain, the renderer or the pack cannot be
    /// made.
    #[allow(clippy::too_many_lines)]
    pub fn new(settings: &PlayerSettings) -> Result<Self, AppError> {
        let started = Instant::now();
        let mut startup = Startup::default();
        let process = settings.process_setup.then(ProcessSetup::new);
        if let Some(process) = &process {
            startup.notes.extend(process.notes.iter().cloned());
        }
        let in_flight = settings.frames_in_flight.max(1);
        let dlss_wanted = settings.frame_generation == FrameGeneration::Dlss;
        let warm_up = settings.warm_up;
        let cache_dir = match &settings.pipeline_cache {
            Some(dir) => dir.clone(),
            None => {
                std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join(&settings.title))
            }
        };
        let preference = AdapterPreference::from_env().unwrap_or(settings.adapter.clone());
        let app_name = settings.title.clone();

        // The Vulkan instance and device (the drivers' own start-up) beside the window.
        let gpu = std::thread::Builder::new()
            .name(String::from("vulkan start-up"))
            .spawn(move || {
                start_gpu(
                    &app_name,
                    dlss_wanted,
                    warm_up,
                    cache_dir.as_deref(),
                    &preference,
                )
            })
            .map_err(AppError::at("the device's start-up thread"))?;
        let phase = Instant::now();
        let windows = WindowSystem::start(4096).map_err(AppError::at("the window system"))?;
        let (mode, width, height) = match settings.window {
            Window::Windowed { width, height } => (WindowMode::Windowed, width, height),
            Window::FullScreen | Window::Exclusive => (WindowMode::FullScreen, 1920, 1080),
        };
        let window = windows
            .create_window(&WindowDesc {
                title: settings.title.clone(),
                width,
                height,
                resizable: mode == WindowMode::Windowed,
                visible: true,
                mode,
                monitor: settings.monitor,
            })
            .map_err(AppError::at("the window"))?;
        startup
            .phases
            .push(("window (beside the device)", ms(phase)));
        let phase = Instant::now();
        let jobs = Arc::new(
            ThreadPool::builder()
                .build()
                .map_err(AppError::at("the job system"))?,
        );
        startup.phases.push(("job system", ms(phase)));
        let gpu = gpu
            .join()
            .map_err(|_| AppError::message("the device", String::from("its thread panicked")))??;
        startup.notes.extend(gpu.notes.iter().cloned());
        startup.cache.clone_from(&gpu.cache);
        startup
            .phases
            .push(("Vulkan instance + device (+ pipeline cache file)", gpu.ms));
        let GpuStart {
            instance,
            device,
            cache_path,
            streamline,
            ..
        } = gpu;

        // With DLSS frame generation Streamline presents: the surface and the swapchain go
        // through its hooks.
        let hooks = streamline.as_ref().map(Streamline::vulkan_hooks);
        let surface = Arc::new(
            match &hooks {
                Some(hooks) => {
                    Surface::from_win32_hooked(&instance, window.hinstance, window.hwnd, hooks)
                }
                None => Surface::from_win32(&instance, window.hinstance, window.hwnd),
            }
            .map_err(AppError::at("the window's surface"))?,
        );
        let swapchain_desc = SwapchainDesc {
            present_mode: if settings.vsync {
                PresentMode::Fifo
            } else {
                PresentMode::Immediate
            },
            srgb: true,
            color: OutputColor::Sdr,
            // Reflex's swapchain only when latency control is on: off, it costs nothing.
            low_latency: settings.low_latency != engine_latency::LowLatency::Off,
            full_screen_exclusive: full_screen_choice(settings.window, window.hwnd),
            ..SwapchainDesc::default()
        };
        // The renderer draws into the format the swapchain will have.
        let swapchain_format = Swapchain::format_for(&device, &surface, &swapchain_desc)
            .map_err(AppError::at("the swapchain's format"))?;
        let device = Arc::new(device);
        let pending_swapchain = {
            let (device, surface, hooks) = (Arc::clone(&device), Arc::clone(&surface), hooks);
            std::thread::Builder::new()
                .name(String::from("swapchain"))
                .spawn(move || {
                    let phase = Instant::now();
                    let swapchain = match &hooks {
                        Some(hooks) => {
                            Swapchain::new_hooked(&device, &surface, &swapchain_desc, hooks)
                        }
                        None => Swapchain::new(&device, &surface, &swapchain_desc),
                    };
                    (swapchain, ms(phase))
                })
                .map_err(AppError::at("the swapchain's thread"))?
        };

        // The renderer, with the device class's defaults applied whole.
        let phase = Instant::now();
        let pipelines_before = engine_rhi::creation_stats();
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).map_err(AppError::at("bindless descriptors"))?;
        let mut defaults = settings
            .render
            .unwrap_or_else(|| RenderDefaults::for_device(&device));
        if let Some(antialiasing) = settings.antialiasing {
            defaults.antialiasing = antialiasing;
        }
        if let Some(quality) = settings.quality {
            defaults.quality = quality;
        }
        if let Some(ambient_occlusion) = settings.ambient_occlusion {
            defaults.ambient_occlusion = ambient_occlusion;
        }
        if let Some(meshlet_culling) = settings.meshlet_culling {
            defaults.meshlet_culling = meshlet_culling;
        }
        let mut renderer = FrameRenderer::new(
            &device,
            &bindless,
            swapchain_format,
            DEPTH_FORMAT,
            ShadowSettings::default(),
            in_flight,
        )
        .map_err(AppError::at("the frame renderer"))?;
        renderer
            .forward_mut()
            .set_ambient_occlusion(&device, &bindless, defaults.ambient_occlusion)
            .map_err(AppError::at("ambient occlusion"))?;
        renderer
            .forward_mut()
            .set_meshlet_culling(&device, &bindless, defaults.meshlet_culling)
            .map_err(AppError::at("meshlet culling"))?;
        if let Some(local_shadows) = settings.local_shadows {
            renderer
                .forward_mut()
                .set_local_shadows(&device, &mut allocator, &bindless, Some(local_shadows))
                .map_err(AppError::at("local lights' shadows"))?;
        }
        let skinning = Skinning::new(
            &device,
            &mut allocator,
            &bindless,
            DEPTH_FORMAT,
            1 << 16,
            in_flight,
        )
        .map_err(AppError::at("skinning"))?;
        let feedback = if settings.texture_feedback {
            Some(
                TextureFeedback::new(&device, &bindless, FeedbackSettings::default(), in_flight)
                    .map_err(AppError::at("texture feedback"))?,
            )
        } else {
            None
        };
        let environment = Environment::new(&device, &mut allocator, &bindless, Sky::default())
            .map_err(AppError::at("the environment"))?;
        let pipelines_after = engine_rhi::creation_stats();
        startup.pipelines = StartupPipelines {
            created: pipelines_after.created - pipelines_before.created,
            from_cache: pipelines_after.cache_hits - pipelines_before.cache_hits,
            compiled: pipelines_after.cache_misses - pipelines_before.cache_misses,
            driver_ms: (pipelines_after.time - pipelines_before.time).as_secs_f64() * 1e3,
        };
        startup
            .phases
            .push(("renderer + features (pipelines)", ms(phase)));

        // The scene from its pack, streamed; its instances as entities.
        let phase = Instant::now();
        let mut budget = GpuBudget::new(&device);
        let mut textures = TexturePool::new(&device, 32 << 20, in_flight);
        let mut geometry = GeometryPool::new(
            &device,
            &mut allocator,
            GeometryCapacity::default(),
            in_flight,
        )
        .map_err(AppError::at("the geometry pool"))?;
        let mut materials = MaterialTable::new(in_flight);
        let mut world = World::new();
        let mut scene_info = SceneInfo::default();
        let streamer = match &settings.pack {
            Some(pack) => {
                let streamer = SceneStreamer::open(
                    pack,
                    &jobs,
                    &mut textures,
                    &mut geometry,
                    &mut allocator,
                    &mut budget,
                    SceneStreamSettings::default(),
                )
                .map_err(AppError::at("the scene's pack"))?;
                scene_info = spawn_scene(&streamer, &mut renderer, &mut materials, &mut world);
                Some(streamer)
            }
            None => None,
        };
        startup
            .phases
            .push(("scene opened from its pack (tails resident)", ms(phase)));

        let cameras = CameraView::new(&mut world, defaults.view_settings());
        let mut scene = GpuScene::new(&device, &bindless, world.len() as u32 + 16, in_flight)
            .map_err(AppError::at("the GPU scene"))?;
        scene.set_jobs(jobs.clone());
        let mut runtime = Runtime::<InstanceId>::new(world, settings.tick_hz);
        runtime.set_jobs(jobs.clone());
        let ring = UploadRing::new(&device, &mut allocator, in_flight, 32 << 20)
            .map_err(AppError::at("the upload ring"))?;
        let frames = Frames::new(&device, device.graphics.family, in_flight)
            .map_err(AppError::at("the frame slots"))?;
        let graph = RenderGraph::new(&device, in_flight);

        // The swapchain, made beside all of the above.
        let wait = Instant::now();
        let (swapchain, swapchain_ms) = pending_swapchain
            .join()
            .map_err(|_| AppError::message("the swapchain", String::from("its thread panicked")))?;
        let swapchain = swapchain.map_err(AppError::at("the swapchain"))?;
        startup.phases.push((
            "surface + swapchain (beside the renderer and the scene)",
            swapchain_ms,
        ));
        startup.phases.push(("waiting for the swapchain", ms(wait)));
        if matches!(settings.window, Window::Exclusive)
            && let Some(refused) = swapchain.exclusive_refused()
        {
            startup.notes.push(format!(
                "the driver refused the monitor ({refused:?}): borderless full screen"
            ));
        }

        let mut player = Self {
            runtime,
            cameras,
            scene_info,
            ui: None,
            view: None,
            scene,
            skinning,
            feedback,
            environment,
            graph,
            ring,
            geometry,
            materials,
            textures,
            streamer,
            budget,
            frames,
            renderer,
            bindless,
            allocator,
            paced: None,
            swapchain: Some(swapchain),
            surface,
            latency: None,
            dlss: None,
            streamline,
            hooks,
            device,
            jobs,
            windows,
            window,
            instance,
            _process: process,
            settings: settings.clone(),
            defaults,
            swapchain_format,
            presentation: Presentation {
                present_mode: PresentMode::Fifo,
                across_adapters: false,
                drawn_apart: false,
                exclusive: false,
                exclusive_refused: None,
                window: settings.window,
            },
            cache_path,
            startup,
            started,
            previous_frame: None,
            frame_time: None,
            client: (0, 0),
            resized: false,
            minimized: false,
            closing: false,
            first_present_ms: None,
        };
        player.start_presentation()?;
        // Saved now as well as at exit: a crash does not cost the next start its cache.
        player.save_pipeline_cache();
        Ok(player)
    }

    /// Frame generation and latency control over the swapchain (again after it is made
    /// anew).
    fn start_presentation(&mut self) -> Result<(), AppError> {
        if let Some(swapchain) = &self.swapchain {
            let across_adapters = swapchain.presents_across_adapters();
            self.presentation = Presentation {
                present_mode: swapchain.present_mode(),
                across_adapters,
                drawn_apart: self.settings.frame_generation != FrameGeneration::Off
                    || self.settings.present_apart.unwrap_or(across_adapters),
                exclusive: swapchain.exclusive(),
                exclusive_refused: swapchain.exclusive_refused(),
                window: self.settings.window,
            };
        }
        let latency_settings = LatencySettings {
            mode: self.settings.low_latency,
            fps_limit: self.settings.fps_limit,
            max_queued_presents: 1,
        };
        match self.settings.frame_generation {
            // FSR's: a paced presenter takes the swapchain (ADR 0091).
            FrameGeneration::Fsr => {
                if let Some(swapchain) = self.swapchain.take() {
                    match PacedPresenter::start(&self.device, &mut self.allocator, swapchain, None)
                    {
                        Ok(paced) => {
                            self.latency = Some(
                                Latency::new(&self.device, &paced, latency_settings)
                                    .map_err(AppError::at("latency control"))?,
                            );
                            self.paced = Some(paced);
                        }
                        Err(failed) => {
                            let (swapchain, e) = *failed;
                            self.startup
                                .notes
                                .push(format!("frame generation unavailable: {e}"));
                            self.swapchain = Some(swapchain);
                        }
                    }
                }
            }
            // DLSS's: Streamline presents and its Reflex paces the real frames (§6).
            FrameGeneration::Dlss => {
                if let (Some(streamline), Some(swapchain)) = (&self.streamline, &self.swapchain) {
                    if self.dlss.is_none() {
                        let mut generation = DlssFrameGeneration::new(streamline)
                            .map_err(AppError::at("DLSS frame generation"))?;
                        generation
                            .set_enabled(true)
                            .map_err(AppError::at("DLSS frame generation"))?;
                        self.dlss = Some(generation);
                    }
                    if let Some(generation) = &self.dlss {
                        self.latency = Some(
                            Latency::with_external(
                                &self.device,
                                swapchain,
                                latency_settings,
                                Box::new(generation.latency()),
                            )
                            .map_err(AppError::at("latency control"))?,
                        );
                    }
                } else if self.streamline.is_none() {
                    let log = Streamline::log();
                    self.startup.notes.push(format!(
                        "DLSS frame generation is off ({} errors, {} warnings from Streamline: {:?})",
                        log.errors, log.warnings, log.first_errors
                    ));
                }
            }
            FrameGeneration::Off => {}
        }
        if self.latency.is_none()
            && let Some(swapchain) = &self.swapchain
        {
            self.latency = Some(
                Latency::new(&self.device, swapchain, latency_settings)
                    .map_err(AppError::at("latency control"))?,
            );
        }
        Ok(())
    }

    /// Where the start-up spent its time, and what could not be done.
    pub fn startup(&self) -> &Startup {
        &self.startup
    }

    /// How the frame reaches the screen.
    pub fn presentation(&self) -> Presentation {
        self.presentation
    }

    /// The validation layer's messages so far (debug builds validate).
    pub fn validation_report(&self) -> engine_rhi::ValidationReport {
        self.instance.validation_report()
    }

    /// Milliseconds from [`Self::new`] to the first frame presented.
    pub fn first_present_ms(&self) -> Option<f64> {
        self.first_present_ms
    }

    /// The parts a game sets its world up with, before its first frame.
    pub fn setup(&mut self) -> Setup<'_> {
        Setup {
            device: &self.device,
            allocator: &mut self.allocator,
            bindless: &self.bindless,
            textures: &mut self.textures,
            geometry: &mut self.geometry,
            materials: &mut self.materials,
            skinning: &mut self.skinning,
            world: &mut self.runtime.world,
            jobs: &self.jobs,
            scene: &self.scene_info,
        }
    }

    /// What the pack's scene holds.
    pub fn scene_info(&self) -> &SceneInfo {
        &self.scene_info
    }

    /// The game's world.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.runtime.world
    }

    /// The runtime: the world, the simulation clock, extraction.
    pub fn runtime(&self) -> &Runtime<InstanceId> {
        &self.runtime
    }

    /// The UI layer, made on first use; its canvases are drawn on every presented frame.
    ///
    /// # Errors
    ///
    /// When the UI renderer cannot be made.
    pub fn ui_mut(&mut self) -> Result<&mut UiLayer, AppError> {
        if self.ui.is_none() {
            let renderer = UiRenderer::new(
                &self.device,
                &mut self.allocator,
                &self.bindless,
                self.swapchain_format,
            )
            .map_err(AppError::at("the UI renderer"))?;
            self.ui = Some(UiLayer {
                renderer,
                canvases: Vec::new(),
            });
        }
        Ok(self
            .ui
            .as_mut()
            .unwrap_or_else(|| unreachable!("made above")))
    }

    /// A perspective camera with the player's antialiasing and render size: put it on an
    /// entity with a `Transform` to render through it.
    pub fn camera(&self, fov_y: f32, near: f32, far: f32) -> Camera {
        Camera::perspective(fov_y, near, far)
            .with_antialiasing(self.defaults.antialiasing, self.defaults.quality)
    }

    /// The renderer's features in effect (the device class's defaults, or the settings').
    pub fn defaults(&self) -> &RenderDefaults {
        &self.defaults
    }

    /// The antialiasing in effect.
    pub fn antialiasing(&self) -> Antialiasing {
        self.defaults.antialiasing
    }

    /// The render size in effect.
    pub fn quality(&self) -> Quality {
        self.defaults.quality
    }

    /// The device.
    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }

    /// GPU memory.
    pub fn allocator(&self) -> &Allocator {
        &self.allocator
    }

    /// The frame graph (its statistics, the GPU times of the last frames measured).
    pub fn graph(&self) -> &RenderGraph {
        &self.graph
    }

    /// The frame renderer.
    pub fn renderer(&self) -> &FrameRenderer {
        &self.renderer
    }

    /// The streamed scene, when the player opened a pack.
    pub fn streamer(&self) -> Option<&SceneStreamer> {
        self.streamer.as_ref()
    }

    /// The latency provider.
    pub fn latency(&self) -> Option<&Latency> {
        self.latency.as_ref()
    }

    /// The swapchain, while the frame loop presents to it itself (not through a paced
    /// presenter).
    pub fn swapchain(&self) -> Option<&Swapchain> {
        self.swapchain.as_ref()
    }

    /// The paced presenter (FSR frame generation).
    pub fn paced(&mut self) -> Option<&mut PacedPresenter> {
        self.paced.as_mut()
    }

    /// DLSS frame generation, when it runs.
    pub fn dlss(&self) -> Option<&DlssFrameGeneration> {
        self.dlss.as_ref()
    }

    /// The frame's size: the swapchain's.
    pub fn extent(&self) -> vk::Extent2D {
        match (&self.swapchain, &self.paced) {
            (Some(swapchain), _) => swapchain.extent(),
            (None, Some(paced)) => paced.extent(),
            (None, None) => vk::Extent2D::default(),
        }
    }

    /// The window.
    pub fn window(&self) -> WindowHandle {
        self.window
    }

    /// Whether the window asked to close: [`Self::run`] stops.
    pub fn closing(&self) -> bool {
        self.closing
    }

    /// Runs frames until the window closes.
    ///
    /// # Errors
    ///
    /// As [`Self::frame`].
    pub fn run(&mut self, game: &mut impl Game) -> Result<(), AppError> {
        while !self.closing {
            self.frame(game)?;
        }
        Ok(())
    }

    /// Saves the pipeline cache (also done when the player is dropped).
    pub fn save_pipeline_cache(&mut self) {
        if let Some(path) = &self.cache_path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = self.device.save_pipeline_cache(path) {
                self.startup
                    .notes
                    .push(format!("pipeline cache not saved: {e}"));
            }
        }
    }

    /// One frame: the window's events, latency control, the frame slot and swapchain
    /// image, the game's ticks and presentation, animation, streaming, the frame's
    /// rendering and UI, submission and presentation. `None` when nothing was rendered:
    /// the window is minimized, closing, or the swapchain was made anew.
    ///
    /// # Errors
    ///
    /// When a Vulkan call or the frame's declaration fails.
    #[allow(clippy::too_many_lines)]
    pub fn frame(&mut self, game: &mut impl Game) -> Result<Option<FrameReport>, AppError> {
        engine_profiler::zone!("app::Player::frame");
        let mut report = FrameReport::default();
        // The window's events: the game sees them all; the player keeps the size and
        // whether to close.
        {
            let (closing, resized, minimized, client) = (
                &mut self.closing,
                &mut self.resized,
                &mut self.minimized,
                &mut self.client,
            );
            self.windows.drain_events(|event| {
                match event.kind {
                    WindowEventKind::CloseRequested => *closing = true,
                    WindowEventKind::Resized { width, height } => {
                        *resized = true;
                        *minimized = width == 0 || height == 0;
                        *client = (width, height);
                    }
                    WindowEventKind::Minimized => *minimized = true,
                    _ => {}
                }
                game.event(&event);
            });
        }
        if self.closing {
            return Ok(None);
        }
        if self.minimized {
            // Nothing is presented while minimized: the next events are awaited.
            std::thread::yield_now();
            return Ok(None);
        }
        if core::mem::take(&mut self.resized) && self.needs_new_swapchain() {
            self.recreate_presentation()?;
            return Ok(None);
        }

        // Latency control waits here, before the frame slot and input.
        let t = Instant::now();
        let frame_id = match (&mut self.latency, &self.swapchain, &self.paced) {
            (Some(latency), Some(swapchain), _) => Some(
                latency
                    .begin_frame(swapchain, &self.frames)
                    .map_err(AppError::at("latency control"))?,
            ),
            (Some(latency), None, Some(paced)) => Some(
                latency
                    .begin_frame(paced, &self.frames)
                    .map_err(AppError::at("latency control"))?,
            ),
            _ => None,
        };
        report.latency_wait = us(t);
        let frame_start = Instant::now();
        let elapsed = match self.frame_time {
            Some(fixed) => fixed,
            None => self
                .previous_frame
                .map_or(1.0 / 60.0, |p| (frame_start - p).as_secs_f64()),
        };
        self.previous_frame = Some(frame_start);
        let pipelines_at_start = engine_rhi::creation_stats().created;
        let frame = self
            .frames
            .begin()
            .map_err(AppError::at("the frame slot"))?;
        report.number = frame.number;
        report.wait = us(frame_start);
        let work = Instant::now();
        let t = Instant::now();
        let acquired = match self.swapchain.as_mut() {
            Some(swapchain) => match swapchain.acquire(frame.image_available, u64::MAX) {
                Ok(a) => Some(a),
                // Out of date, or the monitor held for itself lost (ADR 0099).
                Err(e) if e.is_out_of_date() => {
                    // The slot is given back unused.
                    self.frames
                        .submit(self.device.graphics, frame, false, None)
                        .map_err(AppError::at("the frame slot"))?;
                    self.recreate_presentation()?;
                    return Ok(None);
                }
                Err(e) => return Err(AppError::at("the swapchain image")(e)),
            },
            None => None,
        };
        let paced_images = self.paced.as_ref().map(PacedPresenter::next_images);
        report.acquire = us(t);
        let marker = |latency: &mut Option<Latency>,
                      swapchain: &Option<Swapchain>,
                      paced: &Option<PacedPresenter>,
                      marker: Marker| {
            match (latency.as_mut(), swapchain, paced) {
                (Some(l), Some(s), _) => l.mark(s, marker),
                (Some(l), None, Some(p)) => l.mark(p, marker),
                _ => {}
            }
        };
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::InputSample,
        );
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::SimulationStart,
        );

        // Ticks, presentation, interpolation (ADR 0064).
        let stages = {
            let game_cell = RefCell::new(&mut *game);
            self.runtime.frame(
                elapsed,
                &(),
                &mut self.scene,
                |world, (), dt| game_cell.borrow_mut().tick(world, dt),
                |world, (), alpha| game_cell.borrow_mut().present(world, alpha),
            )
        };
        report.ticks = stages.ticks;
        report.simulate = stages.simulate_us;
        report.propagate = stages.propagate_us;
        report.extract = stages.extract_us;
        report.interpolate = stages.interpolate_us + stages.present_us;
        let t = Instant::now();
        game.animate(
            &mut FrameParts {
                runtime: &self.runtime,
                scene: &mut self.scene,
                skinning: &mut self.skinning,
                geometry: &mut self.geometry,
                jobs: &self.jobs,
            },
            elapsed as f32,
        );
        // Skinned meshes at their scene instances, then their bounds (ADR 0099 step 3).
        self.runtime.place_skinned(&self.scene, &mut self.skinning);
        self.skinning
            .update_bounds(&mut self.geometry, Some(&mut self.scene));
        report.animate = us(t);
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::SimulationEnd,
        );

        // The main camera, or one at the origin.
        let (camera, mut view_settings) = match self.cameras.resolve(&self.runtime.world) {
            Some(main) => (main.camera, main.settings),
            None => (
                ViewCamera::from_rigid(Quat::IDENTITY, Vec3::ZERO, 1.2, 0.1, 1000.0),
                *self.cameras.base_mut(),
            ),
        };
        let extent = self.extent();
        let aspect = extent.width as f32 / extent.height.max(1) as f32;
        let clip_from_world = camera.clip_from_world(aspect);

        // Streaming: mesh levels by distance in every direction (a fast turn finds them
        // loaded), textures by what the views sampled.
        let t = Instant::now();
        if let Some(streamer) = &mut self.streamer {
            let eye = camera.position;
            let lod_view = LodView::new(&clip_from_world, eye, extent.height, 1.0).around();
            let before = streamer.stats().arrived;
            streamer.update(
                &mut self.textures,
                &mut self.geometry,
                &mut self.allocator,
                &mut self.budget,
                self.feedback.as_mut(),
                Some(&lod_view),
                &self.materials,
                frame.number,
            );
            report.arrived = streamer.stats().arrived.saturating_sub(before);
        }
        self.budget.update(&self.allocator);
        report.streaming = us(t);

        if self.paced.is_some() {
            view_settings.frame_generation = FrameGeneration::Fsr;
        }
        if self.dlss.is_some() {
            view_settings.frame_generation = FrameGeneration::Dlss;
        }
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::RenderSubmitStart,
        );
        let t = Instant::now();
        // The view, made with room for the scene's slots (and made again when it outgrows
        // them: a cut).
        let needed = self.scene.slot_count().max(16);
        if self.view.as_ref().is_some_and(|v| v.capacity() < needed) {
            self.frames
                .wait_idle()
                .map_err(AppError::at("growing the view"))?;
            if let Some(mut view) = self.view.take() {
                // SAFETY: the device is idle.
                unsafe { view.destroy(&mut self.allocator, &self.bindless) };
            }
        }
        let view = self
            .view
            .get_or_insert_with(|| View::new(&self.device, needed.next_power_of_two()));
        self.renderer
            .prepare(
                view,
                &view_settings,
                (extent.width, extent.height),
                PrepareContext {
                    device: &self.device,
                    allocator: &mut self.allocator,
                    bindless: &self.bindless,
                    materials: &mut self.materials,
                    frame_number: frame.number,
                    skinning: Some(&mut self.skinning),
                },
            )
            .map_err(AppError::at("the view"))?;
        report.compose = us(t);
        let lod_bias = self.materials.texture_lod_bias();
        // The UI layer's canvases, in an array (no allocation).
        let ui_canvases = self.ui.as_ref().and_then(|ui| {
            let (canvas, placement) = ui.canvases.first()?;
            let mut layer = [UiCanvas {
                canvas,
                placement: *placement,
            }; MAX_UI_CANVASES];
            let mut count = 0;
            for (slot, (canvas, placement)) in layer.iter_mut().zip(&ui.canvases) {
                *slot = UiCanvas {
                    canvas,
                    placement: *placement,
                };
                count += 1;
            }
            Some((layer, count))
        });
        let layer: &[UiCanvas<'_>] = match &ui_canvases {
            Some((layer, count)) => &layer[..*count],
            None => &[],
        };
        let generated_shown;
        {
            let t = Instant::now();
            let mut fg = self.graph.begin(&frame);
            self.ring.begin(&frame);
            let p = Instant::now();
            self.textures
                .begin_frame(&mut fg, &mut self.allocator, &self.bindless, frame.number)
                .map_err(AppError::at("the texture pool"))?;
            report.pool_parts[0] = us(p);
            let p = Instant::now();
            let mats = self
                .materials
                .begin_frame(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &self.textures,
                    frame.number,
                )
                .map_err(AppError::at("the material table"))?;
            report.pool_parts[1] = us(p);
            let p = Instant::now();
            let geo = self
                .geometry
                .begin_frame(&mut fg, &self.device, &mut self.allocator, frame.number)
                .map_err(AppError::at("the geometry pool"))?;
            report.pool_parts[2] = us(p);
            let skinned = self
                .skinning
                .begin_frame(
                    &mut fg,
                    &mut self.ring,
                    &self.device,
                    &mut self.allocator,
                    &self.bindless,
                    &geo,
                    frame.number,
                )
                .map_err(AppError::at("skinning"))?;
            report.pools = us(t);
            let t = Instant::now();
            let scene_frame = self
                .scene
                .add_passes(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &mut self.ring,
                    &self.bindless,
                    &geo,
                    &mats,
                    frame.number,
                )
                .map_err(AppError::at("the GPU scene"))?;
            report.upload = us(t);
            let t = Instant::now();
            let import = |fg: &mut FrameGraph<'_>, name: &'static str, image: PresentImage| {
                fg.import_texture(
                    name,
                    ImportedTexture {
                        image: image.image,
                        view: image.view,
                        desc: TextureDesc::new_2d(image.format, extent.width, extent.height),
                        initial: TextureState::undefined(),
                        final_state: Some(TextureState::transfer_source()),
                        sampled_index: None,
                        storage_index: None,
                    },
                )
            };
            let (present, generated) = match (&self.swapchain, paced_images) {
                (Some(swapchain), _) => (
                    // DLSS frame generation: the frame is drawn apart, kept without its UI
                    // for Streamline, and copied to the swapchain image under the UI.
                    Some(PresentTarget::import_with(
                        &mut fg,
                        swapchain,
                        acquired.map_or(0, |a| a.index),
                        self.dlss.is_some()
                            || self
                                .settings
                                .present_apart
                                .unwrap_or_else(|| swapchain.presents_across_adapters()),
                    )),
                    None,
                ),
                (None, Some(images)) => (
                    None,
                    Some((
                        import(&mut fg, "real frame", images.real),
                        GeneratedTarget {
                            texture: import(&mut fg, "generated frame", images.generated),
                            format: images.generated.raw_format,
                            source_format: images.real.raw_format,
                        },
                    )),
                ),
                (None, None) => {
                    return Err(AppError::message(
                        "the frame",
                        String::from("nothing presents"),
                    ));
                }
            };
            let target = match (&present, &generated) {
                (Some(present), _) => present.texture,
                (None, Some((real, _))) => *real,
                (None, None) => unreachable!("one of them presents"),
            };
            let mut context = FrameContext {
                device: &self.device,
                allocator: &mut self.allocator,
                bindless: &self.bindless,
                ring: &mut self.ring,
                frame_number: frame.number,
                dt: elapsed as f32,
            };
            let draws = match &scene_frame {
                Some(scene_frame) => SceneDraws::Scene(scene_frame),
                None => SceneDraws::List(&[]),
            };
            let scene_input = SceneInput {
                geometry: &geo,
                materials: &mats,
                draws,
                skinning: Some(&skinned),
                light: self.runtime.lights().sun().unwrap_or(NO_SUN),
                lights: self.runtime.lights().lights(),
                decals: &[],
                environment: Some(&self.environment),
                reflection_probes: None,
                gi: None,
            };
            let view_target = ViewTarget {
                texture: target,
                rect: None,
                load: LoadOp::DontCare,
            };
            let view_frame = match generated {
                Some((_, generated_target)) => self
                    .renderer
                    .add_view_generating(
                        &mut fg,
                        &mut context,
                        view,
                        &scene_input,
                        &camera,
                        view_target,
                        generated_target,
                        |_, _| {},
                    )
                    .map_err(AppError::at("the view's passes"))?,
                None => self
                    .renderer
                    .add_view(
                        &mut fg,
                        &mut context,
                        view,
                        &scene_input,
                        &camera,
                        view_target,
                    )
                    .map_err(AppError::at("the view's passes"))?,
            };
            // The UI: on the real frame and on a generated one shown before it; with DLSS
            // frame generation alone into a layer of its own, cleared to nothing
            // (Streamline composites it over the frames it generates), then over the
            // copied frame.
            let ui_target = (self.dlss.is_some() && !layer.is_empty()).then(|| {
                let texture = fg.create_texture(
                    "ui layer",
                    TextureDesc::new_2d(self.swapchain_format, extent.width, extent.height),
                );
                fg.add_pass("ui layer clear")
                    .write(texture, Access::TransferWrite)
                    .execute(move |ctx| {
                        let range = vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        };
                        // SAFETY: recording; the graph made the layer a transfer
                        // destination.
                        unsafe {
                            ctx.device.cmd_clear_color_image(
                                ctx.cmd,
                                ctx.image(texture),
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                                &vk::ClearColorValue::default(),
                                &[range],
                            );
                        }
                    });
                texture
            });
            if let Some(ui) = &self.ui
                && !layer.is_empty()
            {
                ui.renderer
                    .add_passes(
                        &mut fg,
                        &mut self.ring,
                        &self.bindless,
                        layer,
                        ui_target.unwrap_or(target),
                        &camera,
                        &view_frame,
                    )
                    .map_err(AppError::at("the UI"))?;
                if let (Some((_, g)), Some(frame_g)) = (generated, view_frame.generated)
                    && frame_g.shown
                {
                    ui.renderer
                        .add_passes(
                            &mut fg,
                            &mut self.ring,
                            &self.bindless,
                            layer,
                            g.texture,
                            &frame_g.camera,
                            &view_frame,
                        )
                        .map_err(AppError::at("the UI"))?;
                }
            }
            let backbuffer = present.as_ref().map(PresentTarget::swapchain_texture);
            if let Some(present) = present {
                present.finish(&mut fg);
            }
            if let (Some(ui), Some(_), Some(backbuffer)) = (&self.ui, ui_target, backbuffer) {
                ui.renderer
                    .add_passes(
                        &mut fg,
                        &mut self.ring,
                        &self.bindless,
                        layer,
                        backbuffer,
                        &camera,
                        &view_frame,
                    )
                    .map_err(AppError::at("the UI"))?;
            }
            if let (Some(generation), Some(id)) = (&self.dlss, frame_id)
                && let Some(input) = view_frame.dlss_generation_input(&camera, target, ui_target)
            {
                generation.add_pass(&mut fg, input, id);
            }
            generated_shown = view_frame.generated.is_some_and(|g| g.shown);
            if let Some(feedback) = &mut self.feedback {
                feedback
                    .add_pass(
                        &mut fg,
                        &self.device,
                        &mut self.allocator,
                        &self.bindless,
                        &geo,
                        &mats,
                        &view_frame.passes,
                        view_frame.render,
                        lod_bias,
                        frame.number,
                    )
                    .map_err(AppError::at("texture feedback"))?;
            }
            report.compose += us(t);
            let t = Instant::now();
            fg.execute_with_bindless(&mut self.allocator, Some(&self.bindless))
                .map_err(AppError::at("the frame graph"))?;
            report.execute = us(t);
        }

        // Submission and presentation.
        let t = Instant::now();
        let number = frame.number;
        let submit_id = self.latency.as_ref().and_then(Latency::submit_present_id);
        match (&self.swapchain, paced_images) {
            (Some(swapchain), _) => {
                let index = acquired.map_or(0, |a| a.index);
                self.frames
                    .submit_tagged(
                        self.device.graphics,
                        frame,
                        true,
                        Some(swapchain.render_finished(index)),
                        submit_id,
                    )
                    .map_err(AppError::at("submitting the frame"))?;
            }
            (None, Some(images)) => self
                .frames
                .submit_waiting(
                    self.device.graphics,
                    frame,
                    false,
                    Some(images.wait),
                    None,
                    None,
                )
                .map_err(AppError::at("submitting the frame"))?,
            (None, None) => unreachable!("one of them presents"),
        }
        report.submit = us(t);
        let t = Instant::now();
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::RenderSubmitEnd,
        );
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::PresentStart,
        );
        if let Some(latency) = &mut self.latency {
            latency.before_present();
        }
        let present_id = self.latency.as_ref().and_then(Latency::present_id);
        match (&self.swapchain, &mut self.paced) {
            (Some(swapchain), _) => {
                let index = acquired.map_or(0, |a| a.index);
                match swapchain.present_with_id(self.device.graphics, index, present_id) {
                    Ok(suboptimal) => {
                        self.resized |= suboptimal || acquired.is_some_and(|a| a.suboptimal);
                    }
                    Err(e) if e.is_out_of_date() => self.resized = true,
                    Err(e) => return Err(AppError::at("presenting")(e)),
                }
            }
            (None, Some(paced)) => {
                if let Err(e) = paced.present(PacedFrame {
                    done: (self.frames.timeline(), number),
                    generated: generated_shown,
                    present_id: None,
                    reset: !generated_shown,
                }) {
                    if e.is_out_of_date() {
                        self.resized = true;
                    } else {
                        return Err(AppError::at("presenting")(e));
                    }
                }
            }
            (None, None) => unreachable!("one of them presents"),
        }
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::PresentEnd,
        );
        report.present = us(t);
        report.cpu = us(work);
        report.generated_shown = generated_shown;
        report.pipelines = engine_rhi::creation_stats().created - pipelines_at_start;
        if self.first_present_ms.is_none() {
            self.first_present_ms = Some(ms(self.started));
        }
        Ok(Some(report))
    }

    /// Gives every frame `seconds` of game time instead of the real time since the last
    /// one (`None`): the same simulation on every machine, for benchmarks.
    pub fn set_frame_time(&mut self, seconds: Option<f64>) {
        self.frame_time = seconds;
    }

    /// Whether the swapchain no longer matches the window's client area.
    fn needs_new_swapchain(&self) -> bool {
        let extent = self.extent();
        (extent.width, extent.height) != self.client
    }

    /// Makes the swapchain anew for the window's size (after a resize, out-of-date or
    /// the monitor lost), with its frame generation and latency control.
    fn recreate_presentation(&mut self) -> Result<(), AppError> {
        match &self.hooks {
            Some(hooks) => hooks
                .device_wait_idle(&self.device)
                .map_err(AppError::at("waiting for the device"))?,
            None => self
                .frames
                .wait_idle()
                .map_err(AppError::at("waiting for the device"))?,
        }
        self.latency = None;
        if let Some(paced) = self.paced.take() {
            self.swapchain = Some(
                paced
                    .stop(&mut self.allocator)
                    .map_err(AppError::at("stopping the paced presenter"))?,
            );
        }
        if let Some(swapchain) = &mut self.swapchain {
            let extent = swapchain.extent();
            swapchain
                .recreate(&self.device, &self.surface, extent.width, extent.height)
                .map_err(AppError::at("the swapchain"))?;
        }
        self.start_presentation()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Streamline's queues drain too.
        let _ = match &self.hooks {
            Some(hooks) => hooks.device_wait_idle(&self.device),
            None => self.frames.wait_idle(),
        };
        if let Some(paced) = self.paced.take() {
            self.swapchain = paced.stop(&mut self.allocator).ok();
        }
        self.save_pipeline_cache();
        // SAFETY: the device is idle; each object is destroyed once, before the
        // allocator and descriptors it was made from.
        unsafe {
            if let Some(mut ui) = self.ui.take() {
                ui.renderer.destroy(&mut self.allocator);
            }
            self.renderer
                .destroy(&self.device, &mut self.allocator, &self.bindless);
            if let Some(mut view) = self.view.take() {
                view.destroy(&mut self.allocator, &self.bindless);
            }
            self.scene.destroy(&mut self.allocator);
            self.skinning.destroy(&mut self.allocator);
            if let Some(feedback) = &mut self.feedback {
                feedback.destroy(&mut self.allocator);
            }
            self.environment
                .destroy(&mut self.allocator, &self.bindless);
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
        }
        // Streamline before the device it is attached to (its shutdown releases what it
        // made on it); the rest drops in field order.
        self.latency = None;
        self.dlss = None;
        let _ = self.windows.destroy_window(self.window.id);
    }
}

/// What the Vulkan start-up thread makes.
struct GpuStart {
    instance: Instance,
    device: Device,
    cache_path: Option<PathBuf>,
    streamline: Option<Streamline>,
    ms: f64,
    cache: Option<String>,
    notes: Vec<String>,
}

/// The instance and device (with Streamline's extensions and queues for DLSS frame
/// generation), the pipeline cache loaded.
fn start_gpu(
    app_name: &str,
    dlss_wanted: bool,
    warm_up: bool,
    cache_dir: Option<&std::path::Path>,
    preference: &AdapterPreference,
) -> Result<GpuStart, AppError> {
    let phase = Instant::now();
    let mut notes = Vec::new();
    // DLSS frame generation: Streamline before the instance, its extensions and queues
    // with the device (ADR 0091 §7).
    let streamline = dlss_wanted
        .then(|| {
            Streamline::load(&StreamlineDesc {
                frame_generation: true,
                ..StreamlineDesc::default()
            })
            .map_err(|e| notes.push(format!("DLSS frame generation unavailable: {e}")))
            .ok()
        })
        .flatten()
        .filter(Streamline::frame_generation);
    // Validation in debug builds (`InstanceDesc::default`).
    let instance = Instance::with_extensions(
        &InstanceDesc {
            app_name: app_name.to_owned(),
            ..InstanceDesc::default()
        },
        &streamline
            .as_ref()
            .map(Streamline::instance_extensions)
            .unwrap_or_default(),
    )
    .map_err(AppError::at("the Vulkan instance"))?;
    let adapter = instance
        .select_adapter(preference)
        .map_err(AppError::at("the GPU"))?;
    let streamline = streamline.filter(|s| {
        s.check_adapter(&instance, &adapter)
            .map_err(|e| notes.push(format!("DLSS frame generation unsupported: {e}")))
            .is_ok()
    });
    let mut device = Device::with_extras(
        &instance,
        &adapter,
        &streamline
            .as_ref()
            .map(Streamline::device_extensions)
            .unwrap_or_default(),
        streamline
            .as_ref()
            .map(Streamline::extra_queues)
            .unwrap_or_default(),
    )
    .map_err(AppError::at("the device"))?;
    let streamline = streamline.filter(|s| {
        if let Err(e) = s.attach(&device) {
            notes.push(format!("DLSS frame generation unavailable: {e}"));
            return false;
        }
        if let Some(e) = s.frame_generation_error() {
            notes.push(format!("DLSS frame generation unavailable: {e}"));
        }
        if let Some(hint) = s.frame_generation_hint() {
            notes.push(hint.text().to_owned());
        }
        s.frame_generation_supported()
    });
    device.set_pipeline_warm_up(warm_up);
    let cache_path = cache_dir.map(|dir| dir.join(device.pipeline_cache_file_name()));
    let cache = cache_path
        .as_deref()
        .map(|path| format!("{:?}", device.load_pipeline_cache(path)));
    Ok(GpuStart {
        instance,
        device,
        cache_path,
        streamline,
        ms: ms(phase),
        cache,
        notes,
    })
}

/// The swapchain's full screen choice for `window` (ADR 0099).
fn full_screen_choice(window: Window, hwnd: isize) -> FullScreenExclusive {
    match window {
        Window::Exclusive => match engine_platform::display::window_monitor(hwnd) {
            Some(monitor) => FullScreenExclusive::Exclusive {
                monitor: monitor.handle,
            },
            None => FullScreenExclusive::Disallowed,
        },
        Window::FullScreen | Window::Windowed { .. } => FullScreenExclusive::Disallowed,
    }
}

/// Registers the pack's material types and materials and spawns its instances as
/// entities; returns what the scene holds.
fn spawn_scene(
    streamer: &SceneStreamer,
    renderer: &mut FrameRenderer,
    materials: &mut MaterialTable,
    world: &mut World,
) -> SceneInfo {
    let layout = &streamer.layout().scene;
    let textures = streamer.textures().to_vec();
    // Material types (ADR 0092): drawn from the pack's SPIR-V.
    let types =
        engine_streaming::register_material_types(renderer.forward_mut(), &layout.material_types);
    let material_ids = engine_streaming::add_materials(materials, layout, &types, &textures);
    let fallback = materials.default_material();
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for instance in &layout.instances {
        let world_from_object = Mat4::from_cols_array(&instance.world_from_object);
        let mesh = &layout.meshes[instance.mesh as usize];
        for corner in 0..8 {
            let pick = |bit: usize, axis: usize| {
                if corner & bit != 0 {
                    mesh.max[axis]
                } else {
                    mesh.min[axis]
                }
            };
            let c =
                world_from_object.transform_point3(Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)));
            min = min.min(c);
            max = max.max(c);
        }
        world.spawn((
            GlobalTransform(Affine3A::from_mat4(world_from_object)),
            MeshRenderer::new(
                streamer.meshes()[instance.mesh as usize],
                instance
                    .material
                    .map_or(fallback, |i| material_ids[i as usize]),
            ),
        ));
    }
    if layout.instances.is_empty() {
        (min, max) = (Vec3::ZERO, Vec3::ZERO);
    }
    SceneInfo {
        meshes: streamer.meshes().to_vec(),
        materials: material_ids,
        textures,
        bounds: (min, max),
    }
}
