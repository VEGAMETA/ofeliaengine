//! What a view asks of its frame: antialiasing, render size, exposure, post-processing.

use engine_render::ao::{AoQuality, AoSettings};
use engine_render::post::{OutputEncoding, ToneCurve};
use engine_render::postfx::ExposureSettings;
use engine_render::reflections::SsrSettings;
use engine_rhi::{AdapterKind, Device};
use engine_upscale::{Quality, UpscalerKind};

/// How a view is antialiased.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Antialiasing {
    /// None: no jitter, no motion vectors, no history. A render size below the output
    /// ([`ViewSettings::quality`]) is stretched bilinearly by the tone mapper.
    Off,
    /// A temporal upscaler accumulates jittered frames into the output: TAA when the view
    /// renders at the output size ([`Quality::Native`]; FSR's Native AA, NVIDIA's DLAA),
    /// upscaling below it. An upscaler that is not available on this machine
    /// ([`crate::FrameRenderer::available`]) falls back to the engine's TAAU.
    Temporal(UpscalerKind),
}

impl Antialiasing {
    /// The engine's TAAU (at [`Quality::Native`]: plain TAA).
    pub const TAAU: Self = Self::Temporal(UpscalerKind::Taau);
    /// FSR 3.1.
    pub const FSR: Self = Self::Temporal(UpscalerKind::Fsr);
    /// DLSS.
    pub const DLSS: Self = Self::Temporal(UpscalerKind::Dlss);

    /// The upscaler, if temporal.
    pub fn upscaler(self) -> Option<UpscalerKind> {
        match self {
            Self::Off => None,
            Self::Temporal(kind) => Some(kind),
        }
    }

    /// A short name for menus and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Temporal(kind) => kind.name(),
        }
    }
}

/// The exposure the tone mapper applies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Exposure {
    /// A fixed linear multiplier.
    Fixed(f32),
    /// Measured every frame on the scene at render resolution, before antialiasing, and
    /// adapted over time (ADR 0047); FSR and DLSS get the same exposure.
    Auto(ExposureSettings),
}

/// What one view's frame does. Change it any time: [`crate::FrameRenderer::prepare`]
/// applies the difference (an upscaler switch or a new output size starts the history
/// anew; other changes cost nothing).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewSettings {
    /// Antialiasing (default: the engine's TAAU).
    pub antialiasing: Antialiasing,
    /// The render size relative to the output, as each upscaler maps it
    /// (`Upscaler::render_size`): [`Quality::Native`] renders at the output size.
    pub quality: Quality,
    /// Dynamic resolution: the render size per axis as a share of the output, `(0, 1]`,
    /// overriding `quality` frame by frame (a GPU-time controller sets it). The texture
    /// LOD bias stays that of `quality` (changing it rewrites every material); keep the
    /// scale at or below `quality`'s and step it coarsely (each new size makes new
    /// transient images).
    pub render_scale: Option<f32>,
    /// Exposure (default fixed 1).
    pub exposure: Exposure,
    /// Tone curve.
    pub curve: ToneCurve,
    /// Where the tone mapper's roll-off ends, in units of reference white (1 for SDR;
    /// `OutputLevels::peak` for HDR displays, ADR 0038).
    pub peak: f32,
    /// How the output stores values (`OutputLevels::swapchain_encoding`).
    pub encoding: OutputEncoding,
    /// Bloom (ADR 0047; its settings are the frame renderer's, `FrameRenderer::bloom`).
    pub bloom: bool,
    /// The color behind the scene where nothing is drawn (the sky covers it when the
    /// scene has an environment).
    pub clear: Option<[f32; 4]>,
    /// Two-phase occlusion culling (ADR 0035): what was visible last frame is drawn first,
    /// and hidden objects are skipped.
    pub occlusion_culling: bool,
    /// Motion vectors even without temporal antialiasing (for effects of your own).
    /// Temporal antialiasing always has them.
    pub motion_vectors: bool,
    /// Keep the view's antialiased HDR image for the next frame, for screen-space effects
    /// that reproject it (reflections): [`crate::SceneTargets::previous_color`]. Costs a
    /// copy at output size per frame, only when on. Implies motion vectors.
    pub previous_color: bool,
    /// A non-interactive view (cutscene, sequence; ADR 0067): the only kind that may have
    /// camera motion blur once it exists. Gameplay views never get it.
    pub cinematic: bool,
    /// Screen-space reflections (ADR 0073), traced at the render size against the
    /// scene's own lit image, over the scene's reflection probes and the sky. They need
    /// the scene's environment and `ForwardRenderer::set_reflections` (through
    /// `FrameRenderer::forward_mut`); the view keeps their history.
    pub screen_space_reflections: Option<SsrSettings>,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            antialiasing: Antialiasing::TAAU,
            quality: Quality::Native,
            render_scale: None,
            exposure: Exposure::Fixed(1.0),
            curve: ToneCurve::Gt,
            peak: 1.0,
            encoding: OutputEncoding::LINEAR,
            bloom: true,
            clear: Some([0.02, 0.02, 0.05, 1.0]),
            occlusion_culling: false,
            motion_vectors: false,
            previous_color: false,
            cinematic: false,
            screen_space_reflections: None,
        }
    }
}

impl ViewSettings {
    /// Whether the scene passes write motion vectors.
    pub fn wants_motion(&self) -> bool {
        self.antialiasing != Antialiasing::Off || self.motion_vectors || self.previous_color
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
}

impl RenderDefaults {
    /// The defaults for `class`.
    pub fn for_class(class: DeviceClass) -> Self {
        match class {
            DeviceClass::Discrete => Self {
                ambient_occlusion: Some(AoSettings::default()),
                antialiasing: Antialiasing::TAAU,
                quality: Quality::Native,
            },
            DeviceClass::Integrated => Self {
                ambient_occlusion: Some(AoSettings {
                    quality: AoQuality::Low,
                    ..AoSettings::default()
                }),
                antialiasing: Antialiasing::TAAU,
                quality: Quality::Quality,
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
            antialiasing: Antialiasing::Off,
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
        let integrated = RenderDefaults::for_class(DeviceClass::Integrated);
        assert_eq!(integrated.quality, Quality::Quality);
        assert_eq!(
            integrated.ambient_occlusion.map(|a| a.quality),
            Some(AoQuality::Low)
        );
        assert_eq!(integrated.view_settings().quality, Quality::Quality);
    }
}
