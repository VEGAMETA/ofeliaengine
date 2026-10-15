//! What a game asks of its player: the window, presentation, latency and rendering.

use engine_frame::{Antialiasing, FrameGeneration, Quality, RenderDefaults};
use engine_latency::LowLatency;
use engine_render::ao::AoSettings;
use engine_render::forward::LocalShadowSettings;
use engine_rhi::AdapterPreference;
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
    /// The window.
    pub window: Window,
    /// The monitor (an `HMONITOR`, `engine_platform::display::Monitor::handle`); `None`:
    /// the primary one.
    pub monitor: Option<isize>,
    /// The GPU: `ENGINE_ADAPTER` if set, else this.
    pub adapter: AdapterPreference,
    /// Wait for the display's refresh (FIFO) instead of presenting at once.
    pub vsync: bool,
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
    /// Streamed textures follow what the views sample (texture feedback, ADR 0076).
    pub texture_feedback: bool,
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
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            title: String::from("game"),
            pack: None,
            window: Window::FullScreen,
            monitor: None,
            adapter: AdapterPreference::HighPerformance,
            vsync: false,
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
            texture_feedback: true,
            present_apart: None,
            tick_hz: 64.0,
            pipeline_cache: None,
            warm_up: true,
            process_setup: true,
        }
    }
}
