//! The frame renderer: one view's frame, declared into the render graph in order.

use crate::settings::{Antialiasing, Exposure, ViewSettings};
use crate::view::{HistoryImage, Prepared, Retired, View};
use core::fmt;
use engine_core::math::{Mat4, Quat, Vec3, vulkan_perspective};
use engine_render::environment::Environment;
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardPasses, ForwardRenderer, ForwardView, LocalLight,
    MotionOutput, Occlusion, ShadowSettings, Temporal,
};
use engine_render::gpu_scene::SceneFrame;
use engine_render::graph::{
    Access, Buffer, FrameGraph, ImportedTexture, LoadOp, ShaderStages, Texture, TextureDesc,
    TextureState,
};
use engine_render::material::{MaterialFrame, MaterialTable};
use engine_render::mesh::GeometryFrame;
use engine_render::post::{Composite, HDR_FORMAT, Tonemap};
use engine_render::postfx::{AutoExposure, Bloom, BloomSettings};
use engine_render::skinning::SkinningFrame;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{Allocator, Bindless, Device, RhiError, SamplerKind};
use engine_upscale::{
    EXPOSURE_FORMAT, Taau, UpscaleCamera, UpscaleContext, UpscaleError, UpscaleInput, Upscaler,
    UpscalerKind,
};

/// Why a frame could not be declared.
#[derive(Debug)]
pub enum FrameError {
    /// A Vulkan or allocation error.
    Rhi(RhiError),
    /// The upscaler refused its pass.
    Upscale(UpscaleError),
    /// The upload ring had no room for the frame's draws: nothing was declared.
    RingFull,
    /// [`FrameRenderer::add_view`] before [`FrameRenderer::prepare`].
    NotPrepared,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rhi(e) => write!(f, "{e}"),
            Self::Upscale(e) => write!(f, "{e}"),
            Self::RingFull => f.write_str("the upload ring is full"),
            Self::NotPrepared => f.write_str("the view was not prepared"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<RhiError> for FrameError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

impl From<UpscaleError> for FrameError {
    fn from(e: UpscaleError) -> Self {
        Self::Upscale(e)
    }
}

/// The camera a view is drawn from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewCamera {
    /// World to view space (`look_at`, or the inverse of the camera's world transform).
    pub view_from_world: Mat4,
    /// The camera's position in the world.
    pub position: Vec3,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance (finite).
    pub far: f32,
    /// This frame does not continue the last one (teleport, switch of camera, new
    /// level): the history is dropped, nothing of the previous view ghosts into it, and
    /// automatic exposure jumps instead of adapting.
    pub cut: bool,
}

impl ViewCamera {
    /// A camera at `position`, turned by `rotation` (a unit quaternion; the camera looks
    /// down its −Z axis with +Y up, as `look_at` places it): a camera entity's transform.
    pub fn from_rigid(rotation: Quat, position: Vec3, fov_y: f32, near: f32, far: f32) -> Self {
        Self {
            view_from_world: Mat4::from_quat(rotation.conjugate())
                * Mat4::from_translation(-position),
            position,
            fov_y,
            near,
            far,
            cut: false,
        }
    }

