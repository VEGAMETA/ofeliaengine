//! The frame renderer (ADR 0072): one object that composes a view's whole frame into the
//! render graph, in the right order, with the right antialiasing (ADR 0075).
//!
//! ```text
//!  scene passes, render size         step 4: render → output     post chain, output size
//!  ─────────────────────────────     ───────────────────────     ────────────────────────
//!  cull → shadows → (prepass, AO) →  TAAU / FSR / DLSS, or    →  bloom → exposure →
//!  → forward → (SSS) → (reflections) FSR 1, or nearest /          tonemap → FXAA / MLAA /
//!  → sky/fog → transparent           bilinear / integer in the    SMAA (S2x, T2x, 4x)
//!  → skinned motion (ADR 0068)       tone mapper                  → the view's target
//!  (multisampled with MSAA, then
//!  resolved; jitter + motion
//!  vectors when temporal)            history per view
//! ```
//!
//! - [`FrameRenderer`] owns the scene passes (`ForwardRenderer`: culling, shadows, ambient
//!   occlusion, forward and transparent shading, subsurface scattering, the sky) and the
//!   post chain (bloom, tone mapping). One per output format.
//! - [`View`] keeps one camera's state across frames (UE's `FSceneViewState`): its
//!   jitter and last frame's camera for motion vectors, its upscaler and history, its
//!   occlusion-culling state, its adapted exposure, its previous color. One per view:
//!   the main camera, each split-screen player, each render target.
//! - Reflections (ADR 0073) are part of the scene passes: probes come with the scene
//!   ([`SceneInput::reflection_probes`]), screen-space reflections are a view setting
//!   (`ViewSettings::screen_space_reflections`, history in the [`View`]). They run after
//!   the opaque surfaces and before the atmosphere and the transparent ones, which the
//!   hook of [`FrameRenderer::add_view_with`] comes after; the hook gets their inputs
//!   and result in `SceneTargets::passes.reflections`.
//! - [`ViewSettings`] says what a view wants: [`Antialiasing`] (raster samples ([`Msaa`]),
//!   how the render size reaches the output ([`Upscale`]: nearest, bilinear, integer,
//!   FSR 1, the temporal upscalers, SMAA T2x), a filter after tone mapping
//!   ([`PostAntialiasing`]); [`FrameRenderer::check`] says whether a combination runs
//!   here, [`FrameRenderer::antialiasing_support`] what the GPU offers), the render size
//!   (`Quality`, a dynamic `render_scale`, or a fixed `render_size`), exposure, tone
//!   curve, bloom.
//!
//! Scene-level work runs once per frame before the views: the pools' and tables'
//! `begin_frame` (geometry, textures, materials), the GPU scene's `add_passes` and the
//! skinning pass (`Skinning::begin_frame`); their frames go into [`SceneInput`].
//!
//! Every frame, per view:
//!
//! ```text
//! // Before MaterialTable::begin_frame: the texture LOD bias follows the upscaler.
//! frame.prepare(&mut view, &settings, output_size, PrepareContext { .. })?;
//! // In the frame's graph, after the pools' and tables' begin_frame:
//! frame.add_view(&mut fg, &mut ctx, &mut view, &scene, &camera, target)?;
//! ```
//!
//! [`FrameRenderer::prepare`] creates and retires what the settings need (an upscaler
//! switch or a new output size starts the history anew), sets the jitter phases and the
//! LOD bias the upscaler asks for. [`FrameRenderer::add_view`] declares the passes;
//! [`FrameRenderer::add_view_with`] also calls back between the scene and antialiasing
//! with the scene's targets, last frame's color and the motion vectors (screen-space
//! effects go there). A camera cut ([`ViewCamera::cut`]) drops the view's history.
//!
//! [`RenderDefaults`] gives measured defaults per [`DeviceClass`] (discrete or small
//! integrated GPU): ambient occlusion quality, antialiasing, render size and meshlet
//! culling.
//!
//! Frame generation (ADR 0091, [`FrameGeneration`], off by default):
//! [`FrameRenderer::add_view_generating`] draws the real frame into a paced presenter's
//! image and the frame before it into another, and the game hands its UI over as a layer
//! ([`UiRenderer`], [`UiCanvas`]) that is drawn on both, each with its own camera: the
//! HUD is never interpolated, UI in the world never smeared.

mod antialiasing;
mod renderer;
mod settings;
mod ui;
mod view;

pub use antialiasing::{
    Antialiasing, AntialiasingSupport, InvalidAntialiasing, Msaa, PostAntialiasing, SmaaQuality,
    Upscale,
};

pub use renderer::{
    FrameContext, FrameError, FrameRenderer, GeneratedFrame, GeneratedTarget, PrepareContext,
    SceneDraws, SceneInput, SceneTargets, ViewCamera, ViewFrame, ViewTarget,
};
pub use settings::{
    DeviceClass, Exposure, FrameGeneration, RenderDefaults, ViewKind, ViewSettings,
};
pub use ui::{Placement, UiCanvas, UiRenderer};
pub use view::{Prepared, Resets, View};

pub use engine_upscale::{Quality, UpscalerKind};
