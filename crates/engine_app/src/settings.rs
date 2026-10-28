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
    /// Compile the game's scripts to be debugged (ADR 0119): Luau's optimization level 1,
    /// which inlines nothing, so a step goes into a function as its text reads. The
    /// editor's player has it; it costs nothing measurable on ADR 0114's scenes.
    pub debuggable_scripts: bool,
    /// Run the game's scripts interpreted, not as Luau's native code (ADR 0119's full script
    /// debugging, the editor's switch): a stop at an error then reads every frame's locals,
    /// and the scripts run 8-33 % slower on ADR 0114's scenes. Off (the default): native
    /// code, as games run. [`Player::set_interpreted_scripts`](crate::Player::set_interpreted_scripts)
    /// switches scripts already loaded.
    pub interpreted_scripts: bool,
    /// VS Code's script debugger (ADR 0119 E9-3): the player listens for it as the editor
    /// does, its scripts compiled to be debugged. `None` (the default): nothing listens. A
    /// game's `main` ([`run`](crate::run)) sets it from `--debug` and `--wait-for-debugger`;
    /// only development builds (debug assertions on: `cargo run`) listen, never a release or
    /// shipping one.
    pub debugger: Option<DebuggerSettings>,
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
    /// How many frames a second at most ([`FrameCap`]): release builds keep to the
    /// display's refresh rate, debug builds go as fast as they can.
    pub frame_cap: FrameCap,
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
    /// The driver's statistics of the pipelines the player makes (registers, spills), as
    /// `ENGINE_PIPELINE_STATS=1` asks: what [`crate::Player::material_cost`] reads. The
    /// editor turns it on (E10-4c); it costs pipeline creation nothing measured.
    pub pipeline_statistics: bool,
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
    /// The player's own physics world (ADR 0114 E7-3), for a game that owns none
    /// ([`crate::Game::physics`] gives none): made with these settings, the pack's bodies in
    /// it, the scene's models and brushes kept in step before the ticks, stepped after each
    /// tick, its bodies' entities interpolated each frame. Scripts query it
    /// (`physics.raycast`); sound travels through it when the game gives no acoustics.
    /// `None` (the default): none.
    pub physics: Option<engine_physics::PhysicsSettings>,
}

/// VS Code's script debugger in a standalone player (ADR 0119 E9-3,
/// [`PlayerSettings::debugger`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DebuggerSettings {
    /// The world waits, kept still, until VS Code has attached and set its breakpoints
    /// (`--wait-for-debugger`, Unity's "Wait For Managed Debugger"): the scripts' first
    /// hooks can stop.
    pub wait: bool,
}

impl DebuggerSettings {
    /// What a command line (the program's arguments, without its name) asks: `--debug`
    /// listens, `--wait-for-debugger` listens and waits; none without either. The game's
    /// own arguments are left to it.
    pub fn from_args<I: IntoIterator<Item = S>, S: AsRef<std::ffi::OsStr>>(
        args: I,
    ) -> Option<Self> {
        let mut asked = None;
        for arg in args {
            match arg.as_ref().to_str() {
                Some("--debug") => {
                    asked.get_or_insert(Self::default());
                }
                Some("--wait-for-debugger") => asked = Some(Self { wait: true }),
                _ => {}
            }
        }
        asked
    }
}

/// How many frames a second the player draws at most (ADR 0023's addendum of 2026-10-22).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameCap {
    /// The display's refresh rate `hz`, release builds' default (the team, 2026-10-21):
    /// - with vsync off, a few frames under it (`hz - hz² / 3600`, Godot's rule: 138 on
    ///   144 Hz, 224 on 240 Hz), so a variable-refresh display stays in its range;
    /// - with vsync on, as far over it (`hz + hz² / 3600`): the display paces, and the
    ///   cap only stops what does not wait for it (presenting across adapters, a driver
    ///   forcing vsync off).
    ///
    /// Frame generation halves it in real frames. Offscreen there is no display and no
    /// cap.
    Display,
    /// At most this many real frames a second (frame generation shows twice as many).
    Fps(f64),
    /// As fast as it goes: measurements, and debug builds' default.
    Uncapped,
}