    /// The projection times the view at an `aspect` (width over height), without jitter
    /// (the forward renderer adds it).
    pub fn clip_from_world(&self, aspect: f32) -> Mat4 {
        vulkan_perspective(self.fov_y, aspect, self.near, self.far) * self.view_from_world
    }
}

/// What the scene passes draw.
#[derive(Debug, Clone, Copy)]
pub enum SceneDraws<'a, 'g> {
    /// A draw list (`ForwardRenderer::add_passes`).
    List(&'a [DrawItem]),
    /// A persistent GPU scene's frame (`ForwardRenderer::add_scene_passes`, ADR 0064).
    Scene(&'a SceneFrame<'g>),
}

/// The scene a view shows, as it is this frame.
#[derive(Clone, Copy)]
pub struct SceneInput<'a, 'g> {
    /// This frame's geometry (`GeometryPool::begin_frame`).
    pub geometry: &'a GeometryFrame<'g>,
    /// This frame's materials (`MaterialTable::begin_frame`).
    pub materials: &'a MaterialFrame<'g>,
    /// What is drawn.
    pub draws: SceneDraws<'a, 'g>,
    /// This frame's skinning (`Skinning::begin_frame`, ADR 0068), when the scene has
    /// skinned meshes: views with motion vectors get the skinned motion pass after their
    /// scene passes (each skinned vertex's own motion).
    pub skinning: Option<&'a SkinningFrame<'g>>,
    /// The sun.
    pub light: DirectionalLight,
    /// Point and spot lights.
    pub lights: &'g [LocalLight],
    /// Sky light, the sky, the atmosphere and fog, or `None` for the flat ambient color.
    pub environment: Option<&'g Environment>,
}

impl fmt::Debug for SceneInput<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SceneInput")
            .field("draws", &self.draws)
            .field("lights", &self.lights.len())
            .finish_non_exhaustive()
    }
}

/// Where a view's frame goes: an LDR (or HDR-encoded) image the tone mapper writes.
#[derive(Debug, Clone, Copy)]
pub struct ViewTarget {
    /// The output image (the swapchain's, a viewport's texture).
    pub texture: Texture,
    /// The part of it the view covers (split screen), or `None` for all of it.
    pub rect: Option<vk::Rect2D>,
    /// What happens to the target first (`Load` keeps other views' composites).
    pub load: LoadOp,
}

/// The engine objects a frame's declarations use.
pub struct FrameContext<'a, 'g> {
    /// The device.
    pub device: &'a Device,
    /// Device memory (histories created on first use).
    pub allocator: &'a mut Allocator,
    /// The bindless set the frame executes with.
    pub bindless: &'g Bindless,
    /// The frame's upload ring (instances, batches, frame constants).
    pub ring: &'a mut UploadRing,
    /// The frame's number (bounds how long replaced resources are kept).
    pub frame_number: u64,
    /// Seconds since the previous frame (exposure adaptation, FSR's thresholds).
    pub dt: f32,
}

impl fmt::Debug for FrameContext<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameContext")
            .field("frame_number", &self.frame_number)
            .field("dt", &self.dt)
            .finish_non_exhaustive()
    }
}

/// The engine objects [`FrameRenderer::prepare`] uses.
pub struct PrepareContext<'a> {
    /// The device.
    pub device: &'a Device,
    /// Device memory.
    pub allocator: &'a mut Allocator,
    /// The bindless set.
    pub bindless: &'a Bindless,
    /// The material table the view draws with: its texture LOD bias follows the view's
    /// upscaler (ADR 0060). Views sharing a table share its bias; the last prepared wins.
    pub materials: &'a mut MaterialTable,
    /// The number of the frame about to be declared.
    pub frame_number: u64,
}

impl fmt::Debug for PrepareContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrepareContext")
            .field("frame_number", &self.frame_number)
            .finish_non_exhaustive()
    }
}

/// The scene passes' results, between the scene and antialiasing: where effects of your
/// own that work on the scene go (`FrameRenderer::add_view_with`): screen-space
/// reflections, the skinned motion pass, debug views.
#[derive(Debug, Clone, Copy)]
pub struct SceneTargets<'p> {
    /// HDR color at the render size (jittered with temporal antialiasing).
    pub color: Texture,
    /// Depth at the render size (0 near, 1 far).
    pub depth: Texture,
    /// Motion vectors, jitter, reset and the reactive mask, when the view has them.
    pub motion: Option<MotionOutput>,
    /// Last frame's antialiased HDR color ([`ViewSettings::previous_color`]): at the output
    /// size with temporal antialiasing, at the render size without; `None` when off, on the
    /// first frame and after a cut. Reproject it with `motion`.
    pub previous_color: Option<Texture>,
    /// The camera the scene was drawn with, without jitter.
    pub camera: Camera,
    /// Render size.
    pub render: (u32, u32),
    /// Output size.
    pub output: (u32, u32),
    /// What the scene passes declared.
    pub passes: &'p ForwardPasses,
}

