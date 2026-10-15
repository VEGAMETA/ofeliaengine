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
//! ([`Videos::texture_index`]); their sound plays through an audio output the player
//! starts with the first video that has sound.
//!
//! Crash reports are the game's `main`'s to set up (`engine_crash`): its monitor process
//! runs the same executable.

mod game;
mod player;
mod process;
mod report;
mod settings;
mod videos;

pub use game::{FrameParts, Game};
pub use player::{AppError, MAX_UI_CANVASES, Player, Presentation, SceneInfo, Setup, UiLayer};
pub use report::{FrameReport, Startup, StartupPipelines};
pub use settings::{PlayerSettings, Window};
pub use videos::{MaterialSlot, VideoId, Videos};
// What opening a video takes and tells (ADR 0100).
pub use engine_video::player::{VideoDesc, VideoEvent};
