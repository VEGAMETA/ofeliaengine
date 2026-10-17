//! The frame renderer: one view's frame, declared into the render graph in order.

use crate::antialiasing::{
    Antialiasing, AntialiasingSupport, InvalidAntialiasing, Msaa, PostAntialiasing, SmaaQuality,
    Upscale,
};
use crate::settings::{Exposure, FrameGeneration, ViewSettings};
use crate::view::{HistoryImage, Prepared, Retired, View};
use core::fmt;
use engine_core::math::{Mat4, Quat, Vec3, vulkan_perspective};
use engine_render::dof::{DepthOfField, DofCamera};
use engine_render::environment::Environment;
use engine_render::forward::{
    Camera, CascadeCache, Decal, DirectionalLight, DrawItem, ForwardPasses, ForwardRenderer,
    ForwardView, LocalLight, MotionOutput, Occlusion, ShadowSettings, Temporal, ViewGi,
};
use engine_render::fsr1::{Fsr1, Fsr1Exposure};
use engine_render::gpu_scene::SceneFrame;
use engine_render::grading::GradingLut;
use engine_render::graph::{
    Access, Buffer, FrameGraph, ImportedTexture, LoadOp, ShaderStages, Texture, TextureDesc,
    TextureState,
};
use engine_render::material::{MaterialFrame, MaterialTable};
use engine_render::mesh::GeometryFrame;
use engine_render::motion_blur::{MotionBlur, MotionBlurCamera};
use engine_render::msaa::{MsaaInput, MsaaResolve};
use engine_render::post::{Composite, HDR_FORMAT, OutputEncoding, Resample, Tonemap};
use engine_render::postaa::{
    Decode, PERCEPTUAL_FORMAT, PostAa, PostFilter, PostTarget, SmaaInput, SmaaPreset, SmaaTemporal,
    smaa_jitter,
};
use engine_render::postfx::{AutoExposure, Bloom, BloomSettings};
use engine_render::reflections::{ReflectionProbes, Reflections, ScreenSpaceReflections};
use engine_render::skinning::Skinning;
use engine_render::skinning::SkinningFrame;
use engine_render::upload::UploadRing;
use engine_render::vsm::VirtualShadows;
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
    /// The view's antialiasing cannot run (ADR 0075): an invalid combination, or one
    /// this GPU does not support ([`FrameRenderer::check`]).
    Antialiasing(InvalidAntialiasing),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rhi(e) => write!(f, "{e}"),
            Self::Upscale(e) => write!(f, "{e}"),
            Self::RingFull => f.write_str("the upload ring is full"),
            Self::NotPrepared => f.write_str("the view was not prepared"),
            Self::Antialiasing(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<RhiError> for FrameError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

impl From<InvalidAntialiasing> for FrameError {
    fn from(e: InvalidAntialiasing) -> Self {
        Self::Antialiasing(e)
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
    /// Decals (ADR 0082; drawn with `ForwardRenderer::set_decals` on, not in
    /// multisampled views).
    pub decals: &'g [Decal],
    /// Sky light, the sky, the atmosphere and fog, or `None` for the flat ambient color.
    pub environment: Option<&'g Environment>,
    /// Reflection probes (ADR 0073; with an environment and reflections on in the forward
    /// renderer): the scene's, shared by its views; the first view of a frame captures
    /// what needs capturing.
    pub reflection_probes: Option<&'g ReflectionProbes>,
    /// Global illumination from a volume of probes (ADR 0088; with
    /// `ForwardRenderer::set_global_illumination` on, on the persistent GPU scene's path
    /// while it converges): the scene's, shared by its views.
    pub gi: Option<ViewGi<'g>>,
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
    /// The scene's skinning, when it has skinned meshes: with MSAA its motion pass needs
    /// the sample count's pipeline (`Skinning::set_motion_samples`).
    pub skinning: Option<&'a mut Skinning>,
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
    /// The scene's depth at the render size (0 at the near plane, 1 at the far plane),
    /// resolved with MSAA: what UI placed in the scene is tested against
    /// ([`crate::UiRenderer`]).
    pub depth: Texture,
    /// With frame generation on ([`FrameRenderer::add_view_generating`] for FSR's): the
    /// frame generated before this one.
    pub generated: Option<GeneratedFrame>,
}

#[cfg(feature = "dlss")]
impl ViewFrame {
    /// DLSS frame generation's inputs for this frame (ADR 0091 §7), drawn from `camera`
    /// into `hudless` (the view's target, before any UI) with the UI alone in `ui`; `None`
    /// unless [`ViewSettings::frame_generation`] is [`FrameGeneration::Dlss`]. Hand them
    /// to `engine_dlss::DlssFrameGeneration::add_pass` after the UI passes.
    pub fn dlss_generation_input(
        &self,
        camera: &ViewCamera,
        hudless: Texture,
        ui: Option<Texture>,
    ) -> Option<engine_dlss::GenerationInput> {
        let (generated, motion) = (self.generated?, self.passes.motion?);
        let aspect = self.output.0 as f32 / self.output.1.max(1) as f32;
        let clip_from_world = camera.clip_from_world(aspect);
        let previous_clip_from_clip = if generated.shown {
            generated.previous.clip_from_world(aspect) * clip_from_world.inverse()
        } else {
            Mat4::IDENTITY
        };
        let rotation = Quat::from_mat4(&camera.view_from_world).conjugate();
        Some(engine_dlss::GenerationInput {
            depth: self.depth,
            velocity: motion.velocity,
            hudless,
            ui,
            camera: engine_dlss::GenerationCamera {
                clip_from_view: vulkan_perspective(camera.fov_y, aspect, camera.near, camera.far),
                previous_clip_from_clip,
                position: camera.position,
                up: rotation * Vec3::Y,
                right: rotation * Vec3::X,
                forward: rotation * Vec3::NEG_Z,
                near: camera.near,
                far: camera.far,
                fov_y: camera.fov_y,
                aspect,
            },
            jitter: motion.jitter,
            reset: !generated.shown,
        })
    }
}

/// Where [`FrameRenderer::add_view_generating`] writes the generated frame, and how the
/// real frame's target is read (ADR 0091): a paced presenter's images
/// (`engine_latency::PacedImages`).
#[derive(Debug, Clone, Copy)]
pub struct GeneratedTarget {
    /// The generated frame, the output's size: a storage image (with an alpha channel)
    /// written as `format`.
    pub texture: Texture,
    /// The format the generated frame is written as (the UNORM twin of an sRGB output).
    pub format: vk::Format,
    /// The format the real frame's target is read as by frame interpolation (the same
    /// twin: the interpolation reads the encoded values).
    pub source_format: vk::Format,
}

/// The frame generated before a real frame (ADR 0091).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeneratedFrame {
    /// The camera half-way between the last frame's and this one's: the UI placed in the
    /// scene is drawn on the generated frame with it.
    pub camera: ViewCamera,
    /// Whether to show it: not after a cut or a reset, nor on the first frame (it is then
    /// the real frame itself).
    pub shown: bool,
    /// The last real frame's camera (this frame's on the first frame).
    pub previous: ViewCamera,
}

/// Composes a whole frame for each view (see the crate documentation): the scene passes
/// at render resolution, temporal antialiasing or upscaling, then bloom, exposure and tone
/// mapping at output resolution. One per renderer configuration (output format, depth
/// format, shadows); views keep their own state ([`View`]).
pub struct FrameRenderer {
    forward: ForwardRenderer,
    tonemap: Tonemap,
    output_format: vk::Format,
    bloom: Bloom,
    depth_format: vk::Format,
    frames_in_flight: u64,
    /// Sample counts the scene's targets support, by [`Msaa`].
    msaa_support: [bool; 4],
    /// The resolve of multisampled scenes (ADR 0075), made per sample count.
    msaa: Box<MsaaResolve>,
    /// The filters after tone mapping (ADR 0075), made as views ask for them.
    post: Box<PostAa>,
    /// The tone mapper into their perceptual image, made with the first filter.
    perceptual: Option<Box<Tonemap>>,
    /// Tone mappers that resample a smaller scene (Catmull-Rom, Lanczos-3; ADR 0075),
    /// into the output and into the perceptual image ([`resampling_slot`]), made with the
    /// first view that asks.
    resampling: [Option<Box<Tonemap>>; 4],
    /// FSR 1 (ADR 0075), made with the first view that uses it.
    fsr1: Option<Box<Fsr1>>,
    /// Depth of field (ADR 0095), made with the first view that uses it.
    dof: Option<Box<DepthOfField>>,
    /// Motion blur (ADR 0096), made with the first cinematic view that uses it.
    motion_blur: Option<Box<MotionBlur>>,
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

/// The filter after tone mapping of `post`.
fn post_filter(post: PostAntialiasing) -> Option<PostFilter> {
    match post {
        PostAntialiasing::Off => None,
        PostAntialiasing::Fxaa => Some(PostFilter::Fxaa),
        PostAntialiasing::Mlaa => Some(PostFilter::Smaa(SmaaPreset::Mlaa)),
        PostAntialiasing::Smaa(quality) => Some(PostFilter::Smaa(match quality {
            SmaaQuality::Low => SmaaPreset::Low,
            SmaaQuality::Medium => SmaaPreset::Medium,
            SmaaQuality::High => SmaaPreset::High,
            SmaaQuality::Ultra => SmaaPreset::Ultra,
        })),
    }
}

/// Whether step 4 brings the image to the output size (else the tone mapper scales it).
fn resolves_to_output(upscale: Upscale) -> bool {
    matches!(upscale, Upscale::Temporal(_) | Upscale::Fsr1)
}

/// The tone mapper's kernel for `upscale`, when it resamples (a scene smaller than the
/// output: at the output size both kernels give the scene's pixels back).
fn resample(upscale: Upscale, render: (u32, u32), output: (u32, u32)) -> Option<Resample> {
    match upscale {
        _ if render == output => None,
        Upscale::CatmullRom => Some(Resample::CatmullRom),
        Upscale::Lanczos3 => Some(Resample::Lanczos3),
        _ => None,
    }
}

/// Where [`FrameRenderer`] keeps the resampling tone mapper of `resample`, into the
/// output or into the perceptual image.
fn resampling_slot(resample: Resample, perceptual: bool) -> usize {
    usize::from(resample == Resample::Lanczos3) * 2 + usize::from(perceptual)
}

/// The nominal render size (what `quality` gives: the LOD bias follows it), the render
/// size this frame, and for integer scaling the factor and the scaled image's offset.
#[allow(clippy::type_complexity)]
fn render_sizes(
    settings: &ViewSettings,
    upscaler: Option<&dyn Upscaler>,
    upscale: Upscale,
    output: (u32, u32),
) -> ((u32, u32), (u32, u32), Option<(u32, (u32, u32))>) {
    let clamp = |(w, h): (u32, u32)| (w.clamp(1, output.0), h.clamp(1, output.1));
    if upscale == Upscale::Integer {
        let base = match settings.render_size {
            Some(size) => clamp(size),
            None => {
                let factor = (settings
                    .render_scale
                    .map_or(settings.quality.ratio(), |s| 1.0 / s.clamp(0.05, 1.0)))
                .round()
                .max(1.0) as u32;
                clamp((output.0 / factor, output.1 / factor))
            }
        };
        let factor = (output.0 / base.0).min(output.1 / base.1).max(1);
        let shown = (base.0 * factor, base.1 * factor);
        let offset = (
            (output.0.saturating_sub(shown.0)) / 2,
            (output.1.saturating_sub(shown.1)) / 2,
        );
        return (base, base, Some((factor, offset)));
    }
    if let Some(size) = settings.render_size {
        let size = clamp(size);
        return (size, size, None);
    }
    let nominal = match upscaler {
        Some(upscaler) => upscaler.render_size(settings.quality, output),
        None => settings.quality.render_size(output),
    };
    let render = settings
        .render_scale
        .map_or(nominal, |scale| scaled(output, scale));
    (nominal, render, None)
}

/// The sample counts the scene's targets support on `device`: the framebuffer limits and
/// every format a multisampled view renders into.
fn msaa_support(device: &Device, depth_format: vk::Format) -> [bool; 4] {
    let limits = &device.adapter().limits;
    let mut counts = limits.framebuffer_color_sample_counts
        & limits.framebuffer_depth_sample_counts
        & limits.framebuffer_no_attachments_sample_counts;
    let formats = [
        (HDR_FORMAT, vk::ImageUsageFlags::COLOR_ATTACHMENT),
        (
            engine_render::forward::MOTION_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
        ),
        (
            engine_render::forward::REACTIVE_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
        ),
        (
            engine_render::reflections::NORMAL_ROUGHNESS_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
        ),
        (
            engine_render::reflections::WEIGHT_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
        ),
        (
            engine_render::subsurface::IRRADIANCE_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
        ),
        (
            engine_render::subsurface::ALBEDO_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
        ),
        (depth_format, vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT),
    ];
    for (format, usage) in formats {
        // SAFETY: a query on the device's own physical device.
        let properties = unsafe {
            device
                .instance_raw()
                .get_physical_device_image_format_properties(
                    device.physical(),
                    format,
                    vk::ImageType::TYPE_2D,
                    vk::ImageTiling::OPTIMAL,
                    usage | vk::ImageUsageFlags::SAMPLED,
                    vk::ImageCreateFlags::empty(),
                )
        };
        counts &= properties.map_or(vk::SampleCountFlags::TYPE_1, |p| p.sample_counts);
    }
    core::array::from_fn(|k| k == 0 || counts.contains(Msaa::ALL[k].flags()))
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
            output_format,
            bloom: Bloom::new(device, bindless, BloomSettings::default())?,
            depth_format,
            frames_in_flight: frames_in_flight.max(1) as u64,
            msaa_support: msaa_support(device, depth_format),
            msaa: Box::new(MsaaResolve::new()),
            post: Box::new(PostAa::new(output_format)),
            perceptual: None,
            resampling: Default::default(),
            fsr1: None,
            dof: None,
            motion_blur: None,
            #[cfg(feature = "dlss")]
            streamline: None,
        })
    }

    /// What this GPU offers for antialiasing (ADR 0075): the sample counts the scene's
    /// targets support and the temporal upscalers that run here. Spatial scalers and the
    /// post filters run everywhere.
    pub fn antialiasing_support(&self) -> AntialiasingSupport {
        AntialiasingSupport {
            msaa: self.msaa_support,
            upscalers: [
                self.available(UpscalerKind::Taau),
                self.available(UpscalerKind::Fsr),
                self.available(UpscalerKind::Dlss),
            ],
        }
    }

    /// Whether `settings`' antialiasing can run on this GPU at an `output` size: the
    /// error [`Self::prepare`] would refuse it with. A temporal upscaler that is not
    /// available here is not an error (the view falls back to TAAU); ask
    /// [`Self::available`] to leave it out of a menu.
    pub fn check(
        &self,
        settings: &ViewSettings,
        output: (u32, u32),
    ) -> Result<(), InvalidAntialiasing> {
        self.check_antialiasing(settings.antialiasing, settings, output)
    }

    fn check_antialiasing(
        &self,
        antialiasing: Antialiasing,
        settings: &ViewSettings,
        output: (u32, u32),
    ) -> Result<(), InvalidAntialiasing> {
        let output = (output.0.max(1), output.1.max(1));
        let (_, render, _) = render_sizes(settings, None, antialiasing.upscale, output);
        antialiasing.validate(render == output)?;
        if !self
            .antialiasing_support()
            .supports_samples(antialiasing.samples)
        {
            return Err(InvalidAntialiasing::SamplesUnsupported(
                antialiasing.samples,
            ));
        }
        Ok(())
    }

    /// Frees what the renderer keeps in device memory: the scene passes' local shadow
    /// atlases (ADR 0071) and SMAA's tables (ADR 0075). Views are freed apart
    /// ([`View::destroy`]).
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.forward.destroy(device, allocator, bindless);
            self.post.destroy(device, allocator, bindless);
        }
    }

    /// The format the tone mapper writes (the output's, or its HDR intermediate's).
    pub fn output_format(&self) -> vk::Format {
        self.output_format
    }

    /// Writes `format` from now on (HDR output turned on or off, ADR 0038): the tone
    /// mapper and the filters after it are made for it, the resampling tone mappers made
    /// again when next asked for. The scene passes do not depend on it.
    ///
    /// # Errors
    ///
    /// When the tone mapper cannot be made (the renderer then still writes the old
    /// format).
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the old pipelines and filter tables (the device is idle).
    pub unsafe fn set_output_format(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        format: vk::Format,
    ) -> Result<(), RhiError> {
        if format == self.output_format {
            return Ok(());
        }
        let tonemap = Tonemap::new(device, bindless, format)?;
        self.tonemap = tonemap;
        // SAFETY: guaranteed by the caller.
        unsafe { self.post.destroy(device, allocator, bindless) };
        *self.post = PostAa::new(format);
        self.resampling = Default::default();
        self.output_format = format;
        Ok(())
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

    /// Whether frame generation of `kind` can run here (ADR 0091): FSR's when built in
    /// (feature `fsr`). Presenting it also needs a paced presenter
    /// (`engine_latency::PacedPresenter::start` says whether the device has what it
    /// needs).
    pub fn frame_generation_available(&self, kind: FrameGeneration) -> bool {
        match kind {
            FrameGeneration::Off => true,
            FrameGeneration::Fsr => cfg!(feature = "fsr"),
            #[cfg(feature = "dlss")]
            FrameGeneration::Dlss => self
                .streamline
                .as_ref()
                .is_some_and(engine_dlss::Streamline::frame_generation_supported),
            #[cfg(not(feature = "dlss"))]
            FrameGeneration::Dlss => false,
        }
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
        &mut self,
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
            skinning,
        } = ctx;
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        if !view.retired.is_empty() {
            view.collect(allocator, bindless, Some(completed));
        }
        // Material types' pipelines asked for by the last frames, and their hot reload
        // (ADR 0092): pipelines a reload replaced go once the frames using them are done.
        self.forward.collect_material_types(frame_number, completed);
        self.forward.update_material_types(device);
        let output = (output.0.max(1), output.1.max(1));
        let previous = view.prepared;
        // What was asked must be valid (as `check` says), whatever it falls back to.
        self.check_antialiasing(settings.antialiasing, settings, output)?;
        let mut antialiasing = match settings.antialiasing.upscale {
            Upscale::Temporal(kind) if !self.available(kind) => Antialiasing {
                upscale: Upscale::Temporal(UpscalerKind::Taau),
                ..settings.antialiasing
            },
            _ => settings.antialiasing,
        };
        if previous.is_none_or(|p| p.antialiasing.upscale != antialiasing.upscale) {
            let upscaler = match antialiasing.upscaler() {
                Some(kind) => match self.create_upscaler(kind, device, bindless)? {
                    Some(upscaler) => Some(upscaler),
                    None => {
                        antialiasing.upscale = Upscale::Temporal(UpscalerKind::Taau);
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
        #[cfg(feature = "fsr")]
        match (
            settings.frame_generation == FrameGeneration::Fsr,
            view.interpolation.is_some(),
        ) {
            (true, false) => {
                view.interpolation = Some(engine_fsr::FrameInterpolation::new(
                    device,
                    self.frames_in_flight as usize,
                ));
                view.previous_camera = None;
            }
            (false, true) => {
                if let Some(old) = view.interpolation.take() {
                    view.retire(frame_number, Retired::Interpolation(old));
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
        // The sun's virtual shadow pages (ADR 0093 step 3): made the first frame they are
        // wanted (with the renderer's culls and pipelines for them), made again when their
        // settings change; a device without what they need (fragment atomics, clip
        // distances) keeps the cascades alone.
        let pages = view.virtual_shadows.as_ref().map(|(made, _)| *made);
        match (settings.virtual_shadows, pages) {
            (Some(wanted), made) if made != Some(wanted) => {
                if self.forward.set_virtual_shadows(device, bindless, true)? {
                    let pages = VirtualShadows::new(device, allocator, bindless, wanted)?;
                    if let Some((_, old)) = view.virtual_shadows.replace((wanted, pages)) {
                        view.retire(frame_number, Retired::VirtualShadows(old));
                    }
                }
            }
            (None, Some(_)) => {
                if let Some((_, old)) = view.virtual_shadows.take() {
                    view.retire(frame_number, Retired::VirtualShadows(old));
                }
            }
            _ => {}
        }
        match (settings.cached_shadows, view.shadow_cache.is_some()) {
            (true, false) => {
                let shadows = self.forward.shadow_settings();
                view.shadow_cache = Some(CascadeCache::new(device, allocator, bindless, shadows)?);
            }
            (false, true) => {
                if let Some(old) = view.shadow_cache.take() {
                    view.retire(frame_number, Retired::ShadowCache(old));
                }
            }
            _ => {}
        }
        // Color grading (ADR 0094): the view's LUT, made the first frame it grades and kept
        // while it lives, so that a grading animated back to neutral and away again (a
        // damage flash) makes nothing again.
        if (!settings.grading.is_neutral() || !settings.color_vision.is_off())
            && view.grading.is_none()
        {
            view.grading = Some(GradingLut::new(device, allocator, bindless)?);
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

        let (nominal_render, render, integer) = render_sizes(
            settings,
            view.upscaler.as_deref(),
            antialiasing.upscale,
            output,
        );
        let pattern = if antialiasing.upscale == Upscale::SmaaT2x {
            smaa_jitter(antialiasing.samples.count() as usize)
        } else {
            &[]
        };
        let (jitter_phases, lod_bias) = match &view.upscaler {
            Some(upscaler) => (
                upscaler.jitter_phases(render, output),
                upscaler.texture_lod_bias(nominal_render, output),
            ),
            None => (pattern.len() as u32, 0.0),
        };
        if let Some(temporal) = &view.temporal {
            temporal.set_jitter_phases(jitter_phases);
            temporal.set_jitter_pattern(pattern);
        }

        // MSAA: the scene passes' pipelines for the sample count, the resolve's, the
        // skinned motion pass's.
        if antialiasing.samples != Msaa::X1 {
            let flags = antialiasing.samples.flags();
            self.forward.set_samples(device, bindless, flags)?;
            self.msaa
                .ensure(device, bindless, flags, antialiasing.smaa_s2x())?;
            if let Some(skinning) = skinning {
                skinning.set_motion_samples(device, bindless, flags)?;
            }
        }
        if antialiasing.upscale == Upscale::Fsr1 && self.fsr1.is_none() {
            self.fsr1 = Some(Box::new(Fsr1::new(device, bindless)?));
        }
        if settings.depth_of_field.is_some() && self.dof.is_none() {
            self.dof = Some(Box::new(DepthOfField::new(device, bindless)?));
        }
        if settings.kind.motion_blur().is_some() && self.motion_blur.is_none() {
            self.motion_blur = Some(Box::new(MotionBlur::new(device, bindless)?));
        }
        // Catmull-Rom and Lanczos-3: the tone mapper with their kernel.
        if let Some(kernel) = resample(antialiasing.upscale, render, output) {
            let perceptual = post_filter(antialiasing.post).is_some();
            let slot = &mut self.resampling[resampling_slot(kernel, perceptual)];
            if slot.is_none() {
                let format = (!perceptual).then_some(self.output_format);
                *slot = Some(Box::new(Tonemap::with_resample(
                    device, bindless, format, kernel,
                )?));
            }
        }

        // Filters after tone mapping: their pipelines, and SMAA T2x's last frame.
        if let Some(filter) = post_filter(antialiasing.post) {
            // (Catmull-Rom and Lanczos-3 have their own, above.)
            let kernel = resample(antialiasing.upscale, render, output);
            if self.perceptual.is_none() && kernel.is_none() {
                self.perceptual = Some(Box::new(Tonemap::new_perceptual(device, bindless)?));
            }
            let temporal = antialiasing.upscale == Upscale::SmaaT2x;
            self.post.ensure(
                device,
                allocator,
                bindless,
                filter,
                temporal,
                antialiasing.smaa_s2x(),
            )?;
        }
        let keep_post = antialiasing.upscale == Upscale::SmaaT2x;
        if view
            .post_history
            .as_ref()
            .is_some_and(|h| !keep_post || h.size != output)
            && let Some(old) = view.post_history.take()
        {
            view.retire(frame_number, Retired::History(old));
        }
        if keep_post && view.post_history.is_none() {
            view.post_history = Some(HistoryImage::new(
                device,
                allocator,
                bindless,
                PERCEPTUAL_FORMAT,
                output,
            )?);
        }
        if materials.texture_lod_bias() != lod_bias {
            materials.set_texture_lod_bias(bindless, lod_bias)?;
        }

        // The previous antialiased color: at the output size, or the render size without
        // an upscaler (nothing brings the image to the output size before the post chain).
        let history_size = if resolves_to_output(antialiasing.upscale) {
            output
        } else {
            render
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

        // Screen-space reflections' history, at the render size.
        match settings.screen_space_reflections {
            Some(ssr) => {
                let size = [render.0, render.1];
                if view.reflections.as_ref().is_some_and(|r| r.size() != size)
                    && let Some(old) = view.reflections.take()
                {
                    view.retire(frame_number, Retired::Reflections(old));
                }
                match &view.reflections {
                    Some(r) => r.set_settings(ssr),
                    None => {
                        view.reflections = Some(ScreenSpaceReflections::new(
                            device, allocator, bindless, render.0, render.1, ssr,
                        )?);
                    }
                }
            }
            None => {
                if let Some(old) = view.reflections.take() {
                    view.retire(frame_number, Retired::Reflections(old));
                }
            }
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
            integer,
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
    /// [`FrameError::RingFull`] when the upload ring had no room (nothing was declared; it
    /// grows for the next frames), [`FrameError::Upscale`] when the upscaler refused its
    /// pass.
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
        self.declare_view(fg, ctx, view, scene, camera, target, None, hook)
    }

    /// [`Self::add_view_with`] with frame generation (ADR 0091): `target` is a paced
    /// presenter's real frame, and with [`ViewSettings::frame_generation`] on, the frame
    /// between the view's last frame and this one goes into `generated` after the real
    /// one is drawn (no UI in either yet: draw it with [`crate::UiRenderer`] on both).
    /// [`ViewFrame::generated`] says whether to show it and gives its camera.
    ///
    /// # Errors
    ///
    /// As [`Self::add_view`].
    #[allow(clippy::too_many_arguments)]
    pub fn add_view_generating<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ctx: &mut FrameContext<'_, 'g>,
        view: &'g mut View,
        scene: &SceneInput<'_, 'g>,
        camera: &ViewCamera,
        target: ViewTarget,
        generated: GeneratedTarget,
        hook: impl FnOnce(&mut FrameGraph<'g>, &SceneTargets<'_>),
    ) -> Result<ViewFrame, FrameError> {
        self.declare_view(fg, ctx, view, scene, camera, target, Some(generated), hook)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn declare_view<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ctx: &mut FrameContext<'_, 'g>,
        view: &'g mut View,
        scene: &SceneInput<'_, 'g>,
        camera: &ViewCamera,
        target: ViewTarget,
        generated_target: Option<GeneratedTarget>,
        hook: impl FnOnce(&mut FrameGraph<'g>, &SceneTargets<'_>),
    ) -> Result<ViewFrame, FrameError> {
        engine_profiler::zone!("frame::FrameRenderer::add_view");
        // The ring frees what it replaced and grows ahead when frames come close to its
        // capacity (core review 2 §3.6): a full ring would drop the view.
        ctx.ring.grow(ctx.device, ctx.allocator)?;
        let View {
            prepared,
            temporal,
            occlusion,
            shadow_cache,
            virtual_shadows,
            upscaler,
            exposure,
            grading,
            history,
            reflections,
            post_history,
            resets,
            reset_pending,
            #[cfg(feature = "fsr")]
            interpolation,
            previous_camera,
            ..
        } = view;
        let prepared = (*prepared).ok_or(FrameError::NotPrepared)?;
        let settings = prepared.settings;
        // Whether this frame continues the last one: frame generation interpolates only
        // between frames that do.
        let continues = !(camera.cut || *reset_pending) && previous_camera.is_some();
        let last_camera = previous_camera.replace(*camera);
        if camera.cut || core::mem::take(reset_pending) {
            if camera.cut {
                resets.cuts += 1;
                // The cascades' fit kept from before the cut (ADR 0093) goes with it.
                if let Some(cache) = shadow_cache.as_ref() {
                    cache.invalidate();
                }
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
            if let Some(history) = post_history.as_mut() {
                history.valid = false;
            }
            if let Some(ssr) = reflections.as_ref() {
                ssr.reset();
            }
        }
        let temporal: Option<&'g Temporal> = temporal.as_ref();
        let occlusion: Option<&'g Occlusion> = occlusion.as_ref();
        let shadow_cache: Option<&'g CascadeCache> = shadow_cache.as_ref();
        let virtual_shadows: Option<&'g VirtualShadows> = virtual_shadows
            .as_ref()
            .map(|(_, pages)| pages)
            .filter(|_| settings.virtual_shadows.is_some());
        let upscaler: Option<&'g mut Box<dyn Upscaler>> = upscaler.as_mut();
        let exposure: Option<&'g AutoExposure> = exposure.as_ref();
        let grading: Option<&'g GradingLut> = grading.as_ref();
        let (render, output) = (prepared.render, prepared.output);

        // 1. The scene at render resolution, jittered, with motion vectors when temporal;
        // multisampled with MSAA (resolved after the skinned motion pass, below).
        let samples = prepared.antialiasing.samples.flags();
        let scene_desc = |format| TextureDesc {
            samples,
            ..TextureDesc::new_2d(format, render.0, render.1)
        };
        let color = fg.create_texture("hdr", scene_desc(HDR_FORMAT));
        let depth = fg.create_texture("depth", scene_desc(self.depth_format));
        // The projection's aspect is the shown image's: the output's, or with integer
        // scaling the render size's (its scaled rectangle keeps it, black around).
        let aspect = match prepared.integer {
            Some(_) => render.0 as f32 / render.1 as f32,
            None => output.0 as f32 / output.1 as f32,
        };
        let scene_camera = Camera {
            clip_from_world: camera.clip_from_world(aspect),
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
            shadow_cache,
            virtual_shadows,
            lights: scene.lights,
            decals: scene.decals,
            environment: scene.environment,
            reflections: Reflections {
                probes: scene.reflection_probes,
                ssr: reflections.as_ref(),
            },
            gi: scene.gi,
            features: prepared.settings.features,
        };
        let mut passes = match scene.draws {
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
        // MSAA: the samples resolved (color weighted by luminance, motion vectors of the
        // nearest sample, depth by the forward renderer); with SMAA S2x also half the two
        // samples' difference. Everything after reads one value per pixel.
        let mut difference = None;
        let (color, depth) = match passes.resolved_depth {
            Some(resolved_depth) => {
                let resolved = self
                    .msaa
                    .add_pass(
                        fg,
                        ctx.bindless,
                        MsaaInput {
                            color,
                            depth,
                            motion: passes.motion.map(|m| m.velocity),
                            split: prepared.antialiasing.smaa_s2x(),
                        },
                    )
                    .ok_or(FrameError::Antialiasing(
                        InvalidAntialiasing::SamplesUnsupported(prepared.antialiasing.samples),
                    ))?;
                if let (Some(motion), Some(velocity)) = (passes.motion.as_mut(), resolved.motion) {
                    motion.velocity = velocity;
                }
                difference = resolved.difference;
                (resolved.color, resolved_depth)
            }
            None => (color, depth),
        };

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
        let resolved = match (prepared.antialiasing.upscale, upscaler, passes.motion) {
            (Upscale::Temporal(_), Some(upscaler), Some(motion)) => {
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
            (Upscale::Fsr1, ..) => match &self.fsr1 {
                Some(fsr1) => fsr1.add_passes(
                    fg,
                    ctx.bindless,
                    color,
                    render,
                    output,
                    Fsr1Exposure {
                        value: exposure_value,
                        auto: auto_exposure,
                    },
                    settings.sharpness,
                ),
                None => color,
            },
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

        // 4b. Depth of field (ADR 0095), on the antialiased image with the render-size
        // depth: after the upscalers (AMD's FSR guide places it there; they reproject a
        // sharp image along its motion). The previous color above stays sharp.
        let resolved = match (&settings.depth_of_field, self.dof.as_deref()) {
            (Some(dof_settings), Some(dof)) => dof.add_passes(
                fg,
                ctx.bindless,
                resolved,
                depth,
                DofCamera {
                    fov_y: camera.fov_y,
                    near: camera.near,
                    far: camera.far,
                },
                dof_settings,
            ),
            _ => resolved,
        };
        // 4c. Motion blur (ADR 0096), on cinematic views only (ADR 0067): after depth of
        // field (its streaks are of the image as focused), with the motion vectors and the
        // depth at render size.
        let resolved = match (
            settings.kind.motion_blur(),
            self.motion_blur.as_deref(),
            passes.motion,
        ) {
            (Some(blur_settings), Some(blur), Some(motion)) => blur.add_passes(
                fg,
                ctx.bindless,
                resolved,
                motion.velocity,
                depth,
                MotionBlurCamera {
                    near: camera.near,
                    far: camera.far,
                },
                blur_settings,
            ),
            _ => resolved,
        };

        // 5. Bloom and tone mapping, at the output size.
        let bloom = settings
            .bloom
            .then(|| self.bloom.add_passes(fg, ctx.bindless, resolved));
        // With a filter after tone mapping, the tone mapper writes its perceptual image
        // at the output size, and the filter writes the view's target.
        let filter = post_filter(prepared.antialiasing.post);
        // SMAA S2x: each sample's image (the resolved color plus or minus half their
        // difference) is tone mapped and antialiased on its own.
        let sources = match (difference, filter) {
            (Some(difference), Some(PostFilter::Smaa(_))) => {
                match self
                    .post
                    .add_split(fg, ctx.bindless, resolved, difference, render)
                {
                    Some([a, b]) => [a, b],
                    None => [resolved, resolved],
                }
            }
            _ => [resolved, resolved],
        };
        let two = sources[0] != sources[1];
        // Catmull-Rom and Lanczos-3 resample in the tone mapper (made by `prepare`).
        let kernel = resample(prepared.antialiasing.upscale, render, output);
        let resampling = |perceptual: bool| {
            kernel.and_then(|k| self.resampling[resampling_slot(k, perceptual)].as_deref())
        };
        let perceptual_tonemap = resampling(true).or(self.perceptual.as_deref());
        let perceptual = filter.and(perceptual_tonemap).map(|tonemap| {
            let desc = TextureDesc::new_2d(PERCEPTUAL_FORMAT, output.0, output.1);
            let image = fg.create_texture("perceptual", desc);
            let second = two.then(|| fg.create_texture("perceptual", desc));
            (tonemap, image, second)
        });
        let whole = |offset: vk::Offset2D| vk::Rect2D {
            offset,
            extent: vk::Extent2D {
                width: output.0,
                height: output.1,
            },
        };
        let (tonemap, tonemapped, area, load) = match perceptual {
            Some((tonemap, image, _)) => (
                tonemap,
                image,
                whole(vk::Offset2D::default()),
                LoadOp::DontCare,
            ),
            None => (
                resampling(false).unwrap_or(&self.tonemap),
                target.texture,
                target
                    .rect
                    .unwrap_or_else(|| whole(vk::Offset2D::default())),
                target.load,
            ),
        };
        // Nearest and integer scaling sample the scene's pixels as they are; integer
        // scaling draws them at a whole factor, centered, black around.
        let (rect, letterbox) = match prepared.integer {
            Some((factor, (x, y))) => (
                Some(vk::Rect2D {
                    offset: vk::Offset2D {
                        x: area.offset.x + x as i32,
                        y: area.offset.y + y as i32,
                    },
                    extent: vk::Extent2D {
                        width: render.0 * factor,
                        height: render.1 * factor,
                    },
                }),
                (x > 0 || y > 0).then_some(area),
            ),
            None if perceptual.is_some() => (None, None),
            None => (target.rect, None),
        };
        let sampler = match prepared.antialiasing.upscale {
            Upscale::Nearest | Upscale::Integer => SamplerKind::NearestClamp,
            _ => SamplerKind::LinearClamp,
        };
        // Through the view's LUT while it grades or corrects for a color vision deficiency
        // (neither: the plain curve).
        let grading = grading
            .filter(|_| !settings.grading.is_neutral() || !settings.color_vision.is_off())
            .and_then(|lut| {
                lut.add_passes(
                    fg,
                    ctx.ring,
                    ctx.bindless,
                    &settings.grading,
                    settings.curve,
                    settings.peak,
                    &settings.color_vision,
                )
            });
        let composite = Composite {
            rect,
            sampler,
            exposure: exposure_value,
            tonemap: true,
            curve: settings.curve,
            peak: settings.peak,
            encoding: settings.encoding,
            load,
            bloom,
            auto_exposure,
            letterbox,
            grading,
        };
        tonemap.add_composite_pass(fg, ctx.bindless, sources[0], tonemapped, composite);
        let second = perceptual.and_then(|(_, _, second)| second);
        if let Some(second) = second {
            tonemap.add_composite_pass(fg, ctx.bindless, sources[1], second, composite);
        }

        // 6. The filter after tone mapping, into the view's target.
        if let (Some(filter), Some((_, image, _))) = (filter, perceptual) {
            let post_target = PostTarget {
                texture: target.texture,
                rect: target.rect,
                load: target.load,
                decode: match settings.encoding {
                    OutputEncoding::Linear { white } => Decode::Srgb {
                        scale: white * settings.peak,
                    },
                    OutputEncoding::Pq { .. } => Decode::None,
                },
            };
            match filter {
                PostFilter::Fxaa => {
                    self.post
                        .add_fxaa(fg, ctx.bindless, image, output, post_target)
                }
                PostFilter::Smaa(preset) => {
                    // SMAA T2x: this frame's jitter phase, last frame's result.
                    let temporal = match (prepared.antialiasing.upscale, passes.motion) {
                        (Upscale::SmaaT2x, Some(motion)) => post_history.as_mut().map(|h| {
                            let history = fg.import_texture(
                                "smaa history",
                                ImportedTexture {
                                    image: h.image,
                                    view: h.view,
                                    desc: TextureDesc::new_2d(h.format, h.size.0, h.size.1),
                                    initial: h.state,
                                    final_state: Some(TextureState::shader_read(
                                        ShaderStages::FRAGMENT,
                                    )),
                                    sampled_index: Some(h.sampled),
                                    storage_index: None,
                                },
                            );
                            h.state = TextureState::shader_read(ShaderStages::FRAGMENT);
                            let valid = core::mem::replace(&mut h.valid, true);
                            SmaaTemporal {
                                velocity: motion.velocity,
                                previous: valid.then_some(history),
                                history,
                                phase: usize::from(motion.jitter[0] > 0.0),
                            }
                        }),
                        _ => None,
                    };
                    self.post
                        .add_smaa(
                            fg,
                            ctx.bindless,
                            ctx.ring,
                            &SmaaInput {
                                images: [Some(image), second],
                                size: output,
                                preset,
                                temporal,
                            },
                            post_target,
                        )
                        .ok_or(FrameError::RingFull)?;
                }
            }
        }
        // 7. Frame generation (ADR 0091): the frame between the last real frame and this
        // one, from the target this frame was just drawn into (no UI in it yet).
        let mut generated = None;
        #[cfg(feature = "fsr")]
        if let (Some(out), FrameGeneration::Fsr, Some(motion), Some(interpolation)) = (
            generated_target,
            settings.frame_generation,
            passes.motion,
            interpolation.as_mut(),
        ) {
            // The first frame primes the interpolation (it keeps this frame as the
            // previous one); what it writes is this frame itself, not shown.
            let last = last_camera.unwrap_or(*camera);
            if !continues {
                interpolation.reset();
            }
            let transfer = match settings.encoding {
                OutputEncoding::Pq { white_nits } => engine_fsr::Transfer::Pq {
                    max_nits: white_nits * settings.peak,
                },
                OutputEncoding::Linear { white } if is_float(out.source_format) => {
                    engine_fsr::Transfer::ScRgb {
                        min_nits: 0.0,
                        max_nits: white * settings.peak * 80.0,
                    }
                }
                OutputEncoding::Linear { .. } => engine_fsr::Transfer::Srgb,
            };
            let rotation = Quat::from_mat4(&camera.view_from_world).conjugate();
            interpolation
                .add_pass(
                    fg,
                    engine_fsr::InterpolationInput {
                        source: target.texture,
                        source_format: out.source_format,
                        transfer,
                        depth,
                        motion,
                        camera: engine_fsr::FsrCamera {
                            near: camera.near,
                            far: camera.far,
                            fov_y: camera.fov_y,
                            meters_per_unit: 1.0,
                        },
                        basis: engine_fsr::CameraBasis {
                            position: camera.position.to_array(),
                            up: (rotation * Vec3::Y).to_array(),
                            right: (rotation * Vec3::X).to_array(),
                            forward: (rotation * Vec3::NEG_Z).to_array(),
                        },
                        frame_time_ms: ctx.dt * 1000.0,
                    },
                    engine_fsr::InterpolationOutput {
                        texture: out.texture,
                        format: out.format,
                    },
                    ctx.frame_number,
                )
                .map_err(|e| FrameError::Upscale(UpscaleError::from(e)))?;
            generated = Some(GeneratedFrame {
                camera: halfway(&last, camera),
                shown: continues,
                previous: last,
            });
        }
        // DLSS frame generation generates inside Streamline: what the frame hands it
        // (`ViewFrame::dlss_generation_input`) needs the last camera and whether this frame
        // continues it.
        if settings.frame_generation == FrameGeneration::Dlss && passes.motion.is_some() {
            let last = last_camera.unwrap_or(*camera);
            generated = Some(GeneratedFrame {
                camera: halfway(&last, camera),
                shown: continues,
                previous: last,
            });
        }
        #[cfg(not(feature = "fsr"))]
        let _ = generated_target;
        Ok(ViewFrame {
            antialiasing: prepared.antialiasing,
            render,
            output,
            passes,
            resolved,
            exposure: auto_exposure,
            depth,
            generated,
        })
    }
}

/// Whether `format` holds floats (an scRGB output).
#[cfg(feature = "fsr")]
fn is_float(format: vk::Format) -> bool {
    matches!(
        format,
        vk::Format::R16G16B16A16_SFLOAT | vk::Format::R32G32B32A32_SFLOAT
    )
}

/// The camera half-way between `a` and `b` (a generated frame's): positions and fields
/// of view blended, rotations by spherical interpolation.
pub(crate) fn halfway(a: &ViewCamera, b: &ViewCamera) -> ViewCamera {
    let ra = Quat::from_mat4(&a.view_from_world).conjugate();
    let rb = Quat::from_mat4(&b.view_from_world).conjugate();
    let rotation = ra.slerp(rb, 0.5).normalize();
    let position = a.position.lerp(b.position, 0.5);
    ViewCamera {
        fov_y: 0.5 * (a.fov_y + b.fov_y),
        cut: false,
        ..ViewCamera::from_rigid(rotation, position, b.fov_y, b.near, b.far)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_upscale::Quality;

    #[test]
    fn render_scale_stays_within_the_output() {
        assert_eq!(scaled((1920, 1080), 0.5), (960, 540));
        assert_eq!(scaled((1920, 1080), 2.0), (1920, 1080));
        assert_eq!(scaled((1920, 1080), 0.0), (96, 54));
        assert_eq!(scaled((3, 3), 0.1), (1, 1));
    }

    #[test]
    fn integer_scaling_fits_whole_multiples() {
        let fixed = |size| ViewSettings {
            render_size: Some(size),
            ..ViewSettings::default()
        };
        // 320 × 180 fills 1920 × 1080 six times over, and 2560 × 1440 eight times.
        assert_eq!(
            render_sizes(&fixed((320, 180)), None, Upscale::Integer, (1920, 1080)),
            ((320, 180), (320, 180), Some((6, (0, 0))))
        );
        assert_eq!(
            render_sizes(&fixed((320, 180)), None, Upscale::Integer, (2560, 1440)).2,
            Some((8, (0, 0)))
        );
        // 1366 × 768: four times (1280 × 720), black bars around.
        assert_eq!(
            render_sizes(&fixed((320, 180)), None, Upscale::Integer, (1366, 768)).2,
            Some((4, (43, 24)))
        );
        // Without a fixed size: the factor nearest to the quality's ratio.
        let half = ViewSettings {
            quality: Quality::Performance,
            ..ViewSettings::default()
        };
        assert_eq!(
            render_sizes(&half, None, Upscale::Integer, (1920, 1080)),
            ((960, 540), (960, 540), Some((2, (0, 0))))
        );
        // Larger than the output: clamped, shown once.
        assert_eq!(
            render_sizes(&fixed((4000, 100)), None, Upscale::Integer, (1920, 1080)),
            ((1920, 100), (1920, 100), Some((1, (0, 490))))
        );
        // Other modes keep the quality's size, or the fixed one.
        assert_eq!(
            render_sizes(&half, None, Upscale::Nearest, (1920, 1080)).1,
            (960, 540)
        );
        assert_eq!(
            render_sizes(&fixed((320, 180)), None, Upscale::Fsr1, (1920, 1080)),
            ((320, 180), (320, 180), None)
        );
    }

    #[test]
    fn a_rigid_camera_matches_look_at() {
        let eye = Vec3::new(3.0, 2.0, 5.0);
        let target = Vec3::new(-1.0, 0.5, 0.0);
        let look = engine_core::math::look_at(eye, target, Vec3::Y);
        let rotation = Quat::from_mat4(&look.inverse());
        let camera = ViewCamera::from_rigid(rotation, eye, 1.0, 0.1, 100.0);
        for (a, b) in camera
            .view_from_world
            .to_cols_array()
            .iter()
            .zip(look.to_cols_array())
        {
            assert!((a - b).abs() < 1e-5, "{a} {b}");
        }
    }
}