/// What [`FrameRenderer::add_view`] declared.
#[derive(Debug, Clone, Copy)]
pub struct ViewFrame {
    /// The antialiasing used.
    pub antialiasing: Antialiasing,
    /// Render size.
    pub render: (u32, u32),
    /// Output size.
    pub output: (u32, u32),
    /// What the scene passes declared.
    pub passes: ForwardPasses,
    /// The antialiased (or plain) HDR image the post chain read.
    pub resolved: Texture,
    /// The automatic exposure's buffer, when on.
    pub exposure: Option<Buffer>,
}

/// Composes a whole frame for each view (see the crate documentation): the scene passes
/// at render resolution, temporal antialiasing or upscaling, then bloom, exposure and tone
/// mapping at output resolution. One per renderer configuration (output format, depth
/// format, shadows); views keep their own state ([`View`]).
pub struct FrameRenderer {
    forward: ForwardRenderer,
    tonemap: Tonemap,
    bloom: Bloom,
    depth_format: vk::Format,
    frames_in_flight: u64,
    #[cfg(feature = "dlss")]
    streamline: Option<engine_dlss::Streamline>,
}

impl fmt::Debug for FrameRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameRenderer")
            .field("depth_format", &self.depth_format)
            .field("frames_in_flight", &self.frames_in_flight)
            .finish_non_exhaustive()
    }
}

