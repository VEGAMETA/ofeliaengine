//! The player (ADR 0099): what a game's executable owns, so that a game is a few hooks.
//!
//! [`Player::new`] starts everything a game needs, overlapping what does not depend on
//! each other:
//! - **the process** (CLAUDE.md §5): power throttling off, the frame thread under MMCSS
//!   "Games" and on performance cores where the CPU has two kinds;
//! - **the window and its swapchain**: borderless full screen by default, the swapchain
//!   stating that the driver must not take the monitor for itself (ADR 0099: NVIDIA then
//!   flips it, 7.9 against 9.3 ms to the screen), a window fit into the work area,
//!   exclusive full screen, or none (offscreen: each frame drawn into an image that
//!   [`Player::capture`] reads back, for tests and benchmarks);
//! - **the device and latency control**: Reflex, Anti-Lag 2 or the engine's own limiter
//!   (ADR 0023), frame generation (FSR's or DLSS's, ADR 0091);
//! - **the pipeline cache** in `%LOCALAPPDATA%\<title>\` (ADR 0055) and the warm-up
//!   thread (ADR 0079);
//! - **the scene and the renderer**: the pack streamed within the GPU memory budget (ADR
//!   0076), its instances as entities in the world; the frame renderer with the device
//!   class's `RenderDefaults` applied whole;
//! - **the runtime** (ADR 0064): the world, the fixed-step simulation, extraction into
//!   the GPU scene.
//!
//! [`Player::frame`] runs one frame in the order that keeps input latency low (latency
//! control's wait, the frame slot, the swapchain image, input, the ticks, presentation,
//! animation, streaming, rendering, presenting), calling the [`Game`]'s hooks.
//!
//! Lights are components on entities (`engine_runtime::{PointLight, SpotLight, Sun}`);
//! skinned meshes too (`engine_runtime::SkinnedMesh`): the game sets their skinning
//! matrices in [`Game::animate`] (`Skinning::set_palette`), the player places them.
//! Reflection probes and the global illumination volume as well
//! (`engine_runtime::{ReflectionProbe, GiVolume}`): the player makes the probe set, the
//! volume and the scene its rays trace with their first entities, and mirrors them every
//! frame; screen-space reflections are a setting.
//!
//! Videos (ADR 0100) are opened by the game with [`Videos::open`] (in [`Setup`] or any
//! frame's [`FrameParts`]) and shown in materials ([`Videos::show_in`]) or UI sprites
//! ([`Videos::texture_index`]); their sound plays through the player's mixer.
//!
//! Sound (ADR 0105) is components too: an [`AudioSource`] plays a sound file as it is
//! (music, the UI), an [`AudioSource3D`] at its entity in the world; an [`AudioListener`]
//! hears (the main camera without one). Saves keep where every sound was. With the game's
//! physics ([`Game::acoustics`]), walls muffle 3D sounds and the room around the listener
//! answers them; a [`ReverbZone`] gives a place its own reverb. Sounds come from a folder
//! while developing (a changed file heard at once) and a sound pack in a release
//! ([`SoundFiles`]). The world's [`Mixer`] sets the buses' volumes.
//!
//! Text (ADR 0102): the game's localizer is a resource of the world
//! (`world.resource::<Localizer>()`), made from [`PlayerSettings::text`] in the locale of
//! the player's languages; a `loc/` folder ([`TextSource::Folder`]) is kept live, an edit
//! showing in the running game.
//!
//! Networking (ADR 0106): with [`PlayerSettings::network`] the player runs the session the
//! game asks for through the world's [`Network`] resource: hosting, it runs the server around
//! the game's ticks; joined, the client runs the ticks (its predictions); offline, nothing.
//!
//! Physics (ADR 0101) is the game's: a [`ScenePhysics`] it owns gets the pack's bodies with
//! [`Player::load_physics`] (level geometry static, props moving the scene's instances) and
//! is stepped from [`Game::tick`]. A game without physics loads none of the pack's colliders.
//!
//! Crash reports are the game's `main`'s to set up (`engine_crash`): its monitor process
//! runs the same executable.

mod assets;
mod entry;
mod game;
mod net;
mod overlay;
mod physics;
mod player;
pub mod prelude;
mod process;
mod report;
mod saves;
mod settings;
mod sound;
mod text;
mod videos;

pub use assets::{Alpha, Assets, MaterialFile, Model, builtin};
/// Networking's replication (ADR 0074): the schema, `Replicated`, lag compensation.
pub use engine_net as netcode;
pub use engine_platform::display::DisplayInfo;
pub use engine_render::post::OutputLevels;
pub use engine_rhi::OutputColor;
/// Saves (ADR 0104): everything a game's own saves need (`capture`, `write_save`,
/// `read_save`, `apply_with`).
pub use engine_save as save;
/// Saves' types a game's hooks take (ADR 0104).
pub use engine_save::{
    ApplyOptions, CaptureOptions, Encoding, LoadReport, LoadedSave, SaveHeader, SaveImage, SceneId,
    SlotInfo, Transient,
};
pub use entry::{Start, run};
pub use game::{FrameParts, Game};
pub use net::{
    HitCapsule, HostOptions, NetConfig, NetEvent, NetInput, NetState, Network, PlayerId, Predicted,
    SubTick, To,
};
pub use overlay::{Overlay, OverlayOutput, OverlayPass};
pub use physics::{SceneBodies, ScenePhysics};
pub use player::{AppError, MAX_UI_CANVASES, Player, Presentation, SceneInfo, Setup, UiLayer};
pub use report::{FrameReport, Startup, StartupPipelines};
pub use saves::{Autosave, QUICK_SLOT, SaveSettings, Saves};
pub use settings::{PlayerSettings, Window};
pub use sound::{
    AcousticSettings, AudioListener, AudioSettings, AudioSource, AudioSource3D, BusSettings,
    Falloff, Mixer, PlayMode, ReverbZone, Room, SoundFiles,
};
pub use text::TextSource;
pub use videos::{MaterialSlot, VideoId, Videos};
// What opening a video takes and tells (ADR 0100).
pub use engine_video::player::{VideoDesc, VideoEvent};
// What a game formats its text with.
pub use engine_loc::{Args, Diagnostic, Localizer, MessageId, Value, loc};
