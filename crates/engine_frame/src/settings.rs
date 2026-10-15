//! What a view asks of its frame: antialiasing, render size, exposure, post-processing.

use crate::antialiasing::Antialiasing;
use engine_render::ao::{AoQuality, AoSettings};
use engine_render::dof::DofSettings;
use engine_render::grading::ColorGrading;
use engine_render::motion_blur::MotionBlurSettings;
use engine_render::post::{OutputEncoding, ToneCurve};
use engine_render::postfx::ExposureSettings;
use engine_render::reflections::SsrSettings;
use engine_rhi::{AdapterKind, Device};
use engine_upscale::Quality;

/// The exposure the tone mapper applies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Exposure {
    /// A fixed linear multiplier.
    Fixed(f32),
    /// Measured every frame on the scene at render resolution, before antialiasing, and
    /// adapted over time (ADR 0047); FSR and DLSS get the same exposure.
    Auto(ExposureSettings),
}

/// Frame generation (ADR 0091): frames generated between the real ones, the HUD never
/// interpolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum FrameGeneration {
    /// None: the default; the player turns it on (the owner, 2026-10-11).
    #[default]
    Off,
    /// FSR 3.1's frame interpolation and optical flow, built from the FidelityFX SDK's
    /// source: any GPU, nothing to install. A paced presenter
    /// (`engine_latency::PacedPresenter`) shows the frames, with the UI drawn on each.
    Fsr,
    /// DLSS frame generation through Streamline, from the user's DLLs (RTX 40 and newer):
    /// Streamline presents, and composites the UI layer over the frames it generates
    /// (`engine_dlss::DlssFrameGeneration`, [`crate::ViewFrame::dlss_generation_input`]).
    Dlss,
}

/// What one view's frame does. Change it any time: [`crate::FrameRenderer::prepare`]
/// applies the difference (an upscaler switch or a new output size starts the history
/// anew; other changes cost nothing).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewSettings {
    /// Antialiasing and scaling (ADR 0075; default: the engine's TAAU): raster samples,
    /// how the render size reaches the output, a filter after tone mapping.
    pub antialiasing: Antialiasing,
    /// The render size relative to the output, as each upscaler maps it
    /// (`Upscaler::render_size`): [`Quality::Native`] renders at the output size.
    pub quality: Quality,
    /// FSR 1's sharpening (RCAS, [`crate::Upscale::Fsr1`]), in stops: 0 is the
    /// strongest, each stop halves it (AMD's default 0.2).
    pub sharpness: f32,
    /// Dynamic resolution: the render size per axis as a share of the output, `(0, 1]`,
    /// overriding `quality` frame by frame (a GPU-time controller sets it). The texture
    /// LOD bias stays that of `quality` (changing it rewrites every material); keep the
    /// scale at or below `quality`'s and step it coarsely (each new size makes new
    /// transient images).
    pub render_scale: Option<f32>,
    /// A fixed render size in pixels (pixel art, a retro resolution), overriding
    /// `quality` and `render_scale`; clamped to the output. With
    /// [`crate::Upscale::Integer`] the image is shown at the largest whole multiple that
    /// fits the output.
    pub render_size: Option<(u32, u32)>,
    /// Exposure (default fixed 1).
    pub exposure: Exposure,
    /// Tone curve.
    pub curve: ToneCurve,
    /// Color grading before the curve (ADR 0094; neutral by default, which costs
    /// nothing): white balance, channel mixer, shadows / mid-tones / highlights, the ASC
    /// CDL, contrast, vibrance and saturation, in one LUT with the curve. The view makes the
    /// LUT (864 KiB) the first frame it grades and keeps it; it is built again in the
    /// frames where the grading, the curve or the peak change.
    pub grading: ColorGrading,
    /// Where the tone mapper's roll-off ends, in units of reference white (1 for SDR;
    /// `OutputLevels::peak` for HDR displays, ADR 0038).
    pub peak: f32,
    /// How the output stores values (`OutputLevels::swapchain_encoding`).
    pub encoding: OutputEncoding,
    /// Bloom (ADR 0047; its settings are the frame renderer's, `FrameRenderer::bloom`).
    pub bloom: bool,
    /// Depth of field (ADR 0095; off by default, which costs nothing): a thin lens
    /// focused at a distance blurs what lies before and behind it, on the antialiased
    /// image before bloom and tone mapping. Its pipelines are made with the first view that
    /// uses it.
    pub depth_of_field: Option<DofSettings>,
    /// The color behind the scene where nothing is drawn (the sky covers it when the
    /// scene has an environment).
    pub clear: Option<[f32; 4]>,
    /// Two-phase occlusion culling (ADR 0035): what was visible last frame is drawn first,
    /// and hidden objects are skipped.
    pub occlusion_culling: bool,
    /// The sun's shadow cascades kept across frames (ADR 0093, on by default): each frame
    /// draws only the strips the camera's movement exposed and the old and new places of
    /// casters that moved, nothing for a still camera over a still scene. Costs the view a
    /// shadow map of its own (16 MiB with the default 4 cascades of 1024²).
    pub cached_shadows: bool,
    /// Motion vectors even without temporal antialiasing (for effects of your own).
    /// Temporal antialiasing always has them.
    pub motion_vectors: bool,
    /// Keep the view's antialiased HDR image for the next frame, for screen-space effects
    /// that reproject it (reflections): [`crate::SceneTargets::previous_color`]. Costs a
    /// copy at output size per frame, only when on. Implies motion vectors.
    pub previous_color: bool,
    /// Gameplay (the default) or cinematic (ADR 0067). Only a cinematic view has motion
    /// blur: its settings live in [`ViewKind::Cinematic`], so a gameplay view cannot ask
    /// for it.
    pub kind: ViewKind,
    /// Screen-space reflections (ADR 0073), traced at the render size against the
    /// scene's own lit image, over the scene's reflection probes and the sky. They need
    /// the scene's environment and `ForwardRenderer::set_reflections` (through
    /// `FrameRenderer::forward_mut`); the view keeps their history.
    pub screen_space_reflections: Option<SsrSettings>,
    /// Frame generation (ADR 0091), off by default; it implies motion vectors. FSR's needs
    /// the view's frame drawn with [`crate::FrameRenderer::add_view_generating`] into a
    /// paced presenter's images; DLSS's, the frame's inputs handed to Streamline
    /// ([`crate::ViewFrame::dlss_generation_input`]).
    pub frame_generation: FrameGeneration,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            antialiasing: Antialiasing::TAAU,
            quality: Quality::Native,
            sharpness: 0.2,
            render_scale: None,
            render_size: None,
            exposure: Exposure::Fixed(1.0),
            curve: ToneCurve::Gt,
            grading: ColorGrading::default(),
            peak: 1.0,
            encoding: OutputEncoding::LINEAR,
            bloom: true,
            depth_of_field: None,
            clear: Some([0.02, 0.02, 0.05, 1.0]),
            occlusion_culling: false,
            cached_shadows: true,
            motion_vectors: false,
            previous_color: false,
            kind: ViewKind::Gameplay,
            screen_space_reflections: None,
            frame_generation: FrameGeneration::Off,
        }
    }
}