/// The exposure the tone mapper applies, as the 1 × 1 texture FSR and DLSS read: a fixed
/// multiplier cleared into it, or the automatic exposure's multiplier copied from its
/// buffer (its first float).
fn exposure_texture(fg: &mut FrameGraph<'_>, fixed: f32, auto: Option<Buffer>) -> Texture {
    let texture = fg.create_texture("exposure", TextureDesc::new_2d(EXPOSURE_FORMAT, 1, 1));
    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    match auto {
        Some(buffer) => fg
            .add_pass("exposure value")
            .read(buffer, Access::TransferRead)
            .write(texture, Access::TransferWrite)
            .execute(move |pass| {
                let region = vk::BufferImageCopy {
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: 1,
                        height: 1,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording outside a rendering scope; the graph arranged the
                // buffer's read and the image's layout.
                unsafe {
                    pass.device.cmd_copy_buffer_to_image(
                        pass.cmd,
                        pass.buffer(buffer),
                        pass.image(texture),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                }
            }),
        None => fg
            .add_pass("exposure value")
            .write(texture, Access::TransferWrite)
            .execute(move |pass| {
                // SAFETY: as above.
                unsafe {
                    pass.device.cmd_clear_color_image(
                        pass.cmd,
                        pass.image(texture),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &vk::ClearColorValue {
                            float32: [fixed, 0.0, 0.0, 0.0],
                        },
                        &[range],
                    );
                }
            }),
    }
    texture
}

/// The render size for `output` at `scale` per axis.
fn scaled(output: (u32, u32), scale: f32) -> (u32, u32) {
    let scale = scale.clamp(0.05, 1.0);
    (
        ((output.0 as f32 * scale) as u32).clamp(1, output.0),
        ((output.1 as f32 * scale) as u32).clamp(1, output.1),
    )
}

impl FrameRenderer {
    /// Creates the scene passes' and the post chain's pipelines for writing
    /// `output_format` (the swapchain's, or a viewport's) with a `depth_format` depth
    /// buffer. `frames_in_flight` bounds how long replaced view resources are kept.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        output_format: vk::Format,
        depth_format: vk::Format,
        shadows: ShadowSettings,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        Ok(Self {
            forward: ForwardRenderer::new(device, bindless, HDR_FORMAT, depth_format, shadows)?,
            tonemap: Tonemap::new(device, bindless, output_format)?,
            bloom: Bloom::new(device, bindless, BloomSettings::default())?,
            depth_format,
            frames_in_flight: frames_in_flight.max(1) as u64,
            #[cfg(feature = "dlss")]
            streamline: None,
        })
    }

    /// The scene passes (ambient occlusion, subsurface scattering, the depth prepass,
    /// culling options are set there).
    pub fn forward(&self) -> &ForwardRenderer {
        &self.forward
    }

    /// The scene passes, to change their settings.
    pub fn forward_mut(&mut self) -> &mut ForwardRenderer {
        &mut self.forward
    }

    /// Bloom (its settings apply to every view with bloom on).
    pub fn bloom(&self) -> &Bloom {
        &self.bloom
    }

    /// Makes DLSS available to views ([`Antialiasing::DLSS`]): `streamline` loaded,
    /// checked and attached to the device (ADR 0063). `None` withdraws it (views fall
    /// back to TAAU at their next [`Self::prepare`]).
    #[cfg(feature = "dlss")]
    pub fn set_streamline(&mut self, streamline: Option<engine_dlss::Streamline>) {
        self.streamline = streamline;
    }

    /// Whether `kind` can run here: TAAU always; FSR when built in (feature `fsr`); DLSS
    /// with Streamline ([`Self::set_streamline`]).
    pub fn available(&self, kind: UpscalerKind) -> bool {
        match kind {
            UpscalerKind::Taau => true,
            UpscalerKind::Fsr => cfg!(feature = "fsr"),
            #[cfg(feature = "dlss")]
            UpscalerKind::Dlss => self.streamline.is_some(),
            #[cfg(not(feature = "dlss"))]
            UpscalerKind::Dlss => false,
        }
    }

    /// A new upscaler of `kind` for one view, or `None` when it cannot run here.
    fn create_upscaler(
        &self,
        kind: UpscalerKind,
        device: &Device,
        bindless: &Bindless,
    ) -> Result<Option<Box<dyn Upscaler>>, RhiError> {
        let frames = self.frames_in_flight as usize;
        Ok(match kind {
            UpscalerKind::Taau => Some(Box::new(Taau::new(device, bindless, frames)?)),
            #[cfg(feature = "fsr")]
            UpscalerKind::Fsr => Some(Box::new(engine_fsr::Fsr::new(device, frames))),
            #[cfg(feature = "dlss")]
            UpscalerKind::Dlss => self
                .streamline
                .as_ref()
                .and_then(|s| engine_dlss::Dlss::new(s, frames).ok())
                .map(|d| Box::new(d) as Box<dyn Upscaler>),
            #[allow(unreachable_patterns)]
            _ => None,
        })
    }

    /// Applies `settings` to `view` for an output of `output` pixels, before the frame's
    /// `MaterialTable::begin_frame` (the texture LOD bias lives in the materials'
    /// samplers): creates or retires the upscaler, the temporal state, the occlusion
    /// state, the automatic exposure and the previous color; computes the render size;
    /// sets the jitter phases and the LOD bias the upscaler asks for. A new upscaler or
    /// output size starts the history anew. Call it every frame: when nothing changed it
    /// only compares.
    ///
    /// # Errors
    ///
    /// When a resource cannot be created; the view keeps its previous state.
    pub fn prepare(
        &self,
        view: &mut View,
        settings: &ViewSettings,
        output: (u32, u32),
        ctx: PrepareContext<'_>,
    ) -> Result<Prepared, FrameError> {
        engine_profiler::zone!("frame::FrameRenderer::prepare");
        let PrepareContext {
            device,
            allocator,
            bindless,
            materials,
            frame_number,
        } = ctx;
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        if !view.retired.is_empty() {
            view.collect(allocator, bindless, Some(completed));
        }
        let output = (output.0.max(1), output.1.max(1));
        let previous = view.prepared;
        let mut antialiasing = match settings.antialiasing {
            Antialiasing::Temporal(kind) if !self.available(kind) => Antialiasing::TAAU,
            other => other,
        };
        if previous.is_none_or(|p| p.antialiasing != antialiasing) {
            let upscaler = match antialiasing.upscaler() {
                Some(kind) => match self.create_upscaler(kind, device, bindless)? {
                    Some(upscaler) => Some(upscaler),
                    None => {
                        antialiasing = Antialiasing::TAAU;
                        self.create_upscaler(UpscalerKind::Taau, device, bindless)?
                    }
                },
                None => None,
            };
            if let Some(old) = core::mem::replace(&mut view.upscaler, upscaler) {
                view.retire(frame_number, Retired::Upscaler(old));
            }
        }
        let motion = ViewSettings {
            antialiasing,
            ..*settings
        }
        .wants_motion();
        match (motion, view.temporal.is_some()) {
            (true, false) => view.temporal = Some(Temporal::new(device, allocator, view.capacity)?),
            (false, true) => {
                if let Some(old) = view.temporal.take() {
                    view.retire(frame_number, Retired::Temporal(old));
                }
            }
            _ => {}
        }
        match (settings.occlusion_culling, view.occlusion.is_some()) {
            (true, false) => {
                view.occlusion = Some(Occlusion::new(device, allocator, view.capacity)?);
            }
            (false, true) => {
                if let Some(old) = view.occlusion.take() {
                    view.retire(frame_number, Retired::Occlusion(old));
                }
            }
            _ => {}
        }
        match settings.exposure {
            Exposure::Auto(exposure) => match &view.exposure {
                Some(auto) => auto.set_settings(exposure),
                None => {
                    view.exposure = Some(AutoExposure::new(device, allocator, bindless, exposure)?);
                }
            },
            Exposure::Fixed(_) => {
                if let Some(old) = view.exposure.take() {
                    view.retire(frame_number, Retired::Exposure(old));
                }
            }
        }

        let nominal_render = match &view.upscaler {
            Some(upscaler) => upscaler.render_size(settings.quality, output),
            None => settings.quality.render_size(output),
        };
        let render = settings
            .render_scale
            .map_or(nominal_render, |scale| scaled(output, scale));
        let (jitter_phases, lod_bias) = match &view.upscaler {
            Some(upscaler) => (
                upscaler.jitter_phases(render, output),
                upscaler.texture_lod_bias(nominal_render, output),
            ),
            None => (0, 0.0),
        };
        if let Some(temporal) = &view.temporal {
            temporal.set_jitter_phases(jitter_phases);
        }
        if materials.texture_lod_bias() != lod_bias {
            materials.set_texture_lod_bias(bindless, lod_bias)?;
        }

        // The previous antialiased color: at the output size, or the render size without
        // an upscaler (nothing brings the image to the output size before the post chain).
        let history_size = if antialiasing == Antialiasing::Off {
            render
        } else {
            output
        };
        let keep = settings.previous_color;
        if view
            .history
            .as_ref()
            .is_some_and(|h| !keep || h.size != history_size)
            && let Some(old) = view.history.take()
        {
            view.retire(frame_number, Retired::History(old));
        }
        if keep && view.history.is_none() {
            view.history = Some(HistoryImage::new(
                device,
                allocator,
                bindless,
                HDR_FORMAT,
                history_size,
            )?);
        }

        if previous.is_some_and(|p| p.antialiasing != antialiasing || p.output != output) {
            view.reset_pending = true;
            view.resets.changes += 1;
        }
        let prepared = Prepared {
            settings: *settings,
            antialiasing,
            output,
            render,
            nominal_render,
            jitter_phases,
            lod_bias,
        };
        view.prepared = Some(prepared);
        Ok(prepared)
    }

    /// Declares `view`'s whole frame into `fg` (see the crate documentation), drawn from
    /// `camera`, into `target`. [`Self::prepare`] must have run for this frame. The frame
    /// must execute with `ctx.bindless`.
    ///
    /// # Errors
    ///
    /// [`FrameError::RingFull`] when the upload ring had no room (nothing was declared),
    /// [`FrameError::Upscale`] when the upscaler refused its pass.
    pub fn add_view<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ctx: &mut FrameContext<'_, 'g>,
        view: &'g mut View,
        scene: &SceneInput<'_, 'g>,
        camera: &ViewCamera,
        target: ViewTarget,
    ) -> Result<ViewFrame, FrameError> {
        self.add_view_with(fg, ctx, view, scene, camera, target, |_, _| {})
    }

    /// [`Self::add_view`], calling `hook` between the scene passes and antialiasing with
    /// the scene's targets: the place for passes of your own that work on the scene at
    /// render resolution (screen-space reflections, the skinned motion pass, debug
    /// views). What `hook` writes into the color or motion vectors is antialiased and
    /// post-processed like the rest.
    ///
    /// # Errors
    ///
    /// As [`Self::add_view`].
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn add_view_with<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ctx: &mut FrameContext<'_, 'g>,
        view: &'g mut View,
        scene: &SceneInput<'_, 'g>,
        camera: &ViewCamera,
        target: ViewTarget,
        hook: impl FnOnce(&mut FrameGraph<'g>, &SceneTargets<'_>),
    ) -> Result<ViewFrame, FrameError> {
        engine_profiler::zone!("frame::FrameRenderer::add_view");
        let View {
            prepared,
            temporal,
            occlusion,
            upscaler,
            exposure,
            history,
            resets,
            reset_pending,
            ..
        } = view;
        let prepared = (*prepared).ok_or(FrameError::NotPrepared)?;
        let settings = prepared.settings;
        if camera.cut || core::mem::take(reset_pending) {
            if camera.cut {
                resets.cuts += 1;
                if let Some(temporal) = temporal.as_ref() {
                    temporal.reset();
                }
                if let Some(occlusion) = occlusion.as_ref() {
                    occlusion.reset();
                }
                if let Some(exposure) = exposure.as_ref() {
                    exposure.reset();
                }
            }
            if let Some(upscaler) = upscaler.as_mut() {
                upscaler.reset();
            }
            if let Some(history) = history.as_mut() {
                history.valid = false;
            }
        }
        let temporal: Option<&'g Temporal> = temporal.as_ref();
        let occlusion: Option<&'g Occlusion> = occlusion.as_ref();
        let upscaler: Option<&'g mut Box<dyn Upscaler>> = upscaler.as_mut();
        let exposure: Option<&'g AutoExposure> = exposure.as_ref();
        let (render, output) = (prepared.render, prepared.output);

        // 1. The scene at render resolution, jittered, with motion vectors when temporal.
        let color = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, render.0, render.1));
        let depth = fg.create_texture(
            "depth",
            TextureDesc::new_2d(self.depth_format, render.0, render.1),
        );
        let scene_camera = Camera {
            clip_from_world: camera.clip_from_world(output.0 as f32 / output.1 as f32),
            position: camera.position,
        };
        let forward_view = ForwardView {
            camera: scene_camera,
            light: scene.light,
            color,
            depth,
            clear: settings.clear,
            occlusion,
            temporal,
            lights: scene.lights,
            environment: scene.environment,
        };
        let passes = match scene.draws {
            SceneDraws::List(draws) => self.forward.add_passes(
                fg,
                ctx.ring,
                ctx.bindless,
                scene.geometry,
                scene.materials,
                draws,
                &forward_view,
            ),
            SceneDraws::Scene(frame) => self.forward.add_scene_passes(
                fg,
                ctx.ring,
                ctx.bindless,
                scene.geometry,
                scene.materials,
                frame,
                &forward_view,
            ),
        }
        .ok_or(FrameError::RingFull)?;
        if let (Some(skinning), Some(motion)) = (scene.skinning, &passes.motion) {
            skinning.add_motion_pass(fg, ctx.ring, ctx.bindless, scene.geometry, depth, motion)?;
        }

        // The previous antialiased color, kept for screen-space effects.
        let kept = history.as_mut().map(|h| {
            let texture = fg.import_texture(
                "previous color",
                ImportedTexture {
                    image: h.image,
                    view: h.view,
                    desc: TextureDesc::new_2d(h.format, h.size.0, h.size.1),
                    initial: h.state,
                    final_state: Some(TextureState::shader_read(ShaderStages::ALL)),
                    sampled_index: Some(h.sampled),
                    storage_index: None,
                },
            );
            h.state = TextureState::shader_read(ShaderStages::ALL);
            let valid = core::mem::replace(&mut h.valid, true);
            (texture, valid, h.size)
        });

        // 2. Effects of the caller's on the scene.
        hook(
            fg,
            &SceneTargets {
                color,
                depth,
                motion: passes.motion,
                previous_color: kept.and_then(|(t, valid, _)| valid.then_some(t)),
                camera: scene_camera,
                render,
                output,
                passes: &passes,
            },
        );

        // 3. Exposure, measured on the scene at render resolution before antialiasing:
        // the upscalers that judge colors by it (FSR, DLSS) get this frame's exposure,
        // and the measurement costs the render size's pixels, not the output's.
        let (exposure_value, auto_exposure) = match (settings.exposure, exposure) {
            (Exposure::Auto(_), Some(auto)) => {
                (1.0, Some(auto.add_passes(fg, ctx.bindless, color, ctx.dt)))
            }
            (Exposure::Fixed(value), _) => (value, None),
            (Exposure::Auto(_), None) => (1.0, None),
        };

        // 4. Antialiasing / upscaling to the output size.
        let resolved = match (prepared.antialiasing, upscaler, passes.motion) {
            (Antialiasing::Temporal(_), Some(upscaler), Some(motion)) => {
                let upscaler: &'g mut dyn Upscaler = &mut **upscaler;
                // FSR and DLSS take the exposure the tone mapper applies (their own
                // measurement otherwise sees another picture than the player); TAAU
                // blends exposure-independent colors and needs none.
                let exposure_input =
                    matches!(upscaler.kind(), UpscalerKind::Fsr | UpscalerKind::Dlss)
                        .then(|| exposure_texture(fg, exposure_value, auto_exposure));
                upscaler.add_pass(
                    fg,
                    UpscaleContext {
                        device: ctx.device,
                        allocator: ctx.allocator,
                        bindless: ctx.bindless,
                    },
                    UpscaleInput {
                        color,
                        depth,
                        motion,
                        camera: UpscaleCamera {
                            near: camera.near,
                            far: camera.far,
                            fov_y: camera.fov_y,
                        },
                        frame_time_ms: ctx.dt * 1000.0,
                        exposure: exposure_input,
                    },
                    output,
                    ctx.frame_number,
                )?
            }
            _ => color,
        };
        if let Some((kept, _, size)) = kept {
            fg.add_pass("previous color")
                .read(resolved, Access::TransferRead)
                .write(kept, Access::TransferWrite)
                .execute(move |pass| {
                    let layers = vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    };
                    let region = vk::ImageCopy {
                        src_subresource: layers,
                        dst_subresource: layers,
                        extent: vk::Extent3D {
                            width: size.0,
                            height: size.1,
                            depth: 1,
                        },
                        ..Default::default()
                    };
                    // SAFETY: recording; the graph arranged both layouts.
                    unsafe {
                        pass.device.cmd_copy_image(
                            pass.cmd,
                            pass.image(resolved),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            pass.image(kept),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[region],
                        );
                    }
                });
        }

        // 5. Bloom and tone mapping, at the output size.
        let bloom = settings
            .bloom
            .then(|| self.bloom.add_passes(fg, ctx.bindless, resolved));
        self.tonemap.add_composite_pass(
            fg,
            ctx.bindless,
            resolved,
            target.texture,
            Composite {
                rect: target.rect,
                sampler: SamplerKind::LinearClamp,
                exposure: exposure_value,
                tonemap: true,
                curve: settings.curve,
                peak: settings.peak,
                encoding: settings.encoding,
                load: target.load,
                bloom,
                auto_exposure,
            },
        );
        Ok(ViewFrame {
            antialiasing: prepared.antialiasing,
            render,
            output,
            passes,
            resolved,
            exposure: auto_exposure,
        })
    }
}
