//! The player: a game's process, window, device, presentation, scene and frame loop.

use crate::game::{FrameParts, Game};
use crate::overlay::{Overlay, OverlayOutput, OverlayPass, OverlayView};
use crate::process::ProcessSetup;
use crate::report::{FrameReport, Startup, StartupPipelines};
use crate::settings::{PlayerSettings, Window};
use crate::sound::Sound;
use crate::videos::Videos;
use core::cell::{Cell, RefCell};
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
use engine_platform::display::{DisplayInfo, DisplayQuery};
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
use engine_render::post::{Composite, HDR_FORMAT, OutputEncoding, OutputLevels, Tonemap};
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
    FullScreenExclusive, HdrMetadata, Instance, InstanceDesc, MemoryLocation, OutputColor,
    PresentMode, Surface, Swapchain, SwapchainDesc, VulkanHooks,
};
use engine_runtime::{
    Camera, CameraView, GiExtract, MeshRenderer, ProbeExtract, ReflectionProbe, Runtime,
};
use engine_scene::SceneId;
use engine_streaming::{SceneStreamSettings, SceneStreamer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

mod debugger;

/// Peak luminance assumed for an HDR display that reports none: DisplayHDR 400, the
/// lowest HDR certification.
const FALLBACK_PEAK_NITS: f32 = 400.0;

/// The output for `display` (ADR 0038): `choice` (an HDR encoding, or SDR to stay in SDR)
/// while Windows runs the display in HDR, at its SDR white level and peak; SDR otherwise.
/// Frame generation narrows the choice: FSR's interpolates scRGB, DLSS's is not verified
/// in HDR yet.
fn wanted_levels(
    choice: OutputColor,
    generation: FrameGeneration,
    display: Option<&DisplayInfo>,
) -> OutputLevels {
    let color = match generation {
        FrameGeneration::Off => choice,
        FrameGeneration::Fsr if choice.is_hdr() => OutputColor::ScRgb,
        _ => OutputColor::Sdr,
    };
    match display {
        Some(d) if d.hdr_enabled && color.is_hdr() => OutputLevels {
            color,
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

/// What an output of `color` delivers of `wanted`: SDR when it fell back to SDR.
fn actual_levels(color: OutputColor, wanted: OutputLevels) -> OutputLevels {
    if color.is_hdr() {
        OutputLevels { color, ..wanted }
    } else {
        OutputLevels::SDR
    }
}

/// The output a presentable format holds.
fn color_of(format: vk::Format) -> OutputColor {
    match format {
        vk::Format::R16G16B16A16_SFLOAT => OutputColor::ScRgb,
        vk::Format::A2B10G10R10_UNORM_PACK32 | vk::Format::A2R10G10B10_UNORM_PACK32 => {
            OutputColor::Hdr10
        }
        _ => OutputColor::Sdr,
    }
}

/// The offscreen image's format for an output of `color`.
fn offscreen_format(color: OutputColor) -> vk::Format {
    match color {
        OutputColor::Sdr => vk::Format::R8G8B8A8_SRGB,
        OutputColor::ScRgb => vk::Format::R16G16B16A16_SFLOAT,
        OutputColor::Hdr10 => vk::Format::A2B10G10R10_UNORM_PACK32,
    }
}

/// What frames are drawn in for an output of `format`: the output itself, or for HDR10 a
/// linear intermediate, encoded at the end (blending in PQ is not linear).
fn output_format_for(format: vk::Format, levels: &OutputLevels) -> vk::Format {
    if levels.color == OutputColor::Hdr10 {
        HDR_FORMAT
    } else {
        format
    }
}

/// How the passes that draw the frame (the views' tone map, the UI, overlays) write it:
/// the output's encoding, or reference white as 1 in HDR10's intermediate.
fn output_encoding(levels: &OutputLevels) -> OutputEncoding {
    match levels.color {
        OutputColor::Hdr10 => OutputEncoding::LINEAR,
        _ => levels.swapchain_encoding(),
    }
}

/// The value white is written as under `encoding` (`Renderer2d::set_white`).
fn white_of(encoding: OutputEncoding) -> f32 {
    match encoding {
        OutputEncoding::Linear { white } => white,
        OutputEncoding::Pq { .. } => 1.0,
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

/// `[x, y, width, height]` kept inside `extent`, at least a pixel.
fn clamp_rect(rect: [u32; 4], extent: vk::Extent2D) -> vk::Rect2D {
    let x = rect[0].min(extent.width.saturating_sub(1));
    let y = rect[1].min(extent.height.saturating_sub(1));
    vk::Rect2D {
        offset: vk::Offset2D {
            x: x as i32,
            y: y as i32,
        },
        extent: vk::Extent2D {
            width: rect[2].clamp(1, (extent.width - x).max(1)),
            height: rect[3].clamp(1, (extent.height - y).max(1)),
        },
    }
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
    /// Bytes a pixel of the image: 4, or 8 for scRGB.
    pixel: u32,
    /// Host-visible: a captured frame's pixels.
    buffer: vk::Buffer,
    memory: Option<Allocation>,
    /// The next frame copies its image into `buffer`.
    requested: bool,
    /// The frame whose image `buffer` receives.
    captured: Option<u64>,
}

impl Offscreen {
    /// Bytes of a frame.
    fn bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * u64::from(self.pixel)
    }

    /// Reads back images of `format` from now on: the buffer made again for another
    /// pixel size. The device must be idle.
    fn set_format(
        &mut self,
        format: vk::Format,
        allocator: &mut Allocator,
    ) -> Result<(), AppError> {
        let pixel = if format == vk::Format::R16G16B16A16_SFLOAT {
            8
        } else {
            4
        };
        if pixel == self.pixel {
            return Ok(());
        }
        if let Some(memory) = self.memory.take() {
            // SAFETY: the device is idle (the caller's word): no copy writes the buffer.
            unsafe { allocator.destroy_buffer(self.buffer, memory) };
        }
        self.pixel = pixel;
        let (buffer, memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(self.bytes())
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .map_err(AppError::at("the offscreen readback"))?;
        self.buffer = buffer;
        self.memory = Some(memory);
        Ok(())
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
    /// The meshes and materials scenes name (ADR 0108), and the models drawn with them.
    assets: crate::Assets,
    /// Blockout brushes' meshes (ADR 0110).
    blockout: crate::Blockout,
    /// The project's Luau scripts (ADR 0114): their components and hooks; none without a
    /// project's `scripts/` folder. Boxed: a stage stopped halfway (ADR 0119) holds the host
    /// where it is.
    scripts: Option<Box<crate::scripts::Scripts>>,
    /// The part of the output the scene covers ([`Player::set_view_rect`]); `None`: all of
    /// it.
    view_rect: Option<[u32; 4]>,
    /// The cursor is locked over the window ([`Player::lock_cursor`]).
    cursor_locked: bool,
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
    /// Sound (ADR 0105): the mixer, its device, the sources' voices; the videos' sound
    /// plays through it.
    sound: Sound,
    /// The network session (ADR 0106), with `PlayerSettings::network`.
    net: Option<Box<dyn crate::net::NetDrive>>,
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
    /// HDR10: encodes the frame drawn in its intermediate into the output. Declared before
    /// the device, so its pipeline goes first.
    encode: Option<Tonemap>,
    bindless: Bindless,
    allocator: Allocator,
    // Presentation.
    paced: Option<PacedPresenter>,
    swapchain: Option<Swapchain>,
    /// The window's surface (none offscreen).
    surface: Option<Arc<Surface>>,
    /// The offscreen image's readback ([`Window::Offscreen`]).
    offscreen: Option<Offscreen>,
    /// The view in a window of its own ([`Player::open_view_window`]).
    view_window: Option<ViewWindow>,
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
    /// The format presented (the swapchain's) or captured (the offscreen image's).
    present_format: vk::Format,
    /// The format frames are drawn in (the tone mapper's output, the UI's, overlays'):
    /// the presented one, or HDR10's linear intermediate.
    output_format: vk::Format,
    /// How frames reach the display (ADR 0038).
    levels: OutputLevels,
    /// The window's display as last reported (or assumed, offscreen).
    display: Option<DisplayInfo>,
    /// The display changed since the output last followed it.
    display_changed: bool,
    /// Counts the output's changes; overlays learn of them ([`Overlay::output_changed`]).
    output_generation: u64,
    /// The output's change the overlay last learned of.
    overlay_generation: u64,
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
    /// The world's time runs ([`Player::set_simulating`]).
    simulating: bool,
    /// The next frame waits until the assets its models use are made (ADR 0108's E4-4):
    /// the first, the one after a scene is loaded, and one asked for.
    wait_for_assets: bool,
    /// Previews asked for, drawn one a frame (ADR 0112).
    previews: crate::previews::Previews,
    /// Ticks to run while the world is kept still ([`Player::step_tick`]).
    steps: u32,
    /// Fixed ticks run since the start ([`Player::ticks_run`]).
    ticks_run: u64,
    /// The pause point a script reached (ADR 0119): the world was kept still after its
    /// tick, and the editor is told ([`Player::take_debug_pause`]).
    debug_pause: Option<(String, u32)>,
    /// Whether the scripts are debugged ([`Player::set_script_debugging`]), and the
    /// debugger's stage once they are: their fiber and the stage stopped on it.
    script_debugging: bool,
    script_debug: Option<crate::script_debug::ScriptDebug>,
    /// VS Code's script debugger in a standalone player (ADR 0119 E9-3,
    /// `PlayerSettings::debugger`): its endpoint and what its client asked.
    debugger: Option<Box<debugger::Debugger>>,
    /// The game's last error in [`Game::before_ticks`], said once.
    scene_error: Option<String>,
    /// The player's own physics world (`PlayerSettings::physics`, ADR 0114 E7-3), and its
    /// last error, said once.
    own_physics: Option<crate::ScenePhysics>,
    physics_error: Option<String>,
}

/// The view in a window of its own (ADR 0107 E3-4: the editor's Play): the scene drawn
/// there instead of into the main output, which then shows only what overlays draw.
struct ViewWindow {
    output: ViewOutput,
    /// The view draws here; else into the main output as before, the window keeping its
    /// last frame ([`Player::show_view_in_window`]).
    shown: bool,
    /// Its close button was pressed ([`Player::view_window_close_requested`]).
    close_requested: bool,
    /// It has the keyboard: the game's input listens only then.
    focused: bool,
    resized: bool,
    minimized: bool,
    /// The cursor is locked over it ([`Player::lock_view_cursor`]).
    cursor_locked: bool,
}

/// The offscreen view window's readback, if the view window is offscreen.
fn view_offscreen(view: &mut Option<ViewWindow>) -> Option<&mut Offscreen> {
    match view.as_mut().map(|v| &mut v.output) {
        Some(ViewOutput::Offscreen(o)) => Some(o),
        _ => None,
    }
}

/// The view window's image this frame.
enum ViewImage {
    /// Its swapchain's image `index`, acquired with `acquired`.
    Window { index: u32, acquired: vk::Semaphore },
    /// Its offscreen image (made in the frame's graph).
    Offscreen,
}

/// Where a [`ViewWindow`] shows.
enum ViewOutput {
    Window {
        handle: WindowHandle,
        swapchain: Box<Swapchain>,
        /// Its surface, kept until the swapchain made on it goes.
        surface: Arc<Surface>,
        /// Per frame slot: the semaphore its image's acquisition signals.
        acquired: Vec<vk::Semaphore>,
    },
    /// Offscreen: an image of its own, read back by [`Player::capture_view_window`].
    Offscreen(Offscreen),
}

impl ViewOutput {
    fn extent(&self) -> vk::Extent2D {
        match self {
            Self::Window { swapchain, .. } => swapchain.extent(),
            Self::Offscreen(o) => vk::Extent2D {
                width: o.width,
                height: o.height,
            },
        }
    }
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
        let pipeline_statistics = settings.pipeline_statistics;

        // The Vulkan instance and device (the drivers' own start-up) beside the window.
        let gpu = std::thread::Builder::new()
            .name(String::from("vulkan start-up"))
            .spawn(move || {
                start_gpu(
                    &app_name,
                    dlss_wanted,
                    (video_decode, pipeline_statistics),
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
                    placement: settings.window_placement,
                    cloaked: settings.start_hidden,
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
        // The window's display: HDR from the first frame while Windows runs it in HDR
        // (ADR 0038).
        let display = window
            .as_ref()
            .and_then(|w| DisplayQuery::new().window(w.hwnd));
        let wanted = wanted_levels(settings.hdr, settings.frame_generation, display.as_ref());
        // The renderer draws into the format the swapchain will have (offscreen: 8-bit
        // sRGB, as captures read it).
        let (present, swapchain_desc) = match (&surface, window) {
            (Some(surface), Some(window)) => {
                let mut desc = SwapchainDesc {
                    present_mode: if settings.vsync {
                        PresentMode::Fifo
                    } else {
                        PresentMode::Immediate
                    },
                    srgb: true,
                    color: wanted.color,
                    // Reflex's swapchain only when latency control is on: off, it costs
                    // nothing.
                    low_latency: settings.low_latency != engine_latency::LowLatency::Off,
                    full_screen_exclusive: full_screen_choice(settings.window, window.hwnd),
                    ..SwapchainDesc::default()
                };
                let mut format = Swapchain::format_for(&device, surface, &desc)
                    .map_err(AppError::at("the swapchain's format"))?;
                // Frame generation draws the output itself: HDR10 (a fallback for a
                // surface without scRGB) would blend the UI in PQ, so SDR instead.
                if settings.frame_generation != FrameGeneration::Off
                    && color_of(format) == OutputColor::Hdr10
                {
                    desc.color = OutputColor::Sdr;
                    format = Swapchain::format_for(&device, surface, &desc)
                        .map_err(AppError::at("the swapchain's format"))?;
                }
                (format, Some(desc))
            }
            _ => (vk::Format::R8G8B8A8_SRGB, None),
        };
        let levels = match &swapchain_desc {
            Some(_) => actual_levels(color_of(present), wanted),
            None => OutputLevels::SDR,
        };
        let output = output_format_for(present, &levels);
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
            output,
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
        // Sound (ADR 0105): the mixer and its buses now, as the project's layout lays them
        // out (ADR 0121), the device with the first sound; the game sets the buses'
        // volumes on top of the faders through the world's `Mixer`.
        let sound = Sound::new(&settings.audio, &load_layout(&layout_path(settings)));
        world.insert_resource(sound.mixer());
        // Networking (ADR 0106): the session the game asks for through `Network`.
        let net = settings.network.as_ref().map(|config| {
            world.insert_resource(config.network());
            config.driver(settings.tick_hz)
        });
        if settings.audio.enabled {
            startup.notes.push(sound.note().to_owned());
        }
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
                    pixel: 4,
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
            sound,
            net,
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
            view_window: None,
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
            present_format: present,
            output_format: output,
            levels: OutputLevels::SDR,
            display,
            display_changed: false,
            encode: None,
            output_generation: 0,
            overlay_generation: 0,
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
            simulating: true,
            wait_for_assets: true,
            previews: crate::previews::Previews::new(settings.frames_in_flight),
            steps: 0,
            ticks_run: 0,
            debug_pause: None,
            script_debugging: false,
            script_debug: None,
            debugger: None,
            scene_error: None,
            own_physics: None,
            physics_error: None,
            assets: assets(settings),
            blockout: crate::Blockout::new(),
            scripts: None,
            view_rect: None,
            cursor_locked: false,
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
        // The levels the swapchain was made for: the views' encoding and peak, the UI's
        // white, HDR10's encode pass and the display's metadata.
        player.apply_levels(levels)?;
        player.start_presentation()?;
        // VS Code's script debugger (ADR 0119 E9-3): a development build's only, its scripts
        // compiled to be debugged.
        let debugger = settings.debugger.filter(|_| cfg!(debug_assertions));
        if settings.debugger.is_some() && debugger.is_none() {
            log::warn!(
                target: "script",
                "--debug: this build has no script debugger; a development build has one \
                 (`cargo run -- --debug`)"
            );
        }
        if debugger.is_some() {
            player.settings.debuggable_scripts = true;
        }
        // The scripts before any scene: scenes name their components (ADR 0114 §1).
        if let Some(project) = &settings.project {
            let folder = project.join("scripts");
            if folder.is_dir() {
                let world = &mut player.runtime.world;
                let script_settings = player.settings.script_settings();
                match crate::scripts::Scripts::load_with(world, project, &folder, script_settings) {
                    Ok(scripts) => player.scripts = Some(Box::new(scripts)),
                    Err(e) => {
                        log::error!(target: "script", "the scripts' VM: {e}");
                    }
                }
            }
        }
        if let Some(scene) = &settings.scene {
            player.load_scene(scene)?;
        }
        // The player's own physics world, when the game asks for one (ADR 0114 E7-3): the
        // pack's bodies now, the scene's models and brushes before every frame's ticks.
        if let Some(physics) = &settings.physics {
            let mut own = crate::ScenePhysics::new(physics)?;
            if let Some(pack) = &settings.pack {
                let jobs = Arc::clone(&player.jobs);
                let instances = player.scene_info.instances.clone();
                own.load_pack(pack, &jobs, &mut player.runtime.world, &instances)?;
            }
            player.own_physics = Some(own);
        }
        if let Some(asked) = debugger {
            player.start_debugger(asked);
        }
        // Saved now as well as at exit: a crash does not cost the next start its cache.
        player.save_pipeline_cache();
        Ok(player)
    }

    /// The player's own physics world (`PlayerSettings::physics`), if it has one.
    pub fn physics(&self) -> Option<&crate::ScenePhysics> {
        self.own_physics.as_ref()
    }

    /// The player's own physics world, to add bodies of the game's own or query it.
    pub fn physics_mut(&mut self) -> Option<&mut crate::ScenePhysics> {
        self.own_physics.as_mut()
    }

    /// Loads a scene file (ADR 0108; a path relative to `PlayerSettings::project`) into the
    /// world beside what it holds: its entities with every reflected component the program
    /// has (`SceneRegistry::from_program`), the hierarchy, the prefabs its instances place
    /// (read from the project's folder, E5), and the meshes and materials its `Model`s
    /// name, made in the next frame. Lines the types no longer match are warnings in the
    /// log and in the report.
    ///
    /// # Errors
    ///
    /// The file cannot be read, or it is not a scene (with the line); nothing is added then.
    pub fn load_scene(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<engine_scene::LoadReport, AppError> {
        let path = self.assets.root().join(path);
        let text = std::fs::read_to_string(&path).map_err(AppError::at("the scene file"))?;
        let registry = engine_scene::SceneRegistry::from_program();
        let prefabs = engine_scene::PrefabFiles(self.assets.root());
        let options = engine_scene::LoadOptions {
            prefabs: Some(&prefabs),
        };
        let report = engine_scene::load_with(&mut self.runtime.world, &registry, &text, &options)
            .map_err(AppError::at("the scene file"))?;
        for (line, warning) in &report.warnings {
            log::warn!(target: "engine_app::scene", "{}:{line}: {warning}", path.display());
        }
        self.assets.add_paths(&report.assets);
        self.wait_for_assets = true;
        Ok(report)
    }

    /// Asks for a preview of `request`'s parts (ADR 0112): drawn in a later frame, once
    /// each part is made with its textures, and handed out by [`Self::take_previews`]. It
    /// replaces a request of the same key not drawn yet.
    pub fn request_preview(&mut self, request: crate::PreviewRequest) {
        self.previews.ask(request);
    }

    /// The previews drawn since the last call, appended to `out`.
    pub fn take_previews(&mut self, out: &mut Vec<crate::Preview>) {
        self.previews.take(out);
    }

    /// Asks for a material graph's node previews (ADR 0123's E10-4g): an atlas drawn in a
    /// later frame from `request.material`'s type (a graph's editing build), once it is
    /// made and no material type is on its way, handed out by [`Self::take_node_previews`].
    /// It replaces a request of the same key not drawn yet.
    pub fn request_node_previews(&mut self, request: crate::NodePreviewRequest) {
        self.previews.ask_nodes(request);
    }

    /// The node previews drawn since the last call, appended to `out`.
    pub fn take_node_previews(&mut self, out: &mut Vec<crate::NodePreviews>) {
        self.previews.take_nodes(out);
    }

    /// The editor's live model preview (ADR 0124's E12-1b), or none: a skinned model's mesh
    /// posed at a clip's moment and turned, drawn alone each frame that changes either (its
    /// skin made in the background as a model's is, an instance of it the preview's own).
    /// Asking every frame for the same model allocates nothing.
    pub fn set_model_preview(&mut self, preview: Option<crate::ModelPreview>) {
        self.assets.set_preview_skin(
            preview.map(|p| (p.mesh, p.material)),
            &mut self.skinning,
            &mut self.geometry,
        );
        self.assets.pose_preview(preview.and_then(|p| p.pose));
        self.previews.set_live(preview);
    }

    /// The live model preview's rig, once its skin is made.
    pub fn model_preview_rig(&self) -> Option<&crate::Rig> {
        self.assets.preview_rig()
    }

    /// The live model preview's newest drawing into `out` ([`crate::PREVIEW_SIZE`]², RGBA8
    /// sRGB, rows top to bottom, transparent around it; its room kept), when one came back
    /// since the last call.
    pub fn take_model_preview(&mut self, out: &mut Vec<u8>) -> bool {
        self.previews.take_live(out)
    }

    /// The rig of entity `e`'s skinned model (ADR 0124's E12-1a): its skeleton and its
    /// file's clips that move it. `None` for a model drawn without a skin, or before its
    /// skin is made (a frame or a few after its scene loads).
    pub fn model_rig(&self, e: Entity) -> Option<&crate::Rig> {
        self.assets.model_rig(e)
    }

    /// Poses entity `e`'s skinned model at clip `clip`'s moment `time` (held at the clip's
    /// ends) or, with `None`, in its rest pose: what the editor shows while it edits (ADR
    /// 0124's E12-1b). The scene does not change. False for a model drawn without a skin.
    pub fn pose_model(&mut self, e: Entity, pose: Option<(usize, f32)>) -> bool {
        self.assets.pose_model(e, pose)
    }

    /// Makes the next frame wait until every asset its models use is made: what the frame
    /// after [`Self::load_scene`] does. Assets are otherwise made in the background, and a
    /// model draws once its own are (ADR 0108's E4-4).
    pub fn wait_for_assets(&mut self) {
        self.wait_for_assets = true;
    }

    /// The meshes and materials scenes name (ADR 0108).
    pub fn assets(&self) -> &crate::Assets {
        &self.assets
    }

    /// The GPU memory of the textures: the live ones', and the retired ones' not freed yet.
    pub fn texture_memory(&self) -> (u64, u64) {
        self.textures.memory()
    }

    /// The same, to learn asset paths or make an asset ahead of its first use.
    pub fn assets_mut(&mut self) -> &mut crate::Assets {
        &mut self.assets
    }

    /// Whether the world's time runs (ADR 0107 E3): the game's ticks, the timers and tweens,
    /// the sources' sound. Off, the world is drawn as it is and kept still: what the editor
    /// does while editing (a scene's tweens, timers and sounds wait for Play), and what a
    /// pause is. On by default.
    pub fn set_simulating(&mut self, on: bool) {
        self.simulating = on;
    }

    /// Whether the world's time runs ([`Self::set_simulating`]).
    pub fn simulating(&self) -> bool {
        self.simulating
    }

    /// Fixed ticks run since the start, a tick a script stopped in among them: the
    /// Debugger's header names the stopped one (ADR 0119 E9-4a).
    pub fn ticks_run(&self) -> u64 {
        self.ticks_run
    }

    /// While the world is kept still, runs its next tick as if it were not: the timers
    /// and tweens move and the game's tick runs, once (the editor's step while paused).
    pub fn step_tick(&mut self) {
        self.steps += 1;
    }

    /// Steps asked for that have not run yet ([`Self::step_tick`]).
    pub fn steps_pending(&self) -> u32 {
        self.steps
    }

    /// The pause point a script reached since the last call (ADR 0119), its file
    /// (`scripts/door.luau`) and line: the world was kept still after the tick it was
    /// reached in, the frame's other ticks not run ([`Self::set_simulating`] goes on).
    pub fn take_debug_pause(&mut self) -> Option<(String, u32)> {
        self.debug_pause.take()
    }

    /// Ends what a run of the game left in the player (ADR 0107 E3, the editor's Stop):
    /// every video closed, every voice stopped, the network session left.
    pub fn end_run(&mut self) {
        // A stage stopped halfway ends first (ADR 0119).
        self.end_script_stop();
        self.videos.close_all();
        self.sound.stop_all();
        if let Some(net) = self.net.as_mut() {
            net.end(&mut self.runtime.world);
        }
    }

    /// Reads asset file `file` again on its next use, and makes the models that use it
    /// again ([`crate::Assets::reload`]): after the file was written anew.
    pub fn reload_asset(&mut self, file: engine_scene::AssetId) {
        self.assets.reload(&mut self.runtime.world, file);
    }

    /// Asset file `file` at `path` (under the project) was written anew, as a watcher of
    /// the folder sees it ([`crate::Assets::changed`]): what was made from it is made again
    /// in the background, the old version drawn until the new one is whole; an image makes
    /// the glTF files that use it again.
    pub fn asset_changed(&mut self, file: engine_scene::AssetId, path: &str) {
        self.assets.changed(&mut self.runtime.world, file, path);
        // The mixer's layout written anew: the mixer follows it, what plays goes on.
        if Path::new(path) == self.settings.audio.layout {
            self.reload_mixer_layout();
        }
    }

    /// The mixer laid out as `layout` says (ADR 0121): what the editor's mixer panel
    /// holds, unsaved. Buses it no longer has go (their voices on to the master), new ones
    /// are made, the others change where they stand.
    pub fn set_mixer_layout(&mut self, layout: &crate::MixerFile) {
        self.sound.set_layout(layout);
    }

    /// The mixer laid out again from its file (`AudioSettings::layout` under the project),
    /// or the built-in buses without one.
    pub fn reload_mixer_layout(&mut self) {
        let layout = load_layout(&self.mixer_layout_path());
        self.sound.set_layout(&layout);
    }

    /// Where the mixer's layout file is (`AudioSettings::layout` under the project).
    pub fn mixer_layout_path(&self) -> PathBuf {
        layout_path(&self.settings)
    }

    /// The editor's listening aids (E11-1b): `muted` buses silent, and while any bus is
    /// `soloed`, only the soloed ones, the buses they feed and those that feed them heard.
    /// On top of the game's own mutes (`Mixer::set_muted`), which stay as they are.
    pub fn set_mixer_listening(&mut self, muted: &[String], soloed: &[String]) {
        self.sound.set_listening(muted, soloed);
    }

    /// The editor's hearing in place (ADR 0107 E11-4b): while the world is kept still
    /// ([`Self::set_simulating`]), `entities`' sources play where they are, looping, heard
    /// from the main camera with the reverb zones; an empty list ends it. The world is not
    /// written to: no position, no source stopped, nothing despawned.
    pub fn audition(&mut self, entities: &[Entity]) {
        self.sound.set_audition(entities);
    }

    /// The entities heard in place ([`Self::audition`]).
    pub fn auditioned(&self) -> &[Entity] {
        self.sound.audition()
    }

    /// Bus `bus`'s peak level per channel since the last call (linear, after its fader;
    /// `None` without sound or such a bus): what the editor's meters show.
    pub fn take_bus_peak(&self, bus: &str) -> Option<[f32; 2]> {
        self.sound.take_peak(bus)
    }

    /// The room the listener hears now: the room's reverb as it plays, and what it follows
    /// (a reverb zone and its weight, the walls, nothing); the editor's room strip
    /// (E11-2c). Its bus's meter is `take_bus_peak("reverb")`.
    pub fn room_heard(&self) -> crate::RoomHeard {
        self.sound.room_heard()
    }

    /// The mixer's snapshot `name` heard alone at full weight over the mix, whatever the
    /// game blended (the editor's chosen snapshot, ADR 0122); none: the game's weights
    /// again.
    pub fn hear_snapshot(&mut self, name: Option<&str>) {
        self.sound.hear_snapshot(name);
    }

    /// How far effect `effect` (its place in the chain) of bus `bus` (`master` for the
    /// master's) lowered the gain since the last call, in dB (`None` without sound, such a
    /// bus or such an effect): a compressor's or a limiter's meter in the editor.
    pub fn take_effect_reduction(&self, bus: &str, effect: usize) -> Option<f32> {
        self.sound.take_reduction(bus, effect)
    }

    /// The blockout brushes' meshes (ADR 0110).
    pub fn blockout(&self) -> &crate::Blockout {
        &self.blockout
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
            fps_limit: self.fps_limit(),
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

    /// The frame cap's rate now ([`PlayerSettings::frame_cap`]): the window's display's
    /// refresh rate decides [`crate::FrameCap::Display`].
    fn fps_limit(&self) -> Option<f64> {
        self.settings.frame_cap.fps(
            self.settings.vsync,
            self.display.as_ref().and_then(|d| d.refresh_hz),
            self.settings.frame_generation != FrameGeneration::Off,
        )
    }

    /// A frame cap from the next frame on (a game's settings menu).
    ///
    /// # Errors
    ///
    /// When the latency control cannot take the new rate.
    pub fn set_frame_cap(&mut self, cap: crate::FrameCap) -> Result<(), AppError> {
        self.settings.frame_cap = cap;
        self.follow_frame_cap()
    }

    /// The latency control's rate made the frame cap's again: after a setting, and when
    /// the window goes to a display of another refresh rate (`start_presentation` takes
    /// it when the latency control is made).
    fn follow_frame_cap(&mut self) -> Result<(), AppError> {
        let fps = self.fps_limit();
        let Some(latency) = &mut self.latency else {
            return Ok(());
        };
        if latency.settings().fps_limit == fps {
            return Ok(());
        }
        let settings = LatencySettings {
            fps_limit: fps,
            ..latency.settings()
        };
        let applied = match (&self.paced, &self.swapchain) {
            (Some(paced), _) => latency.set_settings(paced, settings),
            (None, Some(swapchain)) => latency.set_settings(swapchain, settings),
            (None, None) => return Ok(()),
        };
        applied.map_err(AppError::at("latency control"))
    }

    /// The project's scripts (ADR 0114), when it has a `scripts/` folder that loaded; none
    /// while a stage stopped halfway holds them ([`Self::script_stop`]).
    pub fn scripts(&self) -> Option<&crate::scripts::Scripts> {
        if self.script_held() {
            return None;
        }
        self.scripts.as_deref()
    }

    /// The project's scripts, to change; none while a stage stopped halfway holds them.
    pub fn scripts_mut(&mut self) -> Option<&mut crate::scripts::Scripts> {
        if self.script_held() {
            return None;
        }
        self.scripts.as_deref_mut()
    }

    /// The scripts' debugger apart from the host (ADR 0119): it reads and sets breakpoints
    /// at a stop too.
    pub fn script_debugger(&self) -> Option<engine_script::ScriptDebugger> {
        match &self.script_debug {
            Some(debug) => Some(debug.debugger().clone()),
            None => self
                .scripts
                .as_deref()
                .map(crate::scripts::Scripts::debugger),
        }
    }

    /// Debugs the scripts (ADR 0119): their stages run on a fiber of their own, and a stop
    /// breakpoint stops a stage halfway, the frames going on without its rest and without
    /// ticks, until [`Self::continue_script`]. Off: stops pause the game after their tick,
    /// as pause points. The editor's Play turns it on.
    pub fn set_script_debugging(&mut self, on: bool) {
        self.script_debugging = on;
        if !on {
            self.end_script_stop();
            self.script_debug = None;
            if let Some(scripts) = self.scripts.as_deref() {
                scripts.set_debugging(false);
            }
        }
    }

    /// Runs the game's scripts interpreted (`true`) or as Luau's native code (ADR 0119's
    /// full script debugging; the editor's Play sets it as its switch says): interpreted, a
    /// stop at an error reads every frame's locals, and the scripts run slower (8-33 % on
    /// ADR 0114's scenes). Loaded modules run again when the choice changes, in the order
    /// they require each other, their instances going on with their state; a host made
    /// later follows it. Not while a script is stopped: the next call switches them. How
    /// many modules ran again.
    pub fn set_interpreted_scripts(&mut self, on: bool) -> usize {
        self.settings.interpreted_scripts = on;
        if self.script_held() {
            return 0;
        }
        match self.scripts.as_deref_mut() {
            Some(scripts) => scripts.set_native(&mut self.runtime.world, !on),
            None => 0,
        }
    }

    /// Plays sound file `file` (absolute, or from the project's folder) as it is, from
    /// `from` seconds, looping or not: the editor's preview (E11-3b). On a bus of its own
    /// straight into the master, dry, heard while the world stands still; one at a time, a
    /// new one stopping the last with a 20 ms fade.
    pub fn preview_sound(&mut self, file: &Path, from: f64, looping: bool) {
        let file = if file.is_absolute() {
            file.to_owned()
        } else {
            self.assets.root().join(file)
        };
        self.sound.preview(&file, from, looping);
    }

    /// The editor's preview stopped, faded out.
    pub fn stop_sound_preview(&mut self) {
        self.sound.stop_preview();
    }

    /// The editor's preview as it plays, if there is one.
    pub fn sound_preview(&self) -> Option<crate::PreviewState<'_>> {
        self.sound.preview_state()
    }

    /// Material file `file` as the editor holds it, read in place of the file on disk and
    /// drawn by the next frame (`None`: the file again; [`crate::Assets::set_material`],
    /// ADR 0120's E10-1c).
    pub fn set_material_file(
        &mut self,
        file: engine_scene::AssetId,
        held: Option<(crate::MaterialFile, engine_scene::AssetPaths)>,
    ) {
        self.assets.set_material(file, held, &mut self.materials);
    }

    /// Draws material file `material` with its look, the glTF material its type falls back
    /// to, instead of its type, or with its type again (E10-4f's Draw as Fallback): the
    /// forward pass's time in the profiler then shows what its type's shading costs over the
    /// engine's own. A material without a type draws its look anyway.
    pub fn draw_as_fallback(&mut self, material: engine_scene::AssetRef, on: bool) {
        self.assets.draw_as_fallback(material, on);
    }

    /// A material type's module at `path` written anew by the caller (the editor's graph
    /// modules, ADR 0123 E10-3g-1): its reload due at once rather than when hot reload's
    /// watcher sees the file, so what waits for the material types sees it on its way.
    pub fn material_source_written(&mut self, path: &Path) {
        self.renderer.forward_mut().material_source_changed(path);
    }

    /// Material type `type_name` of module `module` (an absolute path), registered with the
    /// forward renderer if it was not (ADR 0092): what [`Self::material_params`] asks of.
    pub fn material_type(
        &mut self,
        module: &Path,
        type_name: &str,
    ) -> engine_render::material::MaterialTypeId {
        self.renderer.forward_mut().register_material_type(
            engine_render::forward::MaterialTypeDesc {
                name: type_name.to_owned(),
                module: module.to_owned(),
                type_name: type_name.to_owned(),
            },
        )
    }

    /// What material type `ty`'s shading costs this GPU (E10-4c): its fragment stage's
    /// registers and spill per shading variant, beside the engine's own shading's. `None`
    /// until its pipelines are made (asking makes them, off the frame's thread), or without
    /// [`crate::PlayerSettings::pipeline_statistics`].
    pub fn material_cost(
        &self,
        ty: engine_render::material::MaterialTypeId,
    ) -> Option<engine_render::forward::MaterialCost> {
        self.renderer
            .forward()
            .material_type_cost(self.device(), ty)
    }

    /// Material type `ty`'s parameters' layout, reflected off the frame's thread (ADR 0120's
    /// E10-1b): `None` while on its way; then the layout or why not, and its revision.
    pub fn material_params(
        &self,
        ty: engine_render::material::MaterialTypeId,
    ) -> Option<(Result<&engine_render::forward::StructLayout, &str>, u32)> {
        self.renderer.forward().material_params(ty)
    }

    /// Watches material types' modules (and `forward.slang` with what it includes) and
    /// compiles a type again when one changes, its previous version drawing until then
    /// (ADR 0092); its parameters' layout is reflected again with it, and typed material
    /// files laid out again (ADR 0120's E10-1b). Off by default; the editor turns it on.
    pub fn set_material_hot_reload(&mut self, on: bool) {
        self.renderer.forward_mut().set_material_hot_reload(on);
    }

    /// Where a script stopped, while a stage is stopped halfway.
    pub fn script_stop(&self) -> Option<&engine_script::Stopped> {
        let held = self.script_debug.as_ref()?.held.as_ref()?;
        Some(&held.at)
    }

    /// Whether the script stopped now stopped in a tick (the rest of the tick waits), not
    /// in the frame's stage (E9-4a: the Debugger's header).
    pub fn script_stop_in_tick(&self) -> bool {
        self.script_debug
            .as_ref()
            .and_then(|d| d.held.as_ref())
            .is_some_and(|h| matches!(h.kind, crate::script_debug::StageKind::Tick { .. }))
    }

    /// How many times scripts stopped since debugging began: a step's next stop is a new
    /// one, at the same line or not.
    pub fn script_stops(&self) -> u64 {
        self.script_debug.as_ref().map_or(0, |d| d.stops)
    }

    /// Luau expression `code` evaluated in frame `frame` (0: the stopped one) of the script
    /// stopped now (ADR 0119 E9-1b-5c, the Debugger's Watch and Evaluate), with the world
    /// open to it as a hook has it: its value and what it holds. It runs under the scripts'
    /// call budget; breakpoints it reaches pass.
    ///
    /// # Errors
    ///
    /// No script is stopped, the frame has no Luau, the code does not compile or it fails.
    pub fn evaluate_script(
        &mut self,
        frame: usize,
        code: &str,
    ) -> Result<engine_script::Variable, String> {
        let debugger = self
            .script_debugger()
            .ok_or_else(|| String::from("no scripts run in this game"))?;
        let budget = self.settings.script_settings().call_budget;
        let world: *mut World = &mut self.runtime.world;
        // SAFETY: the player's world, borrowed for this call only; the stopped stage holds
        // no borrow of it while it waits (ADR 0119 E9-1b-1).
        unsafe { crate::scripts::with_world(world, || debugger.evaluate(frame, code, budget)) }
    }

    /// Luau expression `code`'s value given to `place` in frame `frame` of the script
    /// stopped now (ADR 0119 E9-4b: F2 in the editor's Variables, VS Code's setVariable),
    /// with the world open to it as a hook has it: the place's value after. It runs under
    /// the scripts' call budget. A value the compiler folded into a constant (a local never
    /// written, a field of a local table never written that never leaves its function)
    /// changes where it is kept, not in the code that runs.
    ///
    /// # Errors
    ///
    /// As [`Self::evaluate_script`]'s, or the place cannot take the value.
    pub fn set_script_value(
        &mut self,
        frame: usize,
        place: &engine_script::Place,
        code: &str,
    ) -> Result<engine_script::Variable, String> {
        let debugger = self
            .script_debugger()
            .ok_or_else(|| String::from("no scripts run in this game"))?;
        let budget = self.settings.script_settings().call_budget;
        let world: *mut World = &mut self.runtime.world;
        // SAFETY: as `evaluate_script`'s.
        unsafe {
            crate::scripts::with_world(world, || debugger.set_value(frame, place, code, budget))
        }
    }

    /// The stopped stage goes on at the next frame with every stop passed until it ends,
    /// then the rest of its tick (ADR 0119 E9-4a, the UX note's §7: Shift+F7 finishes the
    /// tick); the caller keeps the world still after it ([`Self::set_simulating`]).
    pub fn finish_script_tick(&mut self) {
        if let Some(debug) = &mut self.script_debug
            && let Some(held) = debug.held.as_mut()
        {
            held.answer = Some(engine_script::Resume::Continue);
            debug.debugger().skip_stops(true);
        }
    }

    /// The stopped stage goes on at the next frame, and the rest of its tick after it.
    pub fn continue_script(&mut self) {
        self.step_script(engine_script::Resume::Continue);
    }

    /// The stopped script goes on at the next frame as `how` says: on, or a step to the next
    /// line (over a call, into it, or out of the function; ADR 0119 E9-1b-2). A stop that
    /// cannot be stepped from ([`engine_script::Stopped::steppable`]) goes on.
    pub fn step_script(&mut self, how: engine_script::Resume) {
        if let Some(held) = self.script_debug.as_mut().and_then(|d| d.held.as_mut()) {
            held.answer = Some(how);
        }
    }

    /// Ends the stopped stage now (Play stops, ADR 0119): its stopped call fails, every call
    /// after it in the stage fails at once, and the rest of its tick does not run.
    pub fn end_script_stop(&mut self) {
        if let Some(debug) = &mut self.script_debug {
            // SAFETY: the player's world, alive for the rest of the stage, run here.
            unsafe { debug.abort(&mut self.runtime.world) };
            debug.debugger().skip_stops(false);
        }
    }

    /// A stage stopped halfway holds the host.
    fn script_held(&self) -> bool {
        self.script_debug.as_ref().is_some_and(|d| d.held.is_some())
    }

    /// The debugger's stage made once the scripts are there and debugging is on.
    fn ready_script_debug(&mut self) {
        if !self.script_debugging || self.script_debug.is_some() {
            return;
        }
        let Some(scripts) = self.scripts.as_deref() else {
            return;
        };
        match crate::script_debug::ScriptDebug::new(scripts) {
            Ok(debug) => {
                // Hooks on the debugger's thread, so its stops can be stepped from.
                scripts.set_debugging(true);
                self.script_debug = Some(debug);
            }
            Err(e) => {
                log::error!(target: "script", "the script debugger's fiber: {e}");
                self.script_debugging = false;
            }
        }
    }

    /// The stopped stage goes on once the editor said how (ADR 0119), with this frame's
    /// world and physics world; when it ends, the rest of its tick: the game's tick, the
    /// physics step and the host's end of the tick.
    fn go_on_with_scripts(&mut self, game: &mut impl Game) {
        let Some(debug) = &mut self.script_debug else {
            return;
        };
        let physics = game
            .physics()
            .or(self.own_physics.as_ref())
            .map_or(core::ptr::null(), core::ptr::from_ref);
        // SAFETY: this frame's world and physics world, alive while the stage runs here.
        let ended = unsafe { debug.go_on(&mut self.runtime.world, physics) };
        let Some(ended) = ended else {
            return;
        };
        // A finished tick's stops passed (Shift+F7): the next stops stop again.
        debug.debugger().skip_stops(false);
        if let crate::script_debug::StageKind::Tick { dt } = ended.kind
            && !ended.aborted
        {
            let world = &mut self.runtime.world;
            game.tick(world, dt);
            if let Some(own) = &mut self.own_physics
                && let Err(e) = own.step(world, dt)
            {
                say_once(&mut self.physics_error, &e.to_string());
            }
            if let Some(host) = self.net.as_deref_mut()
                && !host.is_client()
            {
                host.host_tick_end(world, Instant::now());
            }
        }
    }

    /// Loads script `file` (in the project's `scripts/`) made after the start, the editor's
    /// New Script: the names of the components it declares.
    ///
    /// # Errors
    ///
    /// Without a project, when the file cannot be read or does not run, or when it is
    /// loaded already.
    pub fn load_script(&mut self, file: &Path) -> Result<Vec<String>, String> {
        let Some(project) = self.settings.project.clone() else {
            return Err(String::from("scripts need a project folder"));
        };
        let folder = project.join("scripts");
        if self.scripts.is_none() {
            let world = &mut self.runtime.world;
            let settings = self.settings.script_settings();
            let scripts = crate::scripts::Scripts::load_with(world, &project, &folder, settings)
                .map_err(|e| e.to_string())?;
            self.scripts = Some(Box::new(scripts));
            // The project's first script: the host made now loads every file, this one too.
            let names: Vec<String> = self
                .scripts
                .as_ref()
                .map(|s| s.component_names().map(str::to_owned).collect())
                .unwrap_or_default();
            return Ok(names);
        }
        if self.script_held() {
            return Err(String::from("a script is stopped at a breakpoint"));
        }
        let Some(scripts) = self.scripts.as_deref_mut() else {
            return Err(String::from("no scripts"));
        };
        scripts.load_file(&mut self.runtime.world, &project, &folder, file)
    }

    /// Script `file` (in the project's `scripts/`) run again after it changed, its
    /// instances going on with their state: the editor's reload on save (ADR 0114 E7-4b,
    /// [`crate::scripts::Scripts::reload_file`]). A project's first script makes the host.
    ///
    /// # Errors
    ///
    /// Without a project, or when the file cannot be read or does not run: the old version
    /// then runs on.
    pub fn reload_script(&mut self, file: &Path) -> Result<crate::scripts::Reloaded, String> {
        let Some(project) = self.settings.project.clone() else {
            return Err(String::from("scripts need a project folder"));
        };
        let folder = project.join("scripts");
        if self.script_held() {
            return Err(String::from(
                "a script is stopped at a breakpoint: saved, it runs again once it goes on",
            ));
        }
        let Some(scripts) = self.scripts.as_deref_mut() else {
            let started = std::time::Instant::now();
            let added = self.load_script(file)?;
            let shown = file.strip_prefix(&project).unwrap_or(file);
            return Ok(crate::scripts::Reloaded {
                file: shown.to_string_lossy().replace('\\', "/"),
                added,
                took: started.elapsed(),
                ..crate::scripts::Reloaded::default()
            });
        };
        scripts.reload_file(&mut self.runtime.world, &project, &folder, file)
    }

    /// Gives `entity` script component `name` (a script's `component("Door", …)`), its
    /// props at their defaults: the editor's Add Component. False without that component
    /// or for a dead entity.
    pub fn add_script(&mut self, entity: Entity, name: &str) -> bool {
        if self.script_held() {
            return false;
        }
        match &self.scripts {
            Some(scripts) => scripts.add(&mut self.runtime.world, entity, name),
            None => false,
        }
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
            let mut renderer = UiRenderer::new(
                &self.device,
                &mut self.allocator,
                &self.bindless,
                self.output_format,
            )
            .map_err(AppError::at("the UI renderer"))?;
            renderer.set_white(white_of(output_encoding(&self.levels)));
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

    /// Opens a window of its own for the view (ADR 0107 E3-4: the editor plays the game
    /// there): `width` × `height` logical pixels at `placement` (where
    /// [`Self::view_window_placement`] found it before; `None`: centred on the primary
    /// monitor), titled `title`, the scene drawn into it from the next frame and the main
    /// output left to the overlays. Offscreen, an image of that size instead
    /// ([`Self::capture_view_window`]). Nothing when one is open.
    ///
    /// # Errors
    ///
    /// The window, its surface or swapchain cannot be made, or the main output draws in a
    /// way a second one cannot follow (frame generation, HDR10): the view stays in the
    /// main output then.
    pub fn open_view_window(
        &mut self,
        title: &str,
        width: u32,
        height: u32,
        placement: Option<engine_platform::window::WindowPlacement>,
    ) -> Result<(), AppError> {
        if self.view_window.is_some() {
            return Ok(());
        }
        let refuse = |why: &str| Err(AppError::message("the view's window", String::from(why)));
        if self.paced.is_some() || self.dlss.is_some() {
            return refuse("not beside frame generation");
        }
        if self.encode.is_some() {
            return refuse("not beside an HDR10 output");
        }
        // Latency control (Reflex, Anti-Lag) follows one swapchain: off from now on, and
        // the main swapchain made again without NVIDIA's low-latency mode (beside it, a
        // submission drawing into another swapchain's image crashed NVIDIA's driver).
        self.stop_low_latency()?;
        let output = match (&self.windows, &self.swapchain, &self.offscreen) {
            (Some(windows), Some(main), _) => {
                let handle = windows
                    .create_window(&WindowDesc {
                        title: title.to_owned(),
                        width: width.max(1),
                        height: height.max(1),
                        resizable: true,
                        visible: true,
                        placement,
                        ..WindowDesc::default()
                    })
                    .map_err(AppError::at("the view's window"))?;
                let made = (|| {
                    let surface = Arc::new(
                        Surface::from_win32(&self.instance, handle.hinstance, handle.hwnd)
                            .map_err(AppError::at("the view window's surface"))?,
                    );
                    let desc = SwapchainDesc {
                        width,
                        height,
                        present_mode: PresentMode::Fifo,
                        srgb: true,
                        color: main.color(),
                        ..SwapchainDesc::default()
                    };
                    let swapchain = Swapchain::new(&self.device, &surface, &desc)
                        .map_err(AppError::at("the view window's swapchain"))?;
                    if swapchain.format() != main.format() {
                        return Err(AppError::message(
                            "the view's window",
                            String::from("its format differs from the main output's"),
                        ));
                    }
                    let mut acquired = Vec::with_capacity(self.frames.in_flight());
                    for _ in 0..self.frames.in_flight() {
                        // SAFETY: valid create info; destroyed with the window.
                        let semaphore = unsafe {
                            self.device
                                .raw()
                                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                        }
                        .map_err(|e| {
                            AppError::message("the view window's semaphores", e.to_string())
                        })?;
                        acquired.push(semaphore);
                    }
                    Ok(ViewOutput::Window {
                        handle,
                        swapchain: Box::new(swapchain),
                        surface,
                        acquired,
                    })
                })();
                match made {
                    Ok(output) => output,
                    Err(e) => {
                        let _ = windows.destroy_window(handle.id);
                        return Err(e);
                    }
                }
            }
            (None, None, Some(_)) => {
                let mut offscreen = Offscreen {
                    width: width.max(1),
                    height: height.max(1),
                    pixel: 0,
                    buffer: vk::Buffer::null(),
                    memory: None,
                    requested: false,
                    captured: None,
                };
                offscreen.set_format(self.present_format, &mut self.allocator)?;
                ViewOutput::Offscreen(offscreen)
            }
            _ => return refuse("the player presents through no swapchain"),
        };
        self.view_window = Some(ViewWindow {
            output,
            shown: true,
            close_requested: false,
            focused: true,
            resized: false,
            minimized: false,
            cursor_locked: false,
        });
        Ok(())
    }

    /// Closes the view's window: the view draws into the main output again.
    pub fn close_view_window(&mut self) {
        let Some(view) = self.view_window.take() else {
            return;
        };
        let _ = self.wait_idle();
        match view.output {
            ViewOutput::Window {
                handle,
                swapchain,
                surface,
                acquired,
            } => {
                drop(swapchain);
                drop(surface);
                for semaphore in acquired {
                    // SAFETY: the device is idle: no acquisition or submission uses it.
                    unsafe { self.device.raw().destroy_semaphore(semaphore, None) };
                }
                if let Some(windows) = &self.windows {
                    let _ = windows.destroy_window(handle.id);
                }
            }
            ViewOutput::Offscreen(mut offscreen) => {
                if let Some(memory) = offscreen.memory.take() {
                    // SAFETY: the device is idle: no copy writes the buffer.
                    unsafe { self.allocator.destroy_buffer(offscreen.buffer, memory) };
                }
            }
        }
        // Latency control stays off: on NVIDIA's driver (617.42) the frame after Reflex was
        // turned back on crashed; the player's next start has it again.
    }

    /// Latency control off for the rest of the player's life, the main swapchain made
    /// again without NVIDIA's low-latency mode if it had it.
    fn stop_low_latency(&mut self) -> Result<(), AppError> {
        if self.latency.is_some() {
            self.wait_idle()?;
            self.latency = None;
        }
        if let (Some(swapchain), Some(surface)) = (&mut self.swapchain, &self.surface)
            && swapchain.low_latency()
        {
            self.frames
                .wait_idle()
                .map_err(AppError::at("waiting for the GPU"))?;
            let extent = swapchain.extent();
            swapchain
                .recreate_with_low_latency(
                    &self.device,
                    surface,
                    extent.width,
                    extent.height,
                    false,
                )
                .map_err(AppError::at("the swapchain"))?;
        }
        Ok(())
    }

    /// Whether the view draws into its window (`true`, as it opens) or into the main
    /// output again (the editor's eject), the window keeping its last frame.
    pub fn show_view_in_window(&mut self, shown: bool) {
        if let Some(view) = &mut self.view_window {
            view.shown = shown;
        }
    }

    /// Whether the view has a window of its own ([`Self::open_view_window`]).
    pub fn has_view_window(&self) -> bool {
        self.view_window.is_some()
    }

    /// Whether the view window's close button was pressed since it opened.
    pub fn view_window_close_requested(&self) -> bool {
        self.view_window.as_ref().is_some_and(|v| v.close_requested)
    }

    /// The view window's id, for telling its events from the main window's.
    pub fn view_window_id(&self) -> Option<engine_platform::window::WindowId> {
        match &self.view_window.as_ref()?.output {
            ViewOutput::Window { handle, .. } => Some(handle.id),
            ViewOutput::Offscreen(_) => None,
        }
    }

    /// The view window's size in pixels.
    pub fn view_window_extent(&self) -> Option<vk::Extent2D> {
        self.view_window.as_ref().map(|v| v.output.extent())
    }

    /// Renders a frame and returns the offscreen view window's pixels, as
    /// [`Self::capture_overlaid`] does the main output's.
    ///
    /// # Errors
    ///
    /// Without an offscreen view window, or as [`Self::frame_overlaid`].
    pub fn capture_view_window(
        &mut self,
        game: &mut impl Game,
        overlay: Option<&mut dyn Overlay>,
    ) -> Result<Option<Vec<u8>>, AppError> {
        let Some(o) = view_offscreen(&mut self.view_window) else {
            return Err(AppError::message(
                "capturing the view's window",
                String::from("only an offscreen view window captures"),
            ));
        };
        o.requested = true;
        o.captured = None;
        let rendered = self.frame_overlaid(game, overlay)?;
        let Some(o) = view_offscreen(&mut self.view_window) else {
            return Ok(None);
        };
        o.requested = false;
        let Some(number) = o.captured.take().filter(|_| rendered.is_some()) else {
            return Ok(None);
        };
        self.frames
            .wait_for_frame(number)
            .map_err(AppError::at("waiting for the captured frame"))?;
        let Some(o) = view_offscreen(&mut self.view_window) else {
            return Ok(None);
        };
        let bytes = o.bytes() as usize;
        let mapped = o
            .memory
            .as_ref()
            .and_then(Allocation::mapped_ptr)
            .ok_or_else(|| {
                AppError::message(
                    "capturing the view's window",
                    String::from("the readback is not mapped"),
                )
            })?;
        // SAFETY: the frame that wrote the buffer has finished; the mapping covers `bytes`.
        let pixels = unsafe { core::slice::from_raw_parts(mapped.as_ptr(), bytes) };
        Ok(Some(pixels.to_vec()))
    }

    /// Where the window is now, to open it there next time
    /// ([`PlayerSettings::window_placement`]); none offscreen.
    pub fn window_placement(&self) -> Option<engine_platform::window::WindowPlacement> {
        self.window
            .and_then(|w| engine_platform::window::window_placement(w.hwnd))
    }

    /// Where the view's own window is now ([`Self::open_view_window`]), to open it there
    /// next time; none without one, or offscreen.
    pub fn view_window_placement(&self) -> Option<engine_platform::window::WindowPlacement> {
        match &self.view_window.as_ref()?.output {
            ViewOutput::Window { handle, .. } => {
                engine_platform::window::window_placement(handle.hwnd)
            }
            ViewOutput::Offscreen(_) => None,
        }
    }

    /// Shows a window opened hidden ([`PlayerSettings::start_hidden`]) with the frame it
    /// last drew; with `front`, brings it to the front too (when Windows allows it).
    pub fn show_window(&self, front: bool) {
        if let (Some(windows), Some(window)) = (&self.windows, self.window) {
            let _ = windows.set_cloaked(window.id, false);
            if front {
                let _ = windows.bring_to_front(window.id);
            }
        }
    }

    /// The sky, its light, the atmosphere and fog (`engine_render::environment`): set them
    /// for the world (`set_sky`, `set_atmosphere`, `set_fog`, `set_panorama`).
    pub fn environment_mut(&mut self) -> &mut Environment {
        &mut self.environment
    }

    /// Runs one frame like [`Self::frame`] and returns its image once the GPU has finished
    /// it, rows top to bottom: 8-bit sRGB RGBA; for an HDR output
    /// ([`Self::assume_display`], [`Self::levels`]) scRGB as four little-endian half
    /// floats a pixel, or HDR10 as `A2B10G10R10` (PQ, BT.2020) in one little-endian `u32`.
    /// Offscreen players only ([`Window::Offscreen`]); `None` when the frame rendered
    /// nothing.
    ///
    /// # Errors
    ///
    /// As [`Self::frame`], and on a player with a window.
    pub fn capture(&mut self, game: &mut impl Game) -> Result<Option<Vec<u8>>, AppError> {
        self.capture_overlaid(game, None)
    }

    /// [`Self::capture`] of a frame with `overlay` drawn over it ([`Self::frame_overlaid`]).
    ///
    /// # Errors
    ///
    /// As [`Self::capture`].
    pub fn capture_overlaid(
        &mut self,
        game: &mut impl Game,
        overlay: Option<&mut dyn Overlay>,
    ) -> Result<Option<Vec<u8>>, AppError> {
        let Some(offscreen) = &mut self.offscreen else {
            return Err(AppError::message(
                "capturing a frame",
                String::from("only an offscreen player captures"),
            ));
        };
        offscreen.requested = true;
        offscreen.captured = None;
        let rendered = self.frame_overlaid(game, overlay)?;
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
    pub fn frame(&mut self, game: &mut impl Game) -> Result<Option<FrameReport>, AppError> {
        self.frame_overlaid(game, None)
    }

    /// The part of the output the scene is drawn into: `[x, y, width, height]` in pixels
    /// from the top-left corner (the editor's viewport panel), or `None` for all of it
    /// (the default). The camera's aspect follows it. The rest of the output is left to
    /// the overlay ([`Self::frame_overlaid`]).
    pub fn set_view_rect(&mut self, rect: Option<[u32; 4]>) {
        self.view_rect = rect;
    }

    /// Locks the cursor over the window where it is, hidden, the mouse's moves arriving as
    /// `WindowEventKind::CursorDelta` (`true`), or lets it go: the editor's camera drags.
    /// Windows lets it go when the window loses the keyboard.
    pub fn lock_cursor(&mut self, locked: bool) {
        if locked == self.cursor_locked {
            return;
        }
        self.cursor_locked = locked;
        if let (Some(windows), Some(window)) = (&self.windows, self.window) {
            let _ = windows.lock_cursor(window.id, locked);
        }
    }

    /// Whether the cursor is locked over the window ([`Self::lock_cursor`]).
    pub fn cursor_locked(&self) -> bool {
        self.cursor_locked
    }

    /// Locks the cursor over the view's own window ([`Self::open_view_window`]) as
    /// [`Self::lock_cursor`] does over the main one, or lets it go: a game playing there
    /// has the mouse. Windows lets it go when that window loses the keyboard.
    pub fn lock_view_cursor(&mut self, locked: bool) {
        let Some(view) = &mut self.view_window else {
            return;
        };
        if locked == view.cursor_locked {
            return;
        }
        view.cursor_locked = locked;
        if let (Some(windows), ViewOutput::Window { handle, .. }) = (&self.windows, &view.output) {
            let _ = windows.lock_cursor(handle.id, locked);
        }
    }

    /// The part of the output the scene is drawn into ([`Self::set_view_rect`]).
    pub fn view_rect(&self) -> Option<[u32; 4]> {
        self.view_rect
    }

    /// The format the frame is drawn in (the swapchain's, or the offscreen image's): what
    /// an overlay's pipelines write.
    pub fn output_format(&self) -> vk::Format {
        self.output_format
    }

    /// How frames reach the display (ADR 0038): SDR, or HDR at the display's SDR white
    /// level and peak.
    pub fn levels(&self) -> OutputLevels {
        self.levels
    }

    /// What overlays draw into: the format, UI white and levels ([`Overlay`]).
    pub fn output(&self) -> OverlayOutput {
        OverlayOutput {
            format: self.output_format,
            white: white_of(output_encoding(&self.levels)),
            levels: self.levels,
        }
    }

    /// The window's display as last reported (or as [`Self::assume_display`] set it).
    pub fn display(&self) -> Option<DisplayInfo> {
        self.display
    }

    /// The output while the display runs in HDR, from the next frame on (a game's HDR
    /// setting; [`PlayerSettings::hdr`]): scRGB, HDR10, or `OutputColor::Sdr` to stay in
    /// SDR.
    pub fn set_hdr(&mut self, hdr: OutputColor) {
        if self.settings.hdr != hdr {
            self.settings.hdr = hdr;
            self.display_changed = true;
        }
    }

    /// Draws for `display` from the next frame on, as if the window were on it (an
    /// offscreen player's HDR, for tests): the output follows it as it follows a window's
    /// display. A windowed player's next display report replaces it.
    pub fn assume_display(&mut self, display: DisplayInfo) {
        self.display = Some(display);
        self.display_changed = true;
    }

    /// [`Self::frame`], with `overlay` drawing over the finished frame before it is
    /// presented (the editor's panels, ADR 0107).
    ///
    /// # Errors
    ///
    /// As [`Self::frame`], and when the overlay's passes fail.
    #[allow(clippy::too_many_lines)]
    pub fn frame_overlaid(
        &mut self,
        game: &mut impl Game,
        mut overlay: Option<&mut dyn Overlay>,
    ) -> Result<Option<FrameReport>, AppError> {
        engine_profiler::zone!("app::Player::frame");
        let mut report = FrameReport::default();
        // The window's events: the game sees them all; the player keeps the size and
        // whether to close.
        {
            let (closing, resized, minimized, client, display, display_changed) = (
                &mut self.closing,
                &mut self.resized,
                &mut self.minimized,
                &mut self.client,
                &mut self.display,
                &mut self.display_changed,
            );
            let mut focus_lost = false;
            let view_id = match self.view_window.as_ref().map(|v| &v.output) {
                Some(ViewOutput::Window { handle, .. }) => Some(handle.id),
                _ => None,
            };
            let view = &mut self.view_window;
            if let Some(windows) = &mut self.windows {
                windows.drain_events(|event| {
                    // The view's window: its own size, focus and close button.
                    if Some(event.window) == view_id
                        && let Some(view) = view.as_mut()
                    {
                        match event.kind {
                            WindowEventKind::CloseRequested => view.close_requested = true,
                            WindowEventKind::Resized { width, height } => {
                                view.resized = true;
                                view.minimized = width == 0 || height == 0;
                            }
                            WindowEventKind::Minimized => view.minimized = true,
                            WindowEventKind::Focused(f) => {
                                view.focused = f;
                                focus_lost |= !f;
                            }
                            _ => {}
                        }
                        game.event(&event);
                        return;
                    }
                    match event.kind {
                        WindowEventKind::CloseRequested => *closing = true,
                        WindowEventKind::Resized { width, height } => {
                            *resized = true;
                            *minimized = width == 0 || height == 0;
                            *client = (width, height);
                        }
                        WindowEventKind::Minimized => *minimized = true,
                        WindowEventKind::Focused(false) => focus_lost = true,
                        WindowEventKind::Display(info) => {
                            *display = Some(info);
                            *display_changed = true;
                        }
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
        // VS Code's requests (ADR 0119 E9-3), before a stopped script goes on.
        self.debugger_frame();
        if self.minimized {
            // Nothing is presented while minimized: the next events are awaited.
            std::thread::yield_now();
            return Ok(None);
        }
        if core::mem::take(&mut self.resized) && self.needs_new_swapchain() {
            self.recreate_presentation()?;
            return Ok(None);
        }
        // The view's window resized: its swapchain made again at its new size.
        if self
            .view_window
            .as_ref()
            .is_some_and(|v| v.resized && !v.minimized)
        {
            self.wait_idle()?;
            if let Some(view) = &mut self.view_window
                && let ViewOutput::Window {
                    swapchain, surface, ..
                } = &mut view.output
            {
                view.resized = false;
                let extent = swapchain.extent();
                swapchain
                    .recreate(&self.device, surface, extent.width, extent.height)
                    .map_err(AppError::at("the view window's swapchain"))?;
            }
        }
        // The display changed (HDR on or off, the SDR white level, another monitor): the
        // output follows it before this frame is drawn (ADR 0038).
        if core::mem::take(&mut self.display_changed) {
            self.follow_display()?;
        }
        if self.overlay_generation != self.output_generation
            && let Some(overlay) = overlay.as_mut()
        {
            self.wait_idle()?;
            let output = self.output();
            overlay.output_changed(&mut self.setup(), &output)?;
            self.overlay_generation = self.output_generation;
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
        // The view's window: its image, when the view draws there this frame.
        let view_image = match self.view_window.as_mut() {
            Some(view) if view.shown && !view.minimized => match &mut view.output {
                ViewOutput::Window {
                    swapchain,
                    acquired,
                    ..
                } => {
                    let semaphore = acquired[frame.slot() % acquired.len()];
                    match swapchain.acquire(semaphore, u64::MAX) {
                        Ok(a) => Some(ViewImage::Window {
                            index: a.index,
                            acquired: semaphore,
                        }),
                        Err(e) if e.is_out_of_date() => {
                            view.resized = true;
                            None
                        }
                        Err(e) => return Err(AppError::at("the view window's image")(e)),
                    }
                }
                ViewOutput::Offscreen(_) => Some(ViewImage::Offscreen),
            },
            _ => None,
        };
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
        let away = self
            .view_window
            .as_ref()
            .is_some_and(|v| v.shown && !v.focused);
        let sample = match &mut self.input {
            Some(devices) => {
                if let Some(input) = self.runtime.world.resource_mut::<engine_input::Input>() {
                    devices.feed(input);
                    // The view plays in its window: what is typed elsewhere is not the
                    // game's.
                    if away {
                        input.release_all();
                    }
                }
                true
            }
            None => false,
        };
        // A stage stopped halfway (ADR 0119) goes on first, once the editor said how; while
        // one is stopped the world is kept still: no ticks, no scene following.
        self.ready_script_debug();
        self.go_on_with_scripts(game);
        let held_at_start = self.script_held();
        // The scene's parts the game's physics follows (ADR 0117), before the ticks.
        if (self.simulating || self.steps > 0) && !held_at_start {
            let mut scene = crate::SceneParts {
                world: &mut self.runtime.world,
                assets: &mut self.assets,
                blockout: &self.blockout,
            };
            match game.before_ticks(&mut scene) {
                Ok(()) => self.scene_error = None,
                Err(e) => {
                    let said = e.to_string();
                    if self.scene_error.as_deref() != Some(said.as_str()) {
                        log::warn!(target: "engine_app::player", "the game's scene: {said}");
                        self.scene_error = Some(said);
                    }
                }
            }
            // The player's own physics follows the scene too (ADR 0114 E7-3).
            if let Some(own) = &mut self.own_physics {
                let mut scene = crate::SceneParts {
                    world: &mut self.runtime.world,
                    assets: &mut self.assets,
                    blockout: &self.blockout,
                };
                if let Err(e) = own.sync_scene(&mut scene) {
                    say_once(&mut self.physics_error, &e.to_string());
                }
            }
        }
        // Ticks, presentation, interpolation (ADR 0064).
        // The network session's requests (host, join, leave) at the frame's start (ADR 0106).
        if let Some(net) = &mut self.net {
            net.begin_frame(&mut self.runtime.world, Instant::now());
        }
        let joined = self.net.as_ref().is_some_and(|n| n.is_client());
        let simulating = self.simulating;
        let mut steps = self.steps;
        // A pause point reached in a tick (ADR 0119), here or in the stage that went on: the
        // frame's other ticks wait.
        let paused_by_script = Cell::new(
            !held_at_start
                && self
                    .scripts
                    .as_deref()
                    .is_some_and(crate::scripts::Scripts::debug_pause_asked),
        );
        // A stage stopped halfway: the world kept still until it goes on.
        let held = Cell::new(held_at_start);
        // The ticks that simulated (the runtime also counts the steps a still world skips).
        let simulated = Cell::new(0u32);
        let stages = {
            let game_cell = RefCell::new(&mut *game);
            // Scripts run in both stages, before the game's own hooks (ADR 0114 §7), only
            // while the world simulates.
            let scripts_cell = RefCell::new(self.scripts.as_mut());
            // The player's own physics world, stepped after each tick's game hook and
            // interpolated in the presentation; scripts query the game's, else this one.
            let own_physics = RefCell::new(self.own_physics.as_mut());
            // The debugger's stage (ADR 0119): the scripts' stages run on its fiber, but a
            // joined client's, whose ticks its prediction drives.
            let debug = RefCell::new(if joined {
                None
            } else {
                self.script_debug.as_mut()
            });
            let physics_error = &mut self.physics_error;
            let frame_dt = elapsed as f32;
            let timing = &mut self.timing;
            let mut net = self.net.as_deref_mut();
            let assets = &mut self.assets;
            let blockout = &mut self.blockout;
            let geometry = &mut self.geometry;
            let materials = &mut self.materials;
            let skinning = &mut self.skinning;
            let frame_number = frame.number;
            let wait_for_assets = self.wait_for_assets;
            // Whether the scene was extracted this frame before its presentation (every
            // tick extracts it; a joined client's frame always does): what renderers let
            // go of is drawn until then.
            let extracted = Cell::new(joined);
            let present = |world: &mut World, (): &(), alpha: f32| {
                if sample && let Some(input) = world.resource_mut::<engine_input::Input>() {
                    input.sample_frame(Instant::now());
                }
                if !held.get()
                    && let Some(own) = own_physics.borrow_mut().as_mut()
                {
                    own.interpolate(world, alpha);
                }
                if simulating
                    && !held.get()
                    && let Some(scripts) = scripts_cell.borrow_mut().as_mut()
                {
                    let game = game_cell.borrow();
                    let own = own_physics.borrow();
                    let physics = game.physics().or(own.as_deref());
                    match debug.borrow_mut().as_mut() {
                        Some(debug) => {
                            if scripts.has_frame_hooks() {
                                let kind = crate::script_debug::StageKind::Frame {
                                    dt: frame_dt,
                                    alpha,
                                };
                                let physics =
                                    physics.map_or(core::ptr::null(), core::ptr::from_ref);
                                let host: *mut crate::scripts::Scripts = &mut ***scripts;
                                // SAFETY: the host, this frame's world and physics world; while
                                // the stage is held the player hands out no host and opens the
                                // world and the physics world anew before it goes on.
                                held.set(unsafe { debug.run(host, world, physics, kind) });
                            }
                        }
                        None => scripts.frame_with(world, frame_dt, alpha, physics),
                    }
                }
                // As while paused: a game in the editor draws the editor's frame here.
                game_cell.borrow_mut().present(world, alpha);
                // Models (ADR 0108): their meshes and materials reach the GPU this frame,
                // the pools beginning theirs after the simulation.
                assets.sync(
                    world,
                    geometry,
                    materials,
                    skinning,
                    frame_number,
                    wait_for_assets,
                    extracted.get(),
                );
                // Brushes (ADR 0110): their groups evaluated again where they changed.
                blockout.sync(world, geometry, materials, frame_number, extracted.get());
            };
            match net.as_mut() {
                // Joined: the client runs the frame's ticks (its predictions, and past ones
                // again after a correction), the game's tick as its simulation.
                Some(client) if joined => {
                    let world = &mut self.runtime.world;
                    if sample && let Some(input) = world.resource_mut::<engine_input::Input>() {
                        input.sample_tick(Instant::now());
                    }
                    game_cell.borrow_mut().net_input(world);
                    // Timers move once a frame here: re-simulated ticks must not move them.
                    if simulating {
                        timing.advance(world, elapsed as f32);
                    }
                    self.runtime.frame_driven(
                        &(),
                        &mut self.scene,
                        |world, (), dt| {
                            // Scripts predict the client's own entities (ADR 0114 §8).
                            if simulating && let Some(scripts) = scripts_cell.borrow_mut().as_mut()
                            {
                                scripts.tick_predicted(world, dt);
                            }
                            game_cell.borrow_mut().tick(world, dt);
                        },
                        |world, step| client.client_frame(world, Instant::now(), step),
                        present,
                    )
                }
                _ => self.runtime.frame(
                    elapsed,
                    &(),
                    &mut self.scene,
                    |world, (), dt| {
                        extracted.set(true);
                        if sample && let Some(input) = world.resource_mut::<engine_input::Input>() {
                            input.sample_tick(Instant::now());
                        }
                        // Kept still: the tick only places and extracts what is there, but
                        // for a step asked for (`Player::step_tick`).
                        if paused_by_script.get() || held.get() {
                            return;
                        }
                        if !simulating {
                            if steps == 0 {
                                return;
                            }
                            steps -= 1;
                        }
                        simulated.set(simulated.get() + 1);
                        // Hosting: the guests' inputs in, the host's input into its pawn.
                        if let Some(host) = net.as_mut() {
                            game_cell.borrow_mut().net_input(world);
                            host.host_tick_start(world, Instant::now());
                        }
                        timing.advance(world, dt);
                        if let Some(scripts) = scripts_cell.borrow_mut().as_mut() {
                            let game = game_cell.borrow();
                            let own = own_physics.borrow();
                            let physics = game.physics().or(own.as_deref());
                            match debug.borrow_mut().as_mut() {
                                Some(debug) => {
                                    let kind = crate::script_debug::StageKind::Tick { dt };
                                    let physics =
                                        physics.map_or(core::ptr::null(), core::ptr::from_ref);
                                    let host: *mut crate::scripts::Scripts = &mut ***scripts;
                                    // SAFETY: as the frame's stage above.
                                    if unsafe { debug.run(host, world, physics, kind) } {
                                        // Stopped halfway: the rest of this tick runs when
                                        // it goes on, the frame's other ticks not at all.
                                        held.set(true);
                                        return;
                                    }
                                }
                                None => scripts.tick_with(world, dt, physics),
                            }
                            // The tick goes on to its end; the next ones wait.
                            if scripts.debug_pause_asked() {
                                paused_by_script.set(true);
                            }
                        }
                        game_cell.borrow_mut().tick(world, dt);
                        if let Some(own) = own_physics.borrow_mut().as_mut()
                            && let Err(e) = own.step(world, dt)
                        {
                            say_once(physics_error, &e.to_string());
                        }
                        // Hosting: the pawns given, the events, the snapshots out.
                        if let Some(host) = net.as_mut() {
                            host.host_tick_end(world, Instant::now());
                        }
                    },
                    present,
                ),
            }
        };
        self.steps = steps;
        // A pause point reached in a tick or in the frame's hooks: the world kept still
        // from now on, and the editor told where.
        if !self.script_held()
            && let Some(at) = self
                .scripts
                .as_deref_mut()
                .and_then(|s| s.take_debug_pause())
        {
            self.simulating = false;
            self.steps = 0;
            self.debug_pause = Some(at);
        }
        report.ticks = stages.ticks;
        self.ticks_run += u64::from(simulated.get());
        report.simulate = stages.simulate_us;
        report.propagate = stages.propagate_us;
        report.extract = stages.extract_us;
        report.interpolate = stages.interpolate_us + stages.present_us;
        // The brushes' grid texture, once a brush is drawn.
        self.blockout.make_grid(
            &self.device,
            &mut self.allocator,
            &mut self.textures,
            &mut self.materials,
        );
        // The textures of glTF materials decoded by now (ADR 0108).
        self.assets.make_textures(
            &self.device,
            &mut self.allocator,
            &mut self.textures,
            &mut self.materials,
            (&self.bindless, frame.number),
        );
        // Typed material files of their type once their layouts are reflected, off this
        // thread (ADR 0120's E10-1b).
        self.assets
            .make_material_types(self.renderer.forward_mut(), &mut self.materials);
        self.wait_for_assets = false;
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
        // The main camera: the view, and the ears where no entity hears.
        let main = self.cameras.resolve(&self.runtime.world);
        // Sound: the sources' voices, the listener, the world's acoustics (ADR 0105).
        let t = Instant::now();
        self.sound.update(
            &mut self.runtime.world,
            main.as_ref().map(|m| &m.camera),
            game.acoustics().or(self
                .own_physics
                .as_ref()
                .map(|p| p.backend() as &dyn engine_physics::PhysicsBackend)),
            elapsed,
            self.simulating,
        );
        report.sound = us(t);
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
            &mut self.sound,
        );
        marker(
            &mut self.latency,
            &self.swapchain,
            &self.paced,
            Marker::SimulationEnd,
        );

        // The main camera, or one at the origin.
        let (camera, mut view_settings) = match main {
            Some(main) => (main.camera, main.settings),
            None => (
                ViewCamera::from_rigid(Quat::IDENTITY, Vec3::ZERO, 1.2, 0.1, 1000.0),
                *self.cameras.base_mut(),
            ),
        };
        let extent = self.extent();
        // The scene's part of the output (`set_view_rect`): the whole of it by default; all
        // of its own window's while it draws there.
        let view_rect = self.view_rect.map(|r| clamp_rect(r, extent));
        let view_extent = match (&view_image, &self.view_window) {
            (Some(_), Some(view)) => view.output.extent(),
            _ => view_rect.map_or(extent, |r| r.extent),
        };
        let aspect = view_extent.width as f32 / view_extent.height.max(1) as f32;
        let clip_from_world = camera.clip_from_world(aspect);

        // Streaming: mesh levels by distance in every direction (a fast turn finds them
        // loaded), textures by what the views sampled.
        let t = Instant::now();
        if let Some(streamer) = &mut self.streamer {
            let eye = camera.position;
            let lod_view = LodView::new(&clip_from_world, eye, view_extent.height, 1.0).around();
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
        // Material types' pipelines on their way before the view's prepare takes what
        // arrived: a layout taken there was not this frame's materials' (they were laid
        // out above), so a preview waits for it (ADR 0123's E10-3f-2).
        let types_pending = self.renderer.forward().material_types_pending();
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
                (view_extent.width, view_extent.height),
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
        // A preview asked for, once its parts are made (ADR 0112): one a frame.
        self.previews.collect(frame.number);
        let preview = self.previews.pick(
            &mut self.assets,
            &mut self.geometry,
            &mut self.materials,
            &mut self.allocator,
            frame.number,
            types_pending,
        );
        // The editor's live model preview (ADR 0124's E12-1b), when its pose or turn changed.
        let live = self.previews.pick_live(
            &mut self.assets,
            &mut self.materials,
            &self.geometry,
            &mut self.allocator,
            frame.number,
        );
        // A graph's node previews (ADR 0123's E10-4g): one atlas a frame.
        let node_previews = self.previews.pick_nodes(
            &mut self.assets,
            &mut self.materials,
            &mut self.allocator,
            types_pending,
        );
        if preview.is_some() {
            self.previews.prepare(
                &mut self.renderer,
                PrepareContext {
                    device: &self.device,
                    allocator: &mut self.allocator,
                    bindless: &self.bindless,
                    materials: &mut self.materials,
                    frame_number: frame.number,
                    skinning: None,
                },
            )?;
        }
        if live.is_some() {
            self.previews.prepare_live(
                &mut self.renderer,
                PrepareContext {
                    device: &self.device,
                    allocator: &mut self.allocator,
                    bindless: &self.bindless,
                    materials: &mut self.materials,
                    frame_number: frame.number,
                    skinning: None,
                },
            )?;
        }
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
                            TextureDesc::new_2d(self.present_format, extent.width, extent.height),
                        )),
                    ),
                    (None, None, None) => {
                        return Err(AppError::message(
                            "the frame",
                            String::from("nothing presents"),
                        ));
                    }
                };
            let output = match (&present, &generated, offscreen_target) {
                (Some(present), _, _) => present.texture,
                (None, Some((real, _)), _) => *real,
                (None, None, Some(image)) => image,
                (None, None, None) => unreachable!("one of them presents"),
            };
            // HDR10: the frame is drawn linear in an intermediate, encoded into the output
            // after the UI and the overlay (ADR 0038).
            let target = match &self.encode {
                Some(_) => fg.create_texture(
                    "hdr10 frame",
                    TextureDesc::new_2d(HDR_FORMAT, extent.width, extent.height),
                ),
                None => output,
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
            // The view's own window: its image (or offscreen, one of its own), the whole of
            // it; the main output then holds what the overlays draw.
            let view_present = match (&view_image, self.view_window.as_ref().map(|v| &v.output)) {
                (
                    Some(ViewImage::Window { index, .. }),
                    Some(ViewOutput::Window { swapchain, .. }),
                ) => Some(PresentTarget::import(&mut fg, swapchain, *index)),
                _ => None,
            };
            let view_texture = match (&view_image, &view_present) {
                (Some(ViewImage::Window { .. }), Some(present)) => Some(present.texture),
                (Some(ViewImage::Offscreen), _) => {
                    let e = view_extent;
                    Some(fg.create_texture(
                        "view window",
                        TextureDesc::new_2d(self.present_format, e.width, e.height),
                    ))
                }
                _ => None,
            };
            let scene_target = view_texture.unwrap_or(target);
            // The view draws into its window: the main output cleared, for the overlays to
            // draw over (and for a copy to the swapchain to read something written).
            if view_texture.is_some() {
                fg.add_pass("main output clear")
                    .write(target, Access::TransferWrite)
                    .execute(move |ctx| {
                        let range = vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        };
                        // SAFETY: recording; the graph made the output a transfer
                        // destination.
                        unsafe {
                            ctx.device.cmd_clear_color_image(
                                ctx.cmd,
                                ctx.image(target),
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                                &vk::ClearColorValue::default(),
                                &[range],
                            );
                        }
                    });
            }
            let view_target = ViewTarget {
                texture: scene_target,
                rect: if view_texture.is_some() {
                    None
                } else {
                    view_rect
                },
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
            if let Some(drawing) = node_previews {
                self.previews.declare_nodes(
                    &mut fg,
                    context.ring,
                    &self.device,
                    &self.bindless,
                    self.renderer.forward(),
                    &mats,
                    drawing,
                    frame.number,
                );
            }
            if preview.is_some() || live.is_some() {
                self.previews.declare(
                    &mut fg,
                    &mut context,
                    &self.renderer,
                    &geo,
                    &mats,
                    preview.as_ref(),
                    live.as_ref(),
                )?;
            }
            // The UI: on the real frame and on a generated one shown before it; with DLSS
            // frame generation alone into a layer of its own, cleared to nothing
            // (Streamline composites it over the frames it generates), then over the
            // copied frame.
            let ui_target = (self.dlss.is_some() && !layer.is_empty()).then(|| {
                let texture = fg.create_texture(
                    "ui layer",
                    TextureDesc::new_2d(self.output_format, extent.width, extent.height),
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
                        ui_target.unwrap_or(scene_target),
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
            if let Some(overlay) = overlay.as_mut() {
                overlay.record(&mut OverlayPass {
                    fg: &mut fg,
                    ring: &mut self.ring,
                    bindless: &self.bindless,
                    target: ui_target.unwrap_or(target),
                    extent,
                    view: Some(OverlayView {
                        frame: &view_frame,
                        camera: &camera,
                        rect: if view_texture.is_some() {
                            None
                        } else {
                            view_rect
                        },
                        geometry: &geo,
                        materials: &mats,
                        scene: scene_frame.as_ref(),
                        frame_number: frame.number,
                    }),
                })?;
            }
            if let Some(encode) = &self.encode {
                encode.add_composite_pass(
                    &mut fg,
                    &self.bindless,
                    target,
                    output,
                    Composite {
                        tonemap: false,
                        encoding: self.levels.swapchain_encoding(),
                        ..Composite::default()
                    },
                );
            }
            let backbuffer = present.as_ref().map(PresentTarget::swapchain_texture);
            if let Some(present) = present {
                present.finish(&mut fg);
            }
            // The view's window: its image ready to present, or read back offscreen.
            if let Some(view_present) = view_present {
                view_present.finish(&mut fg);
            }
            if let (Some(ViewImage::Offscreen), Some(texture), Some(view)) =
                (&view_image, view_texture, self.view_window.as_mut())
                && let ViewOutput::Offscreen(offscreen) = &mut view.output
            {
                offscreen.add_pass(&mut fg, texture, frame.number);
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
            if let (Some(_), Some(offscreen)) = (offscreen_target, &mut self.offscreen) {
                let image = output;
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
        // A preview drawn with its types' own pipelines (none asked for by this frame) is
        // answered as it is.
        self.previews
            .recorded(self.renderer.forward().material_types_pending());

        // Submission and presentation.
        let t = Instant::now();
        let number = frame.number;
        let submit_id = self.latency.as_ref().and_then(Latency::submit_present_id);
        match (&self.swapchain, paced_images) {
            (Some(swapchain), _) => {
                let index = acquired.map_or(0, |a| a.index);
                let second = match (&view_image, self.view_window.as_ref().map(|v| &v.output)) {
                    (
                        Some(ViewImage::Window { index, acquired }),
                        Some(ViewOutput::Window { swapchain, .. }),
                    ) => Some((*acquired, swapchain.render_finished(*index))),
                    _ => None,
                };
                self.frames
                    .submit_presenting(
                        self.device.graphics,
                        frame,
                        true,
                        None,
                        Some(swapchain.render_finished(index)),
                        second,
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
        if let (Some(ViewImage::Window { index, .. }), Some(view)) =
            (&view_image, self.view_window.as_mut())
            && let ViewOutput::Window { swapchain, .. } = &view.output
        {
            match swapchain.present(self.device.graphics, *index) {
                Ok(suboptimal) => view.resized |= suboptimal,
                Err(e) if e.is_out_of_date() => view.resized = true,
                Err(e) => return Err(AppError::at("presenting the view's window")(e)),
            }
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

    /// The time each frame advances by, when it is fixed ([`Self::set_frame_time`]).
    pub fn frame_time(&self) -> Option<f64> {
        self.frame_time
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
        self.rebuild_presentation(None)
    }

    /// [`Self::recreate_presentation`], for the output `wanted` when given (HDR turned on
    /// or off): the swapchain's color, and the display's metadata stated before a paced
    /// presenter takes the swapchain.
    fn rebuild_presentation(&mut self, wanted: Option<OutputLevels>) -> Result<(), AppError> {
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
            match wanted {
                Some(levels) => {
                    swapchain
                        .recreate_with_color(
                            &self.device,
                            surface,
                            extent.width,
                            extent.height,
                            levels.color,
                        )
                        .map_err(AppError::at("the swapchain"))?;
                    // Frame generation draws the output itself: not HDR10 (see `new`).
                    if self.settings.frame_generation != FrameGeneration::Off
                        && swapchain.color() == OutputColor::Hdr10
                    {
                        swapchain
                            .recreate_with_color(
                                &self.device,
                                surface,
                                extent.width,
                                extent.height,
                                OutputColor::Sdr,
                            )
                            .map_err(AppError::at("the swapchain"))?;
                    }
                    if swapchain.color().is_hdr() {
                        swapchain.set_hdr_metadata(&hdr_metadata(&levels, self.display.as_ref()));
                    }
                }
                None => swapchain
                    .recreate(&self.device, surface, extent.width, extent.height)
                    .map_err(AppError::at("the swapchain"))?,
            }
            self.present_format = swapchain.format();
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

    /// Makes the output follow the display (ADR 0038): the swapchain made again when HDR
    /// turned on or off, the levels (white, peak) applied either way.
    fn follow_display(&mut self) -> Result<(), AppError> {
        let wanted = wanted_levels(
            self.settings.hdr,
            self.settings.frame_generation,
            self.display.as_ref(),
        );
        let color = if self.offscreen.is_some() {
            wanted.color
        } else {
            if wanted.color != color_of(self.present_format) && self.surface.is_some() {
                self.rebuild_presentation(Some(wanted))?;
            }
            color_of(self.present_format)
        };
        self.apply_levels(actual_levels(color, wanted))?;
        // Another monitor may run at another refresh rate.
        self.follow_frame_cap()
    }

    /// Applies `levels` (ADR 0038): the formats drawn in and presented (the tone mapper,
    /// the UI and the offscreen readback made for them), the views' encoding and peak, UI
    /// white, HDR10's encode pass, the display's metadata. Overlays learn of a change
    /// before their next frame.
    fn apply_levels(&mut self, levels: OutputLevels) -> Result<(), AppError> {
        let present = match &self.offscreen {
            Some(_) => offscreen_format(levels.color),
            None => self.present_format,
        };
        let output = output_format_for(present, &levels);
        let encoding = output_encoding(&levels);
        let present_changed = present != self.present_format;
        if output != self.output_format || present_changed {
            self.wait_idle()?;
            // SAFETY: the device is idle: nothing uses the old pipelines any more.
            unsafe {
                self.renderer.set_output_format(
                    &self.device,
                    &mut self.allocator,
                    &self.bindless,
                    output,
                )
            }
            .map_err(AppError::at("the tone mapper"))?;
            if let Some(mut ui) = self.ui.take() {
                // SAFETY: the device is idle.
                unsafe { ui.renderer.destroy(&mut self.allocator) };
                let renderer =
                    UiRenderer::new(&self.device, &mut self.allocator, &self.bindless, output)
                        .map_err(AppError::at("the UI renderer"))?;
                self.ui = Some(UiLayer {
                    renderer,
                    canvases: ui.canvases,
                });
            }
            if let Some(offscreen) = &mut self.offscreen {
                offscreen.set_format(present, &mut self.allocator)?;
            }
            self.present_format = present;
            self.output_format = output;
        }
        self.encode = match (levels.color, self.encode.take()) {
            (OutputColor::Hdr10, Some(encode)) if !present_changed => Some(encode),
            (OutputColor::Hdr10, _) => Some(
                Tonemap::new(&self.device, &self.bindless, present)
                    .map_err(AppError::at("the HDR10 encode pass"))?,
            ),
            _ => None,
        };
        if let Some(ui) = &mut self.ui {
            ui.renderer.set_white(white_of(encoding));
        }
        let base = self.cameras.base_mut();
        base.peak = levels.peak();
        base.encoding = encoding;
        if levels.color.is_hdr()
            && let Some(swapchain) = &mut self.swapchain
        {
            swapchain.set_hdr_metadata(&hdr_metadata(&levels, self.display.as_ref()));
        }
        if levels != self.levels {
            log::info!(
                target: "engine_app::present",
                "output: {:?}, reference white {:.0} nits, peak {:.0} nits",
                levels.color,
                levels.white_nits,
                levels.peak_nits
            );
            self.output_generation += 1;
        }
        self.levels = levels;
        Ok(())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // A stage stopped halfway ends before the host and the world go (ADR 0119).
        self.end_script_stop();
        self.script_debug = None;
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
            self.previews.destroy(&mut self.allocator, &self.bindless);
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
                self.sound.live(),
            );
            self.sound.stop_all();
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
        self.close_view_window();
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
    (video_decode, pipeline_statistics): (bool, bool),
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
    // Validation as the settings ask, else as `ENGINE_VALIDATION` does (`0`, `1`, `sync`),
    // else in debug builds of `engine_rhi` (`InstanceDesc::default`; games made by
    // `ofeliaengine new` build it without, ADR 0107).
    let (validate, sync) = match (
        validation,
        std::env::var("ENGINE_VALIDATION").ok().as_deref(),
    ) {
        (Some(v), _) => (v, v),
        (None, Some("0")) => (false, false),
        (None, Some("sync")) => (true, true),
        (None, Some(_)) => (true, false),
        (None, None) => (InstanceDesc::default().validation, false),
    };
    let instance = Instance::with_extensions(
        &InstanceDesc {
            app_name: app_name.to_owned(),
            validation: validate,
            sync_validation: sync,
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
            pipeline_statistics,
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
        // The world is halfway through a tick while a stage is stopped: saves wait.
        if self.script_held() {
            return;
        }
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
        // Scripts' timers ride on their entities into the save (ADR 0114 §8).
        if let Some(scripts) = self.scripts.as_deref_mut() {
            scripts.before_save(&mut self.runtime.world);
        }
        let world = &self.runtime.world;
        let options = engine_save::CaptureOptions {
            level: Some(&self.saves.level_ids),
            ..engine_save::CaptureOptions::default()
        };
        let mut image = engine_save::capture(world, &options);
        if let Some(scripts) = self.scripts.as_deref_mut() {
            scripts.after_save(&mut self.runtime.world);
        }
        let world = &self.runtime.world;
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
        // Every sound starts again where the save had it (ADR 0105).
        self.sound.after_load(world);
        // Scripts' timers set again, their `on_load` at the next tick (ADR 0114 §8).
        if let Some(scripts) = self.scripts.as_deref_mut() {
            scripts.after_load(world);
        }
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

/// The assets of `settings`' project, with its derived data cache when it gives one (a
/// cache that cannot be opened leaves textures uncompressed, said in the log).
fn assets(settings: &PlayerSettings) -> crate::Assets {
    let mut assets = crate::Assets::new(settings.project.clone().unwrap_or_default());
    if let Some(dir) = &settings.derived_data {
        match engine_assets::DerivedDataCache::new(dir) {
            Ok(cache) => assets.set_derived_data(Some(cache)),
            Err(e) => log::warn!(
                target: "engine_app::assets",
                "the derived data cache {}: {e}; textures stay uncompressed",
                dir.display()
            ),
        }
    }
    assets
}

/// `said` in the log as a warning about the player's physics, unless it was the last one
/// (`last` keeps it).
fn say_once(last: &mut Option<String>, said: &str) {
    if last.as_deref() != Some(said) {
        log::warn!(target: "engine_app::player", "the player's physics: {said}");
        *last = Some(said.to_owned());
    }
}

/// Where the mixer's layout file is: `AudioSettings::layout` under the project.
fn layout_path(settings: &PlayerSettings) -> PathBuf {
    settings
        .project
        .as_deref()
        .unwrap_or(Path::new(""))
        .join(&settings.audio.layout)
}

/// The mixer's layout from the file at `path` (ADR 0121): the built-in buses without one,
/// and when it cannot be read (said in the log).
pub(crate) fn load_layout(path: &Path) -> crate::MixerFile {
    if !path.is_file() {
        return crate::MixerFile::builtin();
    }
    crate::MixerFile::load(path).unwrap_or_else(|e| {
        log::warn!(target: "engine_app::sound", "{e}: the built-in buses play");
        crate::MixerFile::builtin()
    })
}
