//! The frame renderer (ADR 0072): one object that composes a view's whole frame into the
//! render graph, in the right order, with the right antialiasing.
//!
//! ```text
//!  scene passes, render size          antialiasing             post chain, output size
//!  ─────────────────────────────      ────────────────         ─────────────────────────
//!  cull → shadows → (prepass, AO)  →  TAAU / FSR / DLSS   →   bloom → exposure → tonemap
//!  → forward → (SSS) → (reflections)  (or nothing: Off)        → the view's target
//!  → sky/fog
//!  → transparent (+ reactive mask)
//!  → skinned motion (ADR 0068)
//!  jitter + motion vectors            history per view
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
//! - [`ViewSettings`] says what a view wants: [`Antialiasing`] (`Off`, or a temporal
//!   upscaler: the engine's TAAU, FSR 3.1, DLSS), the render size (`Quality`, or a
//!   dynamic `render_scale`), exposure, tone curve, bloom.
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
//! integrated GPU): ambient occlusion quality, antialiasing and render size.

mod antialiasing;
mod renderer;
mod settings;
mod view;

pub use antialiasing::{
    Antialiasing, AntialiasingSupport, InvalidAntialiasing, Msaa, PostAntialiasing, SmaaQuality,
    Upscale,
};

pub use renderer::{
    FrameContext, FrameError, FrameRenderer, PrepareContext, SceneDraws, SceneInput, SceneTargets,
    ViewCamera, ViewFrame, ViewTarget,
};
pub use settings::{DeviceClass, Exposure, RenderDefaults, ViewSettings};
pub use view::{Prepared, Resets, View};

pub use engine_upscale::{Quality, UpscalerKind};