impl FrameCap {
    /// The build's default: [`FrameCap::Display`] in release builds, uncapped in debug
    /// ones.
    pub const fn for_build() -> Self {
        if cfg!(debug_assertions) {
            Self::Uncapped
        } else {
            Self::Display
        }
    }

    /// The real frames a second it allows: `refresh_hz` is the display's (none
    /// offscreen), `generated` whether frame generation is on.
    pub fn fps(self, vsync: bool, refresh_hz: Option<u32>, generated: bool) -> Option<f64> {
        match self {
            Self::Uncapped => None,
            Self::Fps(fps) => Some(fps),
            Self::Display => {
                let hz = f64::from(refresh_hz?);
                let margin = hz * hz / 3600.0;
                let cap = if vsync { hz + margin } else { hz - margin };
                Some(if generated { cap / 2.0 } else { cap })
            }
        }
    }
}

impl PlayerSettings {
    /// The scripts' VM settings: debuggable (Luau's optimization level 1) when asked
    /// ([`Self::debuggable_scripts`]), interpreted when asked ([`Self::interpreted_scripts`]).
    pub(crate) fn script_settings(&self) -> engine_script::ScriptSettings {
        engine_script::ScriptSettings {
            optimization_level: if self.debuggable_scripts { 1 } else { 2 },
            native: !self.interpreted_scripts,
            ..engine_script::ScriptSettings::default()
        }
    }
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            title: String::from("game"),
            pack: None,
            scene: None,
            project: None,
            debuggable_scripts: false,
            interpreted_scripts: false,
            debugger: None,
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
            frame_cap: FrameCap::for_build(),
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
            pipeline_statistics: false,
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
            physics: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debugger_is_asked_on_the_command_line_beside_the_games_own_arguments() {
        assert_eq!(DebuggerSettings::from_args(["--level", "3"]), None);
        assert_eq!(
            DebuggerSettings::from_args(["--debug", "--level", "3"]),
            Some(DebuggerSettings { wait: false })
        );
        assert_eq!(
            DebuggerSettings::from_args(["--wait-for-debugger"]),
            Some(DebuggerSettings { wait: true })
        );
        assert_eq!(
            DebuggerSettings::from_args(["--wait-for-debugger", "--debug"]),
            Some(DebuggerSettings { wait: true })
        );
    }

    #[test]
    fn the_display_cap_keeps_a_few_frames_under_the_refresh_rate_without_vsync() {
        // Godot's rule: 144 Hz → 138.24, 60 Hz → 59.
        let cap = FrameCap::Display.fps(false, Some(144), false).unwrap();
        assert!((cap - 138.24).abs() < 1e-9, "{cap}");
        let cap = FrameCap::Display.fps(false, Some(60), false).unwrap();
        assert!((cap - 59.0).abs() < 1e-9, "{cap}");
        // Frame generation shows twice the real frames it caps.
        let cap = FrameCap::Display.fps(false, Some(144), true).unwrap();
        assert!((cap - 69.12).abs() < 1e-9, "{cap}");
    }

    #[test]
    fn the_display_cap_stays_over_the_refresh_rate_with_vsync_and_is_none_offscreen() {
        // Over the rate: the display paces, the cap stops what does not wait for it.
        let cap = FrameCap::Display.fps(true, Some(240), false).unwrap();
        assert!((cap - 256.0).abs() < 1e-9, "{cap}");
        // 59.94 Hz is reported as 59: still over it.
        let cap = FrameCap::Display.fps(true, Some(59), false).unwrap();
        assert!(cap > 59.94, "{cap}");
        assert_eq!(FrameCap::Display.fps(false, None, false), None);
        assert_eq!(FrameCap::Display.fps(true, None, false), None);
    }

    #[test]
    fn a_rate_is_kept_as_it_is_and_uncapped_is_none() {
        assert_eq!(FrameCap::Fps(100.0).fps(true, Some(144), true), Some(100.0));
        assert_eq!(FrameCap::Uncapped.fps(false, Some(144), false), None);
        assert_eq!(
            PlayerSettings::default().frame_cap,
            if cfg!(debug_assertions) {
                FrameCap::Uncapped
            } else {
                FrameCap::Display
            }
        );
    }
}
