//! What a game asks of its player: the window, presentation, latency and rendering.

use engine_frame::{Antialiasing, FrameGeneration, Quality, RenderDefaults};
use engine_latency::LowLatency;
use engine_render::ao::AoSettings;
use engine_render::forward::{ContactShadowSettings, LocalShadowSettings};
use engine_render::gi::GiSettings;
use engine_render::reflections::{ProbeSettings, SsrSettings};
use engine_rhi::{AdapterPreference, OutputColor};
use std::path::PathBuf;

/// How the game's window shows (ADR 0099).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// A framed window of `width` × `height` logical pixels, fit into its monitor's work
    /// area.
    Windowed {
        /// Client width in logical (96-DPI) pixels.
        width: u32,
        /// Client height in logical (96-DPI) pixels.
        height: u32,
    },
    /// Borderless over the monitor, the swapchain stating that the driver must not take
    /// the monitor for itself: the lowest latency measured (ADR 0099).
    FullScreen,
    /// Borderless over the monitor, the swapchain asking for the monitor
    /// (`VK_EXT_full_screen_exclusive`); refused, it presents as [`Self::FullScreen`].
    Exclusive,
    /// No window: each frame is drawn into an image of `width` × `height` pixels (8-bit
    /// sRGB) that [`Player::capture`](crate::Player::capture) reads back, and nothing is
    /// presented: tests, benchmarks, renders without a screen. No latency control and no
    /// frame generation (they need a swapchain).
    Offscreen {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
    },
}

