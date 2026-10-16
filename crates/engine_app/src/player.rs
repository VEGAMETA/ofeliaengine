//! The player: a game's process, window, device, presentation, scene and frame loop.

use crate::game::{FrameParts, Game};
use crate::process::ProcessSetup;
use crate::report::{FrameReport, Startup, StartupPipelines};
use crate::settings::{PlayerSettings, Window};
use crate::videos::Videos;
use core::cell::RefCell;
use core::fmt;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat4, Quat, Vec3};
use engine_dlss::{DlssFrameGeneration, Streamline, StreamlineDesc};
use engine_ecs::{Entity, GlobalTransform, QueryState, World};
use engine_frame::{
    Antialiasing, FrameContext, FrameGeneration, FrameRenderer, GeneratedTarget, Placement,
    PrepareContext, Quality, RenderDefaults, SceneDraws, SceneInput, UiCanvas, UiRenderer, View,
    ViewCamera, ViewSettings, ViewTarget,
};
use engine_latency::{Latency, LatencySettings, Marker, PacedFrame, PacedPresenter, PresentImage};
use engine_platform::window::{
    WindowDesc, WindowEventKind, WindowHandle, WindowMode, WindowSystem,
};
use engine_render::budget::GpuBudget;
use engine_render::environment::{Environment, Sky};
use engine_render::feedback::{FeedbackSettings, TextureFeedback};
use engine_render::forward::{DirectionalLight, ShadowSettings, ViewGi};
use engine_render::gi::{GiSettings, ProbeGi};
use engine_render::gpu_scene::{GpuScene, InstanceId};
use engine_render::graph::{
    Access, BufferState, FrameGraph, ImportedBuffer, ImportedTexture, LoadOp, RenderGraph, Texture,
    TextureDesc, TextureState,
};
use engine_render::material::{MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId};
use engine_render::present::PresentTarget;
use engine_render::rays::{BlasDetail, RayScene, RaySettings};
use engine_render::reflections::ReflectionProbes;
use engine_render::skinning::Skinning;
use engine_render::streaming::LodView;
use engine_render::texture_pool::{TextureId, TexturePool};
use engine_render::upload::UploadRing;
use engine_render2d::Canvas;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, ExtraQueues, Frames,
    FullScreenExclusive, Instance, InstanceDesc, MemoryLocation, OutputColor, PresentMode, Surface,
    Swapchain, SwapchainDesc, VulkanHooks,
};
use engine_runtime::{
    Camera, CameraView, GiExtract, MeshRenderer, ProbeExtract, ReflectionProbe, Runtime,
};
use engine_scene::SceneId;
use engine_streaming::{SceneStreamSettings, SceneStreamer};
use std::path::{Path, PathBuf};
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

    pub(crate) fn message(what: &'static str, text: String) -> Self {
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

/// The offscreen player's image readback ([`Window::Offscreen`]).
struct Offscreen {
    width: u32,
    height: u32,
    /// Host-visible: a captured frame's pixels.
    buffer: vk::Buffer,
    memory: Option<Allocation>,
    /// The next frame copies its image into `buffer`.
    requested: bool,
    /// The frame whose image `buffer` receives.
    captured: Option<u64>,
}

impl Offscreen {
    /// Bytes of a frame: four a pixel.
    fn bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 4
    }

    /// The frame's last pass: its image copied into the readback buffer when a capture is
    /// asked for, else kept (read by a pass that is never culled).
    fn add_pass(&mut self, fg: &mut FrameGraph<'_>, image: Texture, number: u64) {
        if !self.requested {
            fg.add_pass("offscreen frame")
                .read(image, Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            return;
        }
        self.requested = false;
        self.captured = Some(number);
        let out = fg.import_buffer(
            "offscreen capture",
            ImportedBuffer {
                buffer: self.buffer,
                size: self.bytes(),
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        let (width, height) = (self.width, self.height);
        fg.add_pass("offscreen capture")
            .read(image, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::BufferImageCopy {
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording; the graph made the image a transfer source and the
                // buffer a transfer destination.
                unsafe {
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(image),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(out),
                        &[region],
                    );
                }
            });
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
    /// The pack's instances' entities, in the pack's order.
    pub instances: Vec<Entity>,
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
    /// The sky, its light, the atmosphere and fog (`set_atmosphere` needs the device, the
    /// allocator and the bindless descriptors above).
    pub environment: &'a mut Environment,
    /// Videos (ADR 0100): opened here or in any frame's [`FrameParts`](crate::FrameParts).
    pub videos: &'a mut Videos,
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
    /// Reflection probes, made with the first `ReflectionProbe` entity (ADR 0099 step 4).
    probes: Option<ReflectionProbes>,
    /// The volume of GI probes over the world's `GiVolume`, and the scene its rays trace.
    gi: Option<ProbeGi>,
    gi_rays: Option<RayScene>,
    /// The game's videos (ADR 0100).
    videos: Videos,
    /// The `loc/` folder behind the world's localizer, kept live (ADR 0102).
    text: Option<crate::text::LiveText>,
    /// The devices behind the world's `engine_input::Input` (`PlayerSettings::input`).
    input: Option<PlayerInput>,
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
    /// The window's surface (none offscreen).
    surface: Option<Arc<Surface>>,
    /// The offscreen image's readback ([`Window::Offscreen`]).
    offscreen: Option<Offscreen>,
    latency: Option<Latency>,
    dlss: Option<DlssFrameGeneration>,
    streamline: Option<Streamline>,
    hooks: Option<VulkanHooks>,
    device: Arc<Device>,
    jobs: Arc<ThreadPool>,
    windows: Option<WindowSystem>,
    window: Option<WindowHandle>,
    instance: Instance,
    _process: Option<ProcessSetup>,
    // State.
    settings: PlayerSettings,
    defaults: RenderDefaults,
    /// The `ReflectionProbe` entities, counted until the first makes the probe set; then
    /// their mirror in it.
    probe_entities: QueryState<&'static ReflectionProbe>,
    probe_extract: Option<ProbeExtract>,
    gi_extract: GiExtract,
    /// The volume's settings: `None` when it is off or the GPU has no ray queries.
    gi_settings: Option<GiSettings>,
    /// The volume is made again at the next frame (its settings changed).
    gi_remake: bool,
    /// The volume has converged since it was made: the probes were captured again.
    gi_converged: bool,
    /// The pack's textures hold their tails: from now on probes capture and the volume's
    /// rays trace the scene's materials (before, unloaded textures would read as white).
    scene_ready: bool,
    /// The streaming has settled once since the start: the probes were captured again
    /// with what it brought.
    scene_settled: bool,
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
    /// Saves (ADR 0104).
    saves: SaveState,
    /// The world's timers and tweens, moved before every tick (ADR 0104).
    timing: engine_runtime::Timing,
}

/// One of the pack's instances as the level made it: what loading a save makes again
/// when play destroyed it.
struct LevelInstance {
    id: SceneId,
    transform: Affine3A,
    mesh: MeshId,
    material: MaterialId,
}

/// The player's side of saves.
struct SaveState {
    folder: PathBuf,
    encoding: engine_save::Encoding,
    writer: crate::saves::Writer,
    /// The ids the level made when it loaded: those gone are saved as destroyed.
    level_ids: std::collections::HashSet<SceneId>,
    level: Vec<LevelInstance>,
    /// Seconds played since the last autosave, and the next autosave slot.
    since_autosave: f64,
    autosaves: u32,
    /// Milliseconds played, kept across saves.
    play_ms: u64,
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
        let offscreen = match settings.window {
            Window::Offscreen { width, height } => Some((width.max(1), height.max(1))),
            _ => None,
        };
        if offscreen.is_some() && settings.frame_generation != FrameGeneration::Off {
            startup.notes.push(String::from(
                "frame generation needs a window: offscreen frames are not generated",
            ));
        }
        let dlss_wanted = settings.frame_generation == FrameGeneration::Dlss && offscreen.is_none();
        let validation = settings.validation;
        let warm_up = settings.warm_up;
        let cache_dir = match &settings.pipeline_cache {
            Some(dir) => dir.clone(),
            None => {
                std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join(&settings.title))
            }
        };
        let preference = AdapterPreference::from_env().unwrap_or(settings.adapter.clone());
        let app_name = settings.title.clone();
        let video_decode = settings.video_decode;

        // The Vulkan instance and device (the drivers' own start-up) beside the window.
        let gpu = std::thread::Builder::new()
            .name(String::from("vulkan start-up"))
            .spawn(move || {
                start_gpu(
                    &app_name,
                    dlss_wanted,
                    video_decode,
                    warm_up,
                    cache_dir.as_deref(),
                    &preference,
                    validation,
                    offscreen.is_none(),
                )
            })
            .map_err(AppError::at("the device's start-up thread"))?;
        let phase = Instant::now();
        let (windows, window) = if offscreen.is_none() {
            let windows = WindowSystem::start(4096).map_err(AppError::at("the window system"))?;
            let (mode, width, height) = match settings.window {
                Window::Windowed { width, height } => (WindowMode::Windowed, width, height),
                _ => (WindowMode::FullScreen, 1920, 1080),
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
            (Some(windows), Some(window))
        } else {
            (None, None)
        };
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
        let surface = match window {
            Some(window) => Some(Arc::new(
                match &hooks {
                    Some(hooks) => {
                        Surface::from_win32_hooked(&instance, window.hinstance, window.hwnd, hooks)
                    }
                    None => Surface::from_win32(&instance, window.hinstance, window.hwnd),
                }
                .map_err(AppError::at("the window's surface"))?,
            )),
            None => None,
        };
        // The renderer draws into the format the swapchain will have (offscreen: 8-bit
        // sRGB, as captures read it).
        let (swapchain_format, swapchain_desc) = match (&surface, window) {
            (Some(surface), Some(window)) => {
                let desc = SwapchainDesc {
                    present_mode: if settings.vsync {
                        PresentMode::Fifo
                    } else {
                        PresentMode::Immediate
                    },
                    srgb: true,
                    color: OutputColor::Sdr,
                    // Reflex's swapchain only when latency control is on: off, it costs
                    // nothing.
                    low_latency: settings.low_latency != engine_latency::LowLatency::Off,
                    full_screen_exclusive: full_screen_choice(settings.window, window.hwnd),
                    ..SwapchainDesc::default()
                };
                let format = Swapchain::format_for(&device, surface, &desc)
                    .map_err(AppError::at("the swapchain's format"))?;
                (format, Some(desc))
            }
            _ => (vk::Format::R8G8B8A8_SRGB, None),
        };
        let device = Arc::new(device);
        let pending_swapchain = match (&surface, swapchain_desc) {
            (Some(surface), Some(swapchain_desc)) => {
                let (device, surface, hooks) = (Arc::clone(&device), Arc::clone(surface), hooks);
                Some(
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
                        .map_err(AppError::at("the swapchain's thread"))?,
                )
            }
            _ => None,
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
        if settings.contact_shadows.is_some() {
            renderer
                .forward_mut()
                .set_contact_shadows(&device, &bindless, settings.contact_shadows)
                .map_err(AppError::at("contact shadows"))?;
        }
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
        // Screen-space reflections from the start (a view setting); probes and the GI
        // volume come with their first entities.
        if settings.screen_space_reflections.is_some() {
            renderer
                .forward_mut()
                .set_reflections(&device, &bindless, true)
                .map_err(AppError::at("reflections"))?;
        }
        let gi_settings = settings
            .global_illumination
            .unwrap_or_else(|| Some(GiSettings::for_device(&device)));
        if gi_settings.is_some() && device.ray_query().is_none() {
            startup.notes.push(String::from(
                "no ray queries on this GPU: GiVolume entities light nothing",
            ));
        }
        let gi_settings = gi_settings.filter(|_| device.ray_query().is_some());
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
        // The game's text: its localizer a resource of the world.
        let text = match &settings.text {
            Some(source) => {
                let phase = Instant::now();
                let languages = settings
                    .languages
                    .clone()
                    .unwrap_or_else(engine_platform::locale::preferred_languages);
                let (localizer, live) = crate::text::load_text(source, &languages, &jobs)
                    .map_err(|e| AppError::message("the game's text", e))?;
                startup.notes.push(format!(
                    "text in `{}` ({} locales; the player's languages: {})",
                    localizer.locale(),
                    localizer.locales().count(),
                    languages.join(", ")
                ));
                world.insert_resource(localizer);
                startup.phases.push(("the game's text", ms(phase)));
                live
            }
            None => None,
        };
        // Saves (ADR 0104): the game asks through the world's resource.
        let save_folder = crate::saves::save_folder(&settings.saves, &settings.title);
        let save_status =
            std::sync::Arc::new(std::sync::Mutex::new(crate::saves::WriterStatus::default()));
        world.insert_resource(crate::Saves::new(
            save_folder.clone(),
            std::sync::Arc::clone(&save_status),
        ));
        // The game's input: its actions a resource of the world.
        let input = settings.input.as_ref().map(|map| {
            world.insert_resource(engine_input::Input::new(map.clone()));
            PlayerInput::start(offscreen.is_none(), &mut startup.notes)
        });
        let mut scene_info = SceneInfo::default();
        let mut level = Vec::new();
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
                scene_info = spawn_scene(
                    &streamer,
                    &mut renderer,
                    &mut materials,
                    &mut world,
                    &mut level,
                );
                Some(streamer)
            }
            None => None,
        };
        startup
            .phases
            .push(("scene opened from its pack (tails resident)", ms(phase)));

        let mut base = defaults.view_settings();
        base.screen_space_reflections = settings.screen_space_reflections;
        let cameras = CameraView::new(&mut world, base);
        let probe_entities = QueryState::new(&mut world);
        let gi_extract = GiExtract::new(&mut world);
        let mut scene = GpuScene::new(&device, &bindless, world.len() as u32 + 16, in_flight)
            .map_err(AppError::at("the GPU scene"))?;
        scene.set_jobs(jobs.clone());
        // Timers and tweens on the game's clock (ADR 0104), moved before each tick.
        let timing = engine_runtime::Timing::new(&mut world);
        let mut runtime = Runtime::<InstanceId>::new(world, settings.tick_hz);
        runtime.set_jobs(jobs.clone());
        let ring = UploadRing::new(&device, &mut allocator, in_flight, 32 << 20)
            .map_err(AppError::at("the upload ring"))?;
        let frames = Frames::new(&device, device.graphics.family, in_flight)
            .map_err(AppError::at("the frame slots"))?;
        let graph = RenderGraph::new(&device, in_flight);

        // The swapchain, made beside all of the above; offscreen, the image's readback.
        let wait = Instant::now();
        let swapchain = match pending_swapchain {
            Some(pending) => {
                let (swapchain, swapchain_ms) = pending.join().map_err(|_| {
                    AppError::message("the swapchain", String::from("its thread panicked"))
                })?;
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
                Some(swapchain)
            }
            None => None,
        };
        let offscreen = match offscreen {
            Some((width, height)) => {
                let bytes = u64::from(width) * u64::from(height) * 4;
                let (buffer, memory) = allocator
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(bytes)
                            .usage(vk::BufferUsageFlags::TRANSFER_DST),
                        MemoryLocation::Readback,
                    )
                    .map_err(AppError::at("the offscreen readback"))?;
                Some(Offscreen {
                    width,
                    height,
                    buffer,
                    memory: Some(memory),
                    requested: false,
                    captured: None,
                })
            }
            None => None,
        };

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
            probes: None,
            gi: None,
            gi_rays: None,
            videos: Videos::new(),
            text,
            input,
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
            swapchain,
            surface,
            offscreen,
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
            probe_entities,
            probe_extract: None,
            gi_extract,
            gi_settings,
            gi_remake: false,
            gi_converged: false,
            scene_ready: false,
            scene_settled: false,
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
            timing,
            saves: SaveState {
                folder: save_folder.clone(),
                encoding: settings
                    .saves
                    .encoding
                    .unwrap_or(engine_save::Encoding::for_build()),
                writer: crate::saves::Writer::new(save_status),
                level_ids: level.iter().map(|l| l.id).collect(),
                level,
                since_autosave: 0.0,
                autosaves: 0,
                play_ms: 0,
            },
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
            environment: &mut self.environment,
            videos: &mut self.videos,
        }
    }

    /// What the pack's scene holds.
    pub fn scene_info(&self) -> &SceneInfo {
        &self.scene_info
    }

    /// The pack's bodies in `physics` (ADR 0101): its level geometry static, its props
    /// moving their instances' entities. Nothing without a pack. A game without physics
    /// never calls it, and the pack's colliders are not read.
    pub fn load_physics(
        &mut self,
        physics: &mut crate::ScenePhysics,
    ) -> Result<crate::SceneBodies, AppError> {
        let Some(path) = self.settings.pack.clone() else {
            return Ok(crate::SceneBodies::default());
        };
        let jobs = Arc::clone(&self.jobs);
        let instances = self.scene_info.instances.clone();
        physics.load_pack(&path, &jobs, self.world_mut(), &instances)
    }

    /// The pack's navigation mesh (ADR 0103) as the world's [`engine_nav::NavMesh`]
    /// resource, for the game's paths: whether the pack has one (none without a pack, or
    /// cooked without). A game that does not navigate never calls it, and the mesh is not
    /// read.
    pub fn load_navigation(&mut self) -> Result<bool, AppError> {
        let Some(path) = &self.settings.pack else {
            return Ok(false);
        };
        let pack = engine_assets::pack::Pack::open(path).map_err(AppError::at("the pack"))?;
        let data = engine_assets::navigation::load_navigation(&pack, &self.jobs)
            .map_err(AppError::at("the pack's navigation mesh"))?;
        let Some(data) = data else {
            return Ok(false);
        };
        let mesh = engine_nav::NavMesh::new(&data).map_err(AppError::at("the navigation mesh"))?;
        self.runtime.world.insert_resource(mesh);
        Ok(true)
    }

    /// The game's world.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.runtime.world
    }

    /// The game's text (the world's resource), if [`PlayerSettings::text`] gave one.
    pub fn localizer(&self) -> Option<&engine_loc::Localizer> {
        self.runtime.world.resource::<engine_loc::Localizer>()
    }

    /// The game's text, to choose its locale (`set_locale` with the player's choice).
    pub fn localizer_mut(&mut self) -> Option<&mut engine_loc::Localizer> {
        self.runtime.world.resource_mut::<engine_loc::Localizer>()
    }

    /// What the last compile of a live `loc/` folder found (errors keep a locale's previous
    /// text); empty for a pack.
    pub fn text_diagnostics(&self) -> &[engine_loc::Diagnostic] {
        self.text
            .as_ref()
            .map_or(&[], crate::text::LiveText::diagnostics)
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

    /// The view settings cameras do not carry (`engine_runtime::CameraView::base_mut`):
    /// bloom, the tone curve, grading, color vision, depth of field, occlusion culling,
    /// cached shadows, the clear color. A camera entity gives its antialiasing, render
    /// size, exposure and kind.
    pub fn view_settings_mut(&mut self) -> &mut ViewSettings {
        self.cameras.base_mut()
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

    /// The timeline semaphore that reaches a frame's number ([`FrameReport::number`]) when
    /// its GPU work is done: another thread can wait for frames with it (the flight's
    /// input-to-GPU-done measurement, ADR 0023).
    pub fn frame_timeline(&self) -> vk::Semaphore {
        self.frames.timeline()
    }

    /// GPU memory.
    pub fn allocator(&self) -> &Allocator {
        &self.allocator
    }

    /// The frame graph (its statistics, the GPU times of the last frames measured).
    pub fn graph(&self) -> &RenderGraph {
        &self.graph
    }

    /// The GPU memory budget's last frame (ADR 0076): its share, what the rest of the
    /// process holds, the streaming pool.
    pub fn budget(&self) -> engine_render::budget::BudgetStats {
        self.budget.stats()
    }

    /// The frame renderer.
    pub fn renderer(&self) -> &FrameRenderer {
        &self.renderer
    }

    /// The streamed scene, when the player opened a pack.
    pub fn streamer(&self) -> Option<&SceneStreamer> {
        self.streamer.as_ref()
    }

    /// The reflection probes, once a `ReflectionProbe` entity made them.
    pub fn reflection_probes(&self) -> Option<&ReflectionProbes> {
        self.probes.as_ref()
    }

    /// The `ReflectionProbe` entities' mirror in [`Self::reflection_probes`]: each entity's
    /// probe (to capture a manual one again: `ReflectionProbes::refresh`).
    pub fn probe_extract(&self) -> Option<&ProbeExtract> {
        self.probe_extract.as_ref()
    }

    /// The volume of GI probes over the world's `GiVolume`, while there is one.
    pub fn gi_volume(&self) -> Option<&ProbeGi> {
        self.gi.as_ref()
    }

    /// The scene the GI volume's rays trace, while there is a volume.
    pub fn ray_scene(&self) -> Option<&RayScene> {
        self.gi_rays.as_ref()
    }

    /// The GI volume's settings, when a `GiVolume` entity would get one (the setting is
    /// on and the GPU has ray queries).
    pub fn gi_settings(&self) -> Option<GiSettings> {
        self.gi_settings
    }

    /// Changes the GI volume's settings, or turns it off (`None`); a player's graphics
    /// options. The volume is made again with them at the next frame and converges again.
    /// Without ray queries nothing changes.
    pub fn set_global_illumination(&mut self, settings: Option<GiSettings>) {
        if self.device.ray_query().is_some() {
            self.gi_settings = settings;
            self.gi_remake = true;
        }
    }

    /// The pack's textures hold their tails: probes capture and the GI volume's rays trace
    /// the scene's materials from now on.
    pub fn scene_ready(&self) -> bool {
        self.scene_ready
    }

    /// The streaming has gone quiet once since the start (the view's textures and mesh
    /// levels arrived) and the probes were then asked for new captures: with
    /// [`Self::reflection_probes`]' pending captures done and [`Self::gi_volume`]
    /// converged, the world looks as it will (a loading screen's end).
    pub fn scene_settled(&self) -> bool {
        self.scene_settled
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

    /// The frame's size: the swapchain's, or the offscreen image's.
    pub fn extent(&self) -> vk::Extent2D {
        match (&self.swapchain, &self.paced, &self.offscreen) {
            (Some(swapchain), _, _) => swapchain.extent(),
            (None, Some(paced), _) => paced.extent(),
            (None, None, Some(offscreen)) => vk::Extent2D {
                width: offscreen.width,
                height: offscreen.height,
            },
            (None, None, None) => vk::Extent2D::default(),
        }
    }

    /// The window (none offscreen).
    pub fn window(&self) -> Option<WindowHandle> {
        self.window
    }

    /// The sky, its light, the atmosphere and fog (`engine_render::environment`): set them
    /// for the world (`set_sky`, `set_atmosphere`, `set_fog`, `set_panorama`).
    pub fn environment_mut(&mut self) -> &mut Environment {
        &mut self.environment
    }

    /// Runs one frame like [`Self::frame`] and returns its image once the GPU has finished
    /// it: 8-bit sRGB RGBA, rows top to bottom. Offscreen players only
    /// ([`Window::Offscreen`]); `None` when the frame rendered nothing.
    ///
    /// # Errors
    ///
    /// As [`Self::frame`], and on a player with a window.
    pub fn capture(&mut self, game: &mut impl Game) -> Result<Option<Vec<u8>>, AppError> {
        let Some(offscreen) = &mut self.offscreen else {
            return Err(AppError::message(
                "capturing a frame",
                String::from("only an offscreen player captures"),
            ));
        };
        offscreen.requested = true;
        offscreen.captured = None;
        let rendered = self.frame(game)?;
        let Some(offscreen) = &mut self.offscreen else {
            unreachable!("checked above");
        };
        offscreen.requested = false;
        let Some(number) = offscreen.captured.take().filter(|_| rendered.is_some()) else {
            return Ok(None);
        };
        self.frames
            .wait_for_frame(number)
            .map_err(AppError::at("waiting for the captured frame"))?;
        let Some(offscreen) = &self.offscreen else {
            unreachable!("checked above");
        };
        let bytes = offscreen.bytes() as usize;
        let mapped = offscreen
            .memory
            .as_ref()
            .and_then(Allocation::mapped_ptr)
            .ok_or_else(|| {
                AppError::message(
                    "capturing a frame",
                    String::from("the readback is not mapped"),
                )
            })?;
        // SAFETY: the frame that wrote the buffer has finished; the mapping covers `bytes`.
        let pixels = unsafe { core::slice::from_raw_parts(mapped.as_ptr(), bytes) };
        Ok(Some(pixels.to_vec()))
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
            let mut focus_lost = false;
            if let Some(windows) = &mut self.windows {
                windows.drain_events(|event| {
                    match event.kind {
                        WindowEventKind::CloseRequested => *closing = true,
                        WindowEventKind::Resized { width, height } => {
                            *resized = true;
                            *minimized = width == 0 || height == 0;
                            *client = (width, height);
                        }
                        WindowEventKind::Minimized => *minimized = true,
                        WindowEventKind::Focused(false) => focus_lost = true,
                        _ => {}
                    }
                    game.event(&event);
                });
            }
            if focus_lost
                && self.input.is_some()
                && let Some(input) = self.runtime.world.resource_mut::<engine_input::Input>()
            {
                input.release_all();
            }
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
        // A live `loc/` folder: its new tables between two frames.
        if let Some(live) = &mut self.text
            && let Some(localizer) = self.runtime.world.resource_mut::<engine_loc::Localizer>()
        {
            live.update(localizer);
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
                    log::info!(target: "engine_app::present", "acquiring an image: {e}; the swapchain is made anew");
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

        // The input since the last frame, then sampled before each tick and presentation.
        let sample = match &mut self.input {
            Some(devices) => {
                if let Some(input) = self.runtime.world.resource_mut::<engine_input::Input>() {
                    devices.feed(input);
                }
                true
            }
            None => false,
        };
        // Ticks, presentation, interpolation (ADR 0064).
        let stages = {
            let game_cell = RefCell::new(&mut *game);
            let timing = &mut self.timing;
            self.runtime.frame(
                elapsed,
                &(),
                &mut self.scene,
                |world, (), dt| {
                    if sample && let Some(input) = world.resource_mut::<engine_input::Input>() {
                        input.sample_tick(Instant::now());
                    }
                    timing.advance(world, dt);
                    game_cell.borrow_mut().tick(world, dt);
                },
                |world, (), alpha| {
                    if sample && let Some(input) = world.resource_mut::<engine_input::Input>() {
                        input.sample_frame(Instant::now());
                    }
                    game_cell.borrow_mut().present(world, alpha);
                },
            )
        };
        report.ticks = stages.ticks;
        report.simulate = stages.simulate_us;
        report.propagate = stages.propagate_us;
        report.extract = stages.extract_us;
        report.interpolate = stages.interpolate_us + stages.present_us;
        // Saves asked for during the ticks: taken now, at the tick boundary.
        self.process_saves(game, elapsed);
        let t = Instant::now();
        game.animate(
            &mut FrameParts {
                runtime: &self.runtime,
                scene: &mut self.scene,
                skinning: &mut self.skinning,
                geometry: &mut self.geometry,
                jobs: &self.jobs,
                videos: &mut self.videos,
            },
            elapsed as f32,
        );
        // Skinned meshes at their scene instances, then their bounds (ADR 0099 step 3).
        self.runtime.place_skinned(&self.scene, &mut self.skinning);
        self.skinning
            .update_bounds(&mut self.geometry, Some(&mut self.scene));
        report.animate = us(t);
        let t = Instant::now();
        self.sync_probes()?;
        report.probes = us(t);
        // Videos: their clocks, decoding ahead, the pictures due (ADR 0100).
        self.videos.update(
            &self.device,
            &mut self.allocator,
            &self.bindless,
            &mut self.textures,
            &mut self.materials,
            frame.number,
            self.frames.in_flight(),
            elapsed,
        );
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
        // Probes and the GI volume see the scene's materials once the pack's textures
        // hold their tails; once the streaming settles, the probes are captured again.
        if !self.scene_ready {
            let textures = &self.textures;
            self.scene_ready = self
                .scene_info
                .textures
                .iter()
                .all(|&t| textures.resident_level(t).is_some());
        }
        if self.scene_ready
            && !self.scene_settled
            && self.streamer.as_ref().is_none_or(SceneStreamer::idle)
            && self.textures.pending().0 == 0
        {
            self.scene_settled = true;
            if let Some(probes) = &self.probes {
                probes.refresh_all();
            }
        }
        let scene_ready = self.scene_ready;
        let probes_shown =
            scene_ready && self.probe_extract.as_ref().is_some_and(|e| !e.is_empty());
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
            // The videos' new pictures, before every pass that may sample them.
            self.videos.add_passes(
                &mut fg,
                &self.device,
                &self.bindless,
                &mut self.textures,
                &mut self.materials,
                frame.number,
                frame.slot(),
            );
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
            // The scene for the GI volume's rays, after the GPU scene's passes placed its
            // instances (ADR 0087).
            let ray_frame = match (self.gi_rays.as_mut(), &scene_frame) {
                (Some(rays), Some(scene)) => rays
                    .add_passes(
                        &mut fg,
                        &self.device,
                        &mut self.allocator,
                        &mut self.ring,
                        &self.bindless,
                        &geo,
                        scene,
                        frame.number,
                    )
                    .map_err(AppError::at("the scene for ray queries"))?,
                _ => None,
            };
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
            let (present, generated, offscreen_target) =
                match (&self.swapchain, paced_images, &self.offscreen) {
                    (Some(swapchain), _, _) => (
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
                        None,
                    ),
                    (None, Some(images), _) => (
                        None,
                        Some((
                            import(&mut fg, "real frame", images.real),
                            GeneratedTarget {
                                texture: import(&mut fg, "generated frame", images.generated),
                                format: images.generated.raw_format,
                                source_format: images.real.raw_format,
                            },
                        )),
                        None,
                    ),
                    (None, None, Some(_)) => (
                        None,
                        None,
                        Some(fg.create_texture(
                            "offscreen frame",
                            TextureDesc::new_2d(self.swapchain_format, extent.width, extent.height),
                        )),
                    ),
                    (None, None, None) => {
                        return Err(AppError::message(
                            "the frame",
                            String::from("nothing presents"),
                        ));
                    }
                };
            let target = match (&present, &generated, offscreen_target) {
                (Some(present), _, _) => present.texture,
                (None, Some((real, _)), _) => *real,
                (None, None, Some(image)) => image,
                (None, None, None) => unreachable!("one of them presents"),
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
                reflection_probes: self.probes.as_ref().filter(|_| probes_shown),
                gi: self.gi.as_ref().map(|volume| ViewGi::Probes {
                    volume: volume.begin_frame(&mut fg),
                    rays: ray_frame.filter(|_| scene_ready),
                }),
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
            if let (Some(image), Some(offscreen)) = (offscreen_target, &mut self.offscreen) {
                offscreen.add_pass(&mut fg, image, frame.number);
            }
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
            // Reflex sees the work the graph submits itself (async compute) as this frame's.
            fg.tag_submissions(self.latency.as_ref().and_then(Latency::submit_present_id));
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
            (None, None) => self
                .frames
                .submit(self.device.graphics, frame, false, None)
                .map_err(AppError::at("submitting the frame"))?,
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
            // Offscreen: nothing is presented.
            (None, None) => {}
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

    /// Waits until the GPU has finished every frame (Streamline's queues too).
    fn wait_idle(&self) -> Result<(), AppError> {
        match &self.hooks {
            Some(hooks) => hooks.device_wait_idle(&self.device),
            None => self.frames.wait_idle(),
        }
        .map_err(AppError::at("waiting for the device"))
    }

    /// Reflection probes and the GI volume after the frame's ticks (ADR 0099 step 4).
    /// The probe set (with the renderer's reflections) is made with the first
    /// `ReflectionProbe` entity and then follows the entities. The GI volume (with the
    /// scene for its rays) is made with the world's `GiVolume`, made anew when its grid
    /// changes (the GPU waits for the frames that use the old one) and goes with it.
    fn sync_probes(&mut self) -> Result<(), AppError> {
        if let Some(settings) = self.settings.reflection_probes {
            if self.probes.is_none() && self.probe_entities.entity_count(&self.runtime.world) > 0 {
                self.renderer
                    .forward_mut()
                    .set_reflections(&self.device, &self.bindless, true)
                    .map_err(AppError::at("reflections"))?;
                self.probes = Some(
                    ReflectionProbes::new(
                        &self.device,
                        &mut self.allocator,
                        &self.bindless,
                        settings,
                    )
                    .map_err(AppError::at("reflection probes"))?,
                );
                self.probe_extract = Some(ProbeExtract::new(&mut self.runtime.world));
            }
            if let (Some(probes), Some(extract)) = (&mut self.probes, &mut self.probe_extract) {
                extract.sync(&mut self.runtime.world, probes);
            }
        }
        // Probes capture the scene as the volume lights it: again once it has converged.
        if !self.gi_converged && self.gi.as_ref().is_some_and(ProbeGi::is_converged) {
            self.gi_converged = true;
            if let Some(probes) = &self.probes {
                probes.refresh_all();
            }
        }
        let grid = self
            .gi_settings
            .and_then(|_| self.gi_extract.grid(&self.runtime.world));
        if grid == self.gi.as_ref().map(ProbeGi::grid) && !core::mem::take(&mut self.gi_remake) {
            return Ok(());
        }
        self.gi_converged = false;
        if self.gi.is_some() {
            self.wait_idle()?;
            if let Some(mut old) = self.gi.take() {
                // SAFETY: the device is idle.
                unsafe { old.destroy(&self.device, &mut self.allocator, &self.bindless) };
            }
        }
        match grid.zip(self.gi_settings) {
            Some((grid, settings)) => {
                if self.gi_rays.is_none() {
                    // The meshes' coarsest levels: enough for diffuse light, and resident
                    // from the start in streamed packs (ADR 0087).
                    let rays = RaySettings {
                        detail: BlasDetail::Coarsest,
                        ..RaySettings::for_device(&self.device)
                    };
                    self.gi_rays = Some(
                        RayScene::new(
                            &self.device,
                            &self.bindless,
                            rays,
                            self.settings.frames_in_flight.max(1),
                        )
                        .map_err(AppError::at("the scene for ray queries"))?,
                    );
                }
                self.gi = Some(
                    ProbeGi::new(
                        &self.device,
                        &mut self.allocator,
                        &self.bindless,
                        grid,
                        settings,
                    )
                    .map_err(AppError::at("the GI volume"))?,
                );
            }
            None => {
                if let Some(mut rays) = self.gi_rays.take() {
                    // SAFETY: the device is idle (waited for above).
                    unsafe { rays.destroy(&self.device, &mut self.allocator) };
                }
            }
        }
        self.renderer
            .forward_mut()
            .set_global_illumination(&self.device, &self.bindless, self.gi.is_some())
            .map_err(AppError::at("global illumination"))
    }

    /// Makes the swapchain anew for the window's size (after a resize, out-of-date or
    /// the monitor lost), with its frame generation and latency control.
    fn recreate_presentation(&mut self) -> Result<(), AppError> {
        self.wait_idle()?;
        self.latency = None;
        if let Some(paced) = self.paced.take() {
            self.swapchain = Some(
                paced
                    .stop(&mut self.allocator)
                    .map_err(AppError::at("stopping the paced presenter"))?,
            );
        }
        if let (Some(swapchain), Some(surface)) = (&mut self.swapchain, &self.surface) {
            let extent = swapchain.extent();
            swapchain
                .recreate(&self.device, surface, extent.width, extent.height)
                .map_err(AppError::at("the swapchain"))?;
            log::info!(
                target: "engine_app::present",
                "swapchain made anew: {}×{}, holds the monitor: {}, refused: {:?}",
                extent.width,
                extent.height,
                swapchain.exclusive(),
                swapchain.exclusive_refused()
            );
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
            if let Some(probes) = &mut self.probes {
                probes.destroy(&mut self.allocator, &self.bindless);
            }
            if let Some(gi) = &mut self.gi {
                gi.destroy(&self.device, &mut self.allocator, &self.bindless);
            }
            if let Some(rays) = &mut self.gi_rays {
                rays.destroy(&self.device, &mut self.allocator);
            }
            self.videos.destroy(
                &self.device,
                &mut self.allocator,
                &self.bindless,
                self.frames.submitted(),
            );
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            if let Some(offscreen) = &mut self.offscreen
                && let Some(memory) = offscreen.memory.take()
            {
                self.allocator.destroy_buffer(offscreen.buffer, memory);
            }
        }
        // Streamline before the device it is attached to (its shutdown releases what it
        // made on it); the rest drops in field order.
        self.latency = None;
        self.dlss = None;
        if let (Some(windows), Some(window)) = (&self.windows, self.window) {
            let _ = windows.destroy_window(window.id);
        }
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
#[allow(clippy::too_many_arguments)]
fn start_gpu(
    app_name: &str,
    dlss_wanted: bool,
    video_decode: bool,
    warm_up: bool,
    cache_dir: Option<&std::path::Path>,
    preference: &AdapterPreference,
    validation: Option<bool>,
    surface: bool,
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
            validation: validation.unwrap_or(InstanceDesc::default().validation),
            sync_validation: validation == Some(true),
            surface,
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
        ExtraQueues {
            video_decode,
            ..streamline
                .as_ref()
                .map(Streamline::extra_queues)
                .unwrap_or_default()
        },
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
        Window::FullScreen | Window::Windowed { .. } | Window::Offscreen { .. } => {
            FullScreenExclusive::Disallowed
        }
    }
}

impl Player {
    /// The game's saves: the world's [`crate::Saves`] resource (quicksave, slots, the
    /// last outcomes).
    pub fn saves_mut(&mut self) -> Option<&mut crate::Saves> {
        self.runtime.world.resource_mut::<crate::Saves>()
    }

    /// The folder of the game's save slots.
    pub fn save_folder(&self) -> &Path {
        &self.saves.folder
    }

    /// Takes the frame's save and load requests (and a due autosave), at the tick
    /// boundary after the frame's ticks.
    fn process_saves(&mut self, game: &mut impl Game, frame_seconds: f64) {
        self.saves.play_ms += (frame_seconds * 1000.0) as u64;
        let mut requests = match self.runtime.world.resource_mut::<crate::Saves>() {
            Some(saves) if !saves.requests.is_empty() => core::mem::take(&mut saves.requests),
            _ => Vec::new(),
        };
        if let Some(auto) = self.settings.saves.autosave {
            self.saves.since_autosave += frame_seconds;
            if self.saves.since_autosave >= auto.every.as_secs_f64()
                && !self.saves.writer.pending()
                && game.can_autosave(&self.runtime.world)
            {
                requests.push(crate::saves::Request::Autosave);
            }
        }
        for request in requests {
            match request {
                crate::saves::Request::Save(slot) => self.take_save(game, &slot),
                crate::saves::Request::Autosave => {
                    let slots = self.settings.saves.autosave.map_or(3, |a| a.slots.max(1));
                    let n = self.saves.autosaves % slots + 1;
                    self.saves.autosaves = self.saves.autosaves.wrapping_add(1);
                    self.saves.since_autosave = 0.0;
                    self.take_save(game, &format!("autosave {n}"));
                }
                crate::saves::Request::Load(slot) => {
                    let result = self.load_save(game, &slot).map_err(|e| e.to_string());
                    if let Err(e) = &result {
                        log::error!("load {slot}: {e}");
                    }
                    if let Some(saves) = self.runtime.world.resource_mut::<crate::Saves>() {
                        saves.last_load = Some(result);
                    }
                }
            }
        }
    }

    /// Takes a save of the world now and hands it to the writer thread.
    fn take_save(&mut self, game: &mut impl Game, slot: &str) {
        engine_profiler::zone!("app::take_save");
        let world = &self.runtime.world;
        let options = engine_save::CaptureOptions {
            level: Some(&self.saves.level_ids),
            ..engine_save::CaptureOptions::default()
        };
        let mut image = engine_save::capture(world, &options);
        image.set_state(
            "clock",
            &crate::saves::ClockState {
                remainder: self.runtime.clock.remainder(),
                play_ms: self.saves.play_ms,
            },
        );
        game.before_save(world, &mut image);
        let header = engine_save::SaveHeader {
            slot: slot.to_owned(),
            game_version: self.settings.saves.game_version.clone(),
            level: self
                .settings
                .pack
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            play_ms: self.saves.play_ms,
            ..engine_save::SaveHeader::default()
        };
        self.saves.writer.send(crate::saves::Job {
            image,
            path: engine_save::slot_path(&self.saves.folder, slot),
            slot: slot.to_owned(),
            encoding: self.saves.encoding,
            header,
        });
    }

    /// Loads `slot` now: the level's objects destroyed in play made again from the pack,
    /// the entities the save makes again removed, the save applied, the clock restored, a
    /// camera cut.
    fn load_save(
        &mut self,
        game: &mut impl Game,
        slot: &str,
    ) -> Result<engine_save::LoadReport, AppError> {
        engine_profiler::zone!("app::load_save");
        let path = engine_save::slot_path(&self.saves.folder, slot);
        let (_, save) = engine_save::read_save(&path).map_err(AppError::at("the save"))?;
        let world = &mut self.runtime.world;
        let present: std::collections::HashSet<SceneId> = {
            let mut ids = QueryState::<&SceneId>::new(world);
            ids.iter_readonly(world).copied().collect()
        };
        for l in self.saves.level.iter().filter(|l| !present.contains(&l.id)) {
            world.spawn((
                l.id,
                GlobalTransform(l.transform),
                MeshRenderer::new(l.mesh, l.material),
            ));
        }
        for e in engine_save::anonymous_entities(world) {
            world.despawn(e);
        }
        game.before_load(world, &save);
        let report = engine_save::apply(world, &save).map_err(AppError::at("the save"))?;
        if let Ok(Some(clock)) = save.state::<crate::saves::ClockState>("clock") {
            self.runtime.clock.set_remainder(clock.remainder);
            self.saves.play_ms = clock.play_ms;
        }
        let world = &mut self.runtime.world;
        // A load is a camera cut: the temporal history starts over.
        let mut cameras = QueryState::<&mut engine_runtime::Camera>::new(world);
        cameras.for_each(world, |camera| camera.cut());
        game.after_load(world, &save, &report);
        Ok(report)
    }
}

/// Registers the pack's material types and materials and spawns its instances as
/// entities (with their `SceneId`s from the pack's keys, kept in `level` for saves);
/// returns what the scene holds.
fn spawn_scene(
    streamer: &SceneStreamer,
    renderer: &mut FrameRenderer,
    materials: &mut MaterialTable,
    world: &mut World,
    level: &mut Vec<LevelInstance>,
) -> SceneInfo {
    let layout = &streamer.layout().scene;
    let textures = streamer.textures().to_vec();
    // Material types (ADR 0092): drawn from the pack's SPIR-V.
    let types =
        engine_streaming::register_material_types(renderer.forward_mut(), &layout.material_types);
    let material_ids = engine_streaming::add_materials(materials, layout, &types, &textures);
    let fallback = materials.default_material();
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut instances = Vec::with_capacity(layout.instances.len());
    for (index, instance) in layout.instances.iter().enumerate() {
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
        let transform = Affine3A::from_mat4(world_from_object);
        let mesh = streamer.meshes()[instance.mesh as usize];
        let material = instance
            .material
            .map_or(fallback, |i| material_ids[i as usize]);
        let renderer = MeshRenderer::new(mesh, material);
        instances.push(match layout.instance_keys.get(index) {
            // Known to saves by its key (pack version 9).
            Some(&key) => {
                let id = SceneId::from_u128(key);
                level.push(LevelInstance {
                    id,
                    transform,
                    mesh,
                    material,
                });
                world.spawn((id, GlobalTransform(transform), renderer))
            }
            None => world.spawn((GlobalTransform(transform), renderer)),
        });
    }
    if layout.instances.is_empty() {
        (min, max) = (Vec3::ZERO, Vec3::ZERO);
    }
    SceneInfo {
        meshes: streamer.meshes().to_vec(),
        materials: material_ids,
        textures,
        bounds: (min, max),
        instances,
    }
}

/// The devices behind the game's input actions (`PlayerSettings::input`): raw keyboard and
/// mouse input on its own thread with timestamps (CLAUDE.md §7), and XInput gamepads.
struct PlayerInput {
    raw: Option<engine_platform::raw_input::RawInput>,
    pads: Option<engine_input::platform::XInput>,
}

impl PlayerInput {
    /// Starts the devices (`devices`: a window; offscreen there are none, and a test feeds
    /// the world's `Input` itself). A device that cannot start is noted and left out.
    fn start(devices: bool, notes: &mut Vec<String>) -> Self {
        if !devices {
            return Self {
                raw: None,
                pads: None,
            };
        }
        let raw = match engine_platform::raw_input::RawInput::start(
            engine_platform::raw_input::RawInputConfig::default(),
        ) {
            Ok(raw) => Some(raw),
            Err(e) => {
                notes.push(format!("raw keyboard and mouse input unavailable: {e}"));
                None
            }
        };
        Self {
            raw,
            pads: Some(engine_input::platform::XInput::new()),
        }
    }

    /// Everything the devices reported since the last call, into `input`.
    fn feed(&mut self, input: &mut engine_input::Input) {
        if let Some(raw) = &mut self.raw {
            raw.drain(|event| {
                if let Some(event) = engine_input::platform::from_raw(&event) {
                    input.handle(&event);
                }
            });
        }
        if let Some(pads) = &mut self.pads {
            pads.poll(Instant::now(), |event| input.handle(&event));
        }
    }
}