impl ViewSettings {
    /// Whether the scene passes write motion vectors.
    pub fn wants_motion(&self) -> bool {
        self.antialiasing.is_temporal()
            || self.motion_vectors
            || self.previous_color
            || self.frame_generation != FrameGeneration::Off
            || self.kind.motion_blur().is_some()
    }
}

/// What a view shows (ADR 0067, team decision 2026-10-01).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ViewKind {
    /// The player's view, interactive. It has no motion blur: blur from the camera's own
    /// motion hides the scene when the player needs to read it, adds felt latency and
    /// makes some players unwell.
    #[default]
    Gameplay,
    /// A non-interactive view: a cutscene, a sequence. Switching a view to it is explicit
    /// (the editor shows it).
    Cinematic {
        /// Motion blur of the camera's and the objects' motion (ADR 0096; `None`: off).
        motion_blur: Option<MotionBlurSettings>,
    },
}

impl ViewKind {
    /// Whether it is cinematic.
    pub fn is_cinematic(&self) -> bool {
        matches!(self, Self::Cinematic { .. })
    }

    /// Its motion blur: only a cinematic view's.
    pub fn motion_blur(&self) -> Option<&MotionBlurSettings> {
        match self {
            Self::Gameplay => None,
            Self::Cinematic { motion_blur } => motion_blur.as_ref(),
        }
    }
}

/// The kind of GPU, for defaults that depend on its size (ADR 0072).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    /// A discrete GPU, or anything that is not a small integrated one.
    Discrete,
    /// A GPU sharing the CPU's memory (an APU): a few compute units, little bandwidth.
    Integrated,
}

impl DeviceClass {
    /// The class of `device`'s adapter.
    pub fn of(device: &Device) -> Self {
        match device.adapter().kind {
            AdapterKind::Integrated | AdapterKind::Cpu => Self::Integrated,
            _ => Self::Discrete,
        }
    }
}