/// Everything [`Player::new`](crate::Player::new) needs.
#[derive(Debug, Clone)]
pub struct PlayerSettings {
    /// The window's title, and the name of the folder in `%LOCALAPPDATA%` that holds the
    /// pipeline cache.
    pub title: String,
    /// The scene's pack (`ofeliaengine cook`), streamed within the GPU memory budget
    /// (ADR 0076); `None`: an empty scene.
    pub pack: Option<PathBuf>,
    /// A scene file (ADR 0108, what the editor saves) loaded into the world after the
    /// pack, relative to [`Self::project`]: its entities, and the meshes and materials its
    /// `Model`s name (`Player::load_scene`). `None`: none.
    pub scene: Option<PathBuf>,
    /// The game's folder: scene and asset paths start from it. `None`: the working folder.
    pub project: Option<PathBuf>,
    /// The derived data cache's folder (ADR 0041, ADR 0108's E4-8): the textures of glTF
    /// files the player makes are compressed into it in the background (BC7, BC5 for
    /// normal maps, as the cooker does) and swapped in once compressed; a later run loads
    /// them from it compressed. `None`: they stay uncompressed (RGBA8). The editor gives
    /// its project's `.ofelia/ddc`.
    pub derived_data: Option<PathBuf>,
    /// The window.
    pub window: Window,
    /// The monitor (an `HMONITOR`, `engine_platform::display::Monitor::handle`); `None`:
    /// the primary one.
    pub monitor: Option<isize>,
    /// Where a windowed window opens, as `Player::window_placement` read it earlier (the
    /// editor keeps its own); `None`: centred on its monitor.
    pub window_placement: Option<engine_platform::window::WindowPlacement>,
    /// The window opens hidden (cloaked by DWM, still drawn) until `Player::show_window`:
    /// its first frame appears whole, over the window it takes over from.
    pub start_hidden: bool,
    /// The GPU: `ENGINE_ADAPTER` if set, else this.
    pub adapter: AdapterPreference,
    /// The validation layer: `None` as the build has it (debug builds validate), `Some(true)`
    /// with synchronization validation too (tests), `Some(false)` never.
    pub validation: Option<bool>,
    /// Wait for the display's refresh (FIFO) instead of presenting at once.
    pub vsync: bool,
    /// The output while Windows runs the window's display in HDR (ADR 0038): scRGB (the
    /// default), HDR10, or `OutputColor::Sdr` to stay in SDR. The player follows the
    /// display: HDR turned on or off, the SDR white level, another monitor. A display in
    /// SDR always gets SDR. With FSR frame generation the output is scRGB or SDR; with
    /// DLSS frame generation, SDR (not verified in HDR yet).
    pub hdr: OutputColor,
    /// Latency reduction (Reflex, Anti-Lag 2 or the engine's own; ADR 0023).
    pub low_latency: LowLatency,
    /// At most this many real frames a second (frame generation shows twice as many).
    pub fps_limit: Option<f64>,
    /// Frames the CPU may record ahead of the GPU.
    pub frames_in_flight: usize,
    /// Frame generation (ADR 0091): FSR's through a paced presenter, or DLSS's through
    /// Streamline (with the user's DLLs, on an RTX 40 or newer). Unavailable, the frame
    /// is presented without it.
    pub frame_generation: FrameGeneration,
    /// The renderer's features: the device class's (`RenderDefaults::for_device`), or
    /// these.
    pub render: Option<RenderDefaults>,
    /// Antialiasing, over the defaults' choice.
    pub antialiasing: Option<Antialiasing>,
    /// The render size against the output's, over the defaults' choice.
    pub quality: Option<Quality>,
    /// Ambient occlusion, over the defaults' choice (`Some(None)`: off).
    pub ambient_occlusion: Option<Option<AoSettings>>,
    /// Culling and drawing by meshlet (ADR 0085), over the defaults' choice.
    pub meshlet_culling: Option<bool>,
    /// Shadowed local lights (ADR 0071).
    pub local_shadows: Option<LocalShadowSettings>,
    /// The sun's contact shadows (ADR 0071; they turn the depth prepass on); `None` (the
    /// default): off.
    pub contact_shadows: Option<ContactShadowSettings>,
    /// Reflection probes (ADR 0073) for the world's `engine_runtime::ReflectionProbe`
    /// entities: their set's room and captures, made with the first such entity; `None`:
    /// the entities reflect nothing.
    pub reflection_probes: Option<ProbeSettings>,
    /// Screen-space reflections (ADR 0073) in the view, at the render size; `None` (the
    /// default): off. Unlike probes they cost every frame whatever the world holds (at
    /// 1080p with half-resolution rays: 0.26 ms on an RTX 4060 Ti, 7.5 ms on a two-CU
    /// Radeon), so a game turns them on.
    pub screen_space_reflections: Option<SsrSettings>,
    /// Global illumination from a volume of probes (ADR 0088, ADR 0089) over the world's
    /// `engine_runtime::GiVolume` entity, on GPUs with ray queries: the device class's
    /// settings (`None`: `GiSettings::for_device`), these, or none (`Some(None)`).
    pub global_illumination: Option<Option<GiSettings>>,
    /// Streamed textures follow what the views sample (texture feedback, ADR 0076).
    pub texture_feedback: bool,
    /// Videos decode on the GPU's video engine: the device asks for its AV1 decode queue
    /// (ADR 0100; 2.5 ms of device creation on the RTX 4060 Ti, nothing on the Radeon).
    /// Without it they decode on the CPU (rav1d).
    pub video_decode: bool,
    /// The frame drawn into an image of its own and copied into the swapchain image
    /// (`Some(true)`), or straight into it (`Some(false)`); `None`: apart only when the
    /// swapchain presents across adapters (`engine_render::present`).
    pub present_apart: Option<bool>,
    /// The simulation's fixed rate, ticks a second (ADR 0019: 64).
    pub tick_hz: f64,
    /// Where the pipeline cache is kept (ADR 0055): `None` for
    /// `%LOCALAPPDATA%\<title>\`; `Some(None)` keeps none.
    pub pipeline_cache: Option<Option<PathBuf>>,
    /// Shading variants also made on the device's warm-up thread, not only on first use
    /// (ADR 0079).
    pub warm_up: bool,
    /// The CLAUDE.md §5 process set-up: power throttling off, the frame thread under
    /// MMCSS "Games" on performance cores, crash reports and logging.
    pub process_setup: bool,
    /// The game's text (ADR 0102): a strings pack, or in development a `loc/` folder kept
    /// live; its localizer is the world's `engine_loc::Localizer` resource. `None`: none.
    pub text: Option<crate::TextSource>,
    /// The player's languages, most preferred first (BCP 47 tags, as the game's settings
    /// keep them): the text's locale is the first it serves. `None`: Windows' display
    /// languages.
    pub languages: Option<Vec<String>>,
    /// The game's input actions: the player reads raw keyboard and mouse input and XInput
    /// gamepads into the world's `engine_input::Input` resource, sampled before every tick
    /// (`input.tick()`) and before `Game::present` (`input.frame()`). `None`: no input
    /// system (the game sees the window's events only).
    pub input: Option<engine_input::InputMap>,
    /// Saves (ADR 0104): the folder, the game's version, the encoding, autosaves. The
    /// world's [`crate::Saves`] resource takes the game's requests.
    pub saves: crate::SaveSettings,
    /// Sound (ADR 0105): the sounds' folder, the buses, the acoustics. The world's
    /// [`crate::Mixer`] resource sets the buses' volumes.
    pub audio: crate::AudioSettings,
    /// Networking (ADR 0106): the game's input type and schema
    /// (`NetConfig::new::<Input>(schema)`); the world's [`crate::Network`] resource hosts,
    /// joins and leaves. `None`: singleplayer only.
    pub network: Option<crate::NetConfig>,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            title: String::from("game"),
            pack: None,
            scene: None,
            project: None,
            derived_data: None,
            window: Window::FullScreen,
            monitor: None,
            window_placement: None,
            start_hidden: false,
            adapter: AdapterPreference::HighPerformance,
            validation: None,
            vsync: false,
            hdr: OutputColor::ScRgb,
            low_latency: LowLatency::On,
            fps_limit: None,
            frames_in_flight: 2,
            frame_generation: FrameGeneration::Off,
            render: None,
            antialiasing: None,
            quality: None,
            ambient_occlusion: None,
            meshlet_culling: None,
            local_shadows: Some(LocalShadowSettings::default()),
            contact_shadows: None,
            reflection_probes: Some(ProbeSettings::default()),
            screen_space_reflections: None,
            global_illumination: None,
            texture_feedback: true,
            video_decode: false,
            present_apart: None,
            tick_hz: 64.0,
            pipeline_cache: None,
            warm_up: true,
            process_setup: true,
            text: None,
            languages: None,
            input: None,
            saves: crate::SaveSettings::default(),
            audio: crate::AudioSettings::default(),
            network: None,
        }
    }
}