/// Defaults chosen by measurement for a [`DeviceClass`] (ADR 0072): what a game starts
/// with before the player changes anything.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderDefaults {
    /// Ambient occlusion (`ForwardRenderer::set_ambient_occlusion`).
    pub ambient_occlusion: Option<AoSettings>,
    /// Antialiasing.
    pub antialiasing: Antialiasing,
    /// Render size relative to the output.
    pub quality: Quality,
    /// Culling and drawing by meshlet (ADR 0085, `ForwardRenderer::set_meshlet_culling`):
    /// the meshes with meshlets, in views with the depth prepass and in the sun's shadow
    /// cascades.
    pub meshlet_culling: bool,
}

impl RenderDefaults {
    /// The defaults for `class`.
    pub fn for_class(class: DeviceClass) -> Self {
        match class {
            DeviceClass::Discrete => Self {
                ambient_occlusion: Some(AoSettings::default()),
                antialiasing: Antialiasing::TAAU,
                quality: Quality::Native,
                // Dense scenes gain far more than light ones lose (ADR 0085): on an RTX
                // 4060 Ti with these settings Intel's Sponza (3.7 M triangles) 6.4 %,
                // 10,000 detailed spheres 3-8 %; Sponza costs 1.8 % (24 µs).
                meshlet_culling: true,
            },
            DeviceClass::Integrated => Self {
                // Without the bent normals: their registers cost the Radeon iGPU about
                // 20 % of its shading with local lights (ADR 0058).
                ambient_occlusion: Some(AoSettings {
                    quality: AoQuality::Low,
                    bent_normals: false,
                    ..AoSettings::default()
                }),
                antialiasing: Antialiasing::TAAU,
                quality: Quality::Quality,
                // The Radeon iGPU's few vertices and primitives a clock (ADR 0085): with
                // these settings Intel's Sponza gains 10.4 %, 10,000 detailed spheres
                // 10-17 %, Sponza about the same.
                meshlet_culling: true,
            },
        }
    }

    /// The defaults for `device`.
    pub fn for_device(device: &Device) -> Self {
        Self::for_class(DeviceClass::of(device))
    }

    /// View settings with these defaults and the others of [`ViewSettings::default`].
    pub fn view_settings(&self) -> ViewSettings {
        ViewSettings {
            antialiasing: self.antialiasing,
            quality: self.quality,
            ..ViewSettings::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_vectors_only_when_something_uses_them() {
        let off = ViewSettings {
            antialiasing: Antialiasing::OFF,
            ..ViewSettings::default()
        };
        assert!(!off.wants_motion());
        assert!(
            ViewSettings {
                previous_color: true,
                ..off
            }
            .wants_motion()
        );
        assert!(ViewSettings::default().wants_motion());
        // A cinematic view with motion blur needs them; one without does not.
        let cinematic = |motion_blur| ViewSettings {
            kind: ViewKind::Cinematic { motion_blur },
            ..off
        };
        assert!(cinematic(Some(MotionBlurSettings::default())).wants_motion());
        assert!(!cinematic(None).wants_motion());
    }

    #[test]
    fn only_a_cinematic_view_has_motion_blur() {
        assert_eq!(ViewKind::default(), ViewKind::Gameplay);
        assert!(ViewKind::Gameplay.motion_blur().is_none());
        assert!(!ViewKind::Gameplay.is_cinematic());
        let blur = MotionBlurSettings::default();
        let cinematic = ViewKind::Cinematic {
            motion_blur: Some(blur),
        };
        assert!(cinematic.is_cinematic());
        assert_eq!(cinematic.motion_blur(), Some(&blur));
    }

    #[test]
    fn defaults_by_device_class() {
        let discrete = RenderDefaults::for_class(DeviceClass::Discrete);
        assert_eq!(
            (discrete.antialiasing, discrete.quality),
            (Antialiasing::TAAU, Quality::Native)
        );
        assert_eq!(
            discrete.ambient_occlusion.map(|a| a.quality),
            Some(AoQuality::High)
        );
        assert!(discrete.meshlet_culling);
        let integrated = RenderDefaults::for_class(DeviceClass::Integrated);
        assert_eq!(integrated.quality, Quality::Quality);
        assert!(integrated.meshlet_culling);
        assert_eq!(
            integrated.ambient_occlusion.map(|a| a.quality),
            Some(AoQuality::Low)
        );
        assert_eq!(integrated.view_settings().quality, Quality::Quality);
    }
}
