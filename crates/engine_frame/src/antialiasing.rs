//! How a view is antialiased and scaled (ADR 0075): three independent parts, so a game
//! offers its players exactly the combinations it wants.
//!
//! ```text
//!  scene at render size           step 4: render size → output        after tone mapping
//!  ─────────────────────          ────────────────────────────        ──────────────────
//!  samples: 1 / 2 / 4 / 8   →     upscale: spatial (nearest,     →    post: FXAA, MLAA,
//!  (MSAA, alpha to coverage)      bilinear, Catmull-Rom,              SMAA (SMAA T2x's
//!                                 Lanczos-3, integer, FSR 1) or       temporal resolve
//!                                 temporal (TAAU, FSR, DLSS,          runs here)
//!                                 SMAA T2x's jitter)
//! ```
//!
//! [`crate::FrameRenderer::check`] says whether a combination runs on this GPU (the error
//! says why not), [`crate::FrameRenderer::antialiasing_support`] lists what the GPU
//! offers, and [`crate::FrameRenderer::prepare`] refuses an invalid one with the same
//! error. A game builds its menu from its own whitelist filtered by `check`.

use core::fmt;
use engine_rhi::ash::vk;
use engine_upscale::UpscalerKind;

/// Raster samples per pixel: multisample antialiasing (MSAA). Geometry edges get the
/// samples' coverage; shading still runs once per pixel. Masked (alpha-tested)
/// materials turn their alpha into coverage (alpha to coverage). Views with samples
/// always draw a depth prepass (the shading then tests depth for equality per sample).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Msaa {
    /// One sample: no MSAA.
    #[default]
    X1,
    /// 2 samples.
    X2,
    /// 4 samples.
    X4,
    /// 8 samples.
    X8,
}

impl Msaa {
    /// Every sample count, in increasing order.
    pub const ALL: [Self; 4] = [Self::X1, Self::X2, Self::X4, Self::X8];

    /// Samples per pixel.
    pub fn count(self) -> u32 {
        match self {
            Self::X1 => 1,
            Self::X2 => 2,
            Self::X4 => 4,
            Self::X8 => 8,
        }
    }

    /// The Vulkan sample count.
    pub fn flags(self) -> vk::SampleCountFlags {
        match self {
            Self::X1 => vk::SampleCountFlags::TYPE_1,
            Self::X2 => vk::SampleCountFlags::TYPE_2,
            Self::X4 => vk::SampleCountFlags::TYPE_4,
            Self::X8 => vk::SampleCountFlags::TYPE_8,
        }
    }

    /// The sample count of `count` samples per pixel, if it is one.
    pub fn from_count(count: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.count() == count)
    }
}

/// How the scene's image (at the render size) reaches the output size, after the hook
/// for scene effects and before bloom and tone mapping (ADR 0072's step 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Upscale {
    /// Each output pixel takes the nearest scene pixel: blocky, no blur (the tone mapper
    /// samples with a nearest sampler; no pass of its own).
    Nearest,
    /// Bilinear stretch by the tone mapper (no pass of its own). At the output size
    /// nothing happens: plain rendering.
    Bilinear,
    /// Bicubic stretch by the tone mapper with Catmull-Rom's kernel (4 × 4 scene pixels
    /// for each output pixel, no pass of its own): sharper than bilinear, limited to the
    /// four nearest scene pixels' range (no ringing around highlights). At the output size
    /// nothing happens.
    CatmullRom,
    /// Lanczos-3 stretch by the tone mapper (6 × 6 scene pixels for each output pixel, no
    /// pass of its own): the sharpest spatial filter, limited like
    /// [`Self::CatmullRom`]. At the output size nothing happens.
    Lanczos3,
    /// Pixel art: the largest whole multiple of the render size that fits the output,
    /// nearest-sampled and centered, the rest black (letterboxed). The render size is
    /// the output divided by the factor that `Quality` asks for (rounded to a whole
    /// number, at least 1), so every scene pixel is a square of output pixels.
    Integer,
    /// AMD FidelityFX Super Resolution 1 (EASU, then RCAS sharpening with
    /// `ViewSettings::sharpness`): edge-adaptive spatial upscaling, in a reversibly tone
    /// mapped space. At the output size only the sharpening runs.
    Fsr1,
    /// A temporal upscaler: jitter, motion vectors, history (TAAU, FSR 3.1, DLSS).
    Temporal(UpscalerKind),
    /// SMAA T2x's temporal half (Jimenez et al. 2012): two jitter positions, and after
    /// tone mapping a resolve with the previous frame along the motion vectors. Needs
    /// [`PostAntialiasing::Smaa`] (its spatial half) and the output size.
    SmaaT2x,
}

impl Upscale {
    /// Every mode (temporal upscalers in [`UpscalerKind`]'s order).
    pub const ALL: [Self; 10] = [
        Self::Nearest,
        Self::Bilinear,
        Self::CatmullRom,
        Self::Lanczos3,
        Self::Integer,
        Self::Fsr1,
        Self::Temporal(UpscalerKind::Taau),
        Self::Temporal(UpscalerKind::Fsr),
        Self::Temporal(UpscalerKind::Dlss),
        Self::SmaaT2x,
    ];

    /// Whether the mode keeps a history across frames (jitter, motion vectors).
    pub fn is_temporal(self) -> bool {
        matches!(self, Self::Temporal(_) | Self::SmaaT2x)
    }

    /// The temporal upscaler, if this is one.
    pub fn upscaler(self) -> Option<UpscalerKind> {
        match self {
            Self::Temporal(kind) => Some(kind),
            _ => None,
        }
    }

    /// A short name for menus and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Bilinear => "bilinear",
            Self::CatmullRom => "Catmull-Rom",
            Self::Lanczos3 => "Lanczos-3",
            Self::Integer => "integer",
            Self::Fsr1 => "FSR 1",
            Self::Temporal(kind) => kind.name(),
            Self::SmaaT2x => "SMAA T2x",
        }
    }
}

/// SMAA's quality presets (the reference code's): edge threshold, search distance,
/// diagonal and corner handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SmaaQuality {
    /// Threshold 0.15, 4 search steps, no diagonals or corners.
    Low,
    /// Threshold 0.1, 8 search steps, no diagonals or corners.
    Medium,
    /// Threshold 0.1, 16 search steps, diagonals (8 steps) and corners.
    #[default]
    High,
    /// Threshold 0.05, 32 search steps, diagonals (16 steps) and corners.
    Ultra,
}

/// Antialiasing of the tone-mapped image, before UI: filters that find edges in the
/// final colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PostAntialiasing {
    /// None.
    #[default]
    Off,
    /// FXAA 3.11's quality algorithm (Lottes 2011): one pass, luma edges, a search along
    /// each edge, subpixel smoothing. The cheapest, the softest.
    Fxaa,
    /// Jimenez et al.'s practical MLAA (GPU Pro 2, 2011), Reshetov's morphological
    /// antialiasing on the GPU: edges, line search, area table blending; no diagonals,
    /// corners or local contrast adaptation (SMAA's reference code with those off).
    Mlaa,
    /// SMAA 1x (Jimenez et al. 2012). With [`Msaa::X2`] it becomes SMAA S2x (each sample
    /// antialiased on its own, then averaged); with [`Upscale::SmaaT2x`] SMAA T2x, both
    /// SMAA 4x.
    Smaa(SmaaQuality),
}

impl PostAntialiasing {
    /// SMAA at its default (High) preset.
    pub const SMAA: Self = Self::Smaa(SmaaQuality::High);

    /// A short name for menus and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Fxaa => "FXAA",
            Self::Mlaa => "MLAA",
            Self::Smaa(SmaaQuality::Low) => "SMAA low",
            Self::Smaa(SmaaQuality::Medium) => "SMAA medium",
            Self::Smaa(SmaaQuality::High) => "SMAA high",
            Self::Smaa(SmaaQuality::Ultra) => "SMAA ultra",
        }
    }
}

/// How a view is antialiased: raster samples, how the render size reaches the output
/// (spatially or temporally), and a filter after tone mapping. Any combination
/// [`crate::FrameRenderer::check`] accepts runs; a mode that is not chosen costs nothing
/// (no passes, no pipelines, no shader registers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Antialiasing {
    /// Raster samples per pixel (MSAA).
    pub samples: Msaa,
    /// How the render size reaches the output.
    pub upscale: Upscale,
    /// The filter after tone mapping.
    pub post: PostAntialiasing,
}

impl Default for Antialiasing {
    fn default() -> Self {
        Self::TAAU
    }
}

impl Antialiasing {
    /// None: one sample, no history, no filter. A render size below the output is
    /// stretched bilinearly by the tone mapper.
    pub const OFF: Self = Self {
        samples: Msaa::X1,
        upscale: Upscale::Bilinear,
        post: PostAntialiasing::Off,
    };
    /// The engine's TAAU (at [`crate::Quality::Native`]: plain TAA).
    pub const TAAU: Self = Self::temporal(UpscalerKind::Taau);
    /// FSR 3.1.
    pub const FSR: Self = Self::temporal(UpscalerKind::Fsr);
    /// DLSS.
    pub const DLSS: Self = Self::temporal(UpscalerKind::Dlss);
    /// FXAA.
    pub const FXAA: Self = Self::post(PostAntialiasing::Fxaa);
    /// MLAA.
    pub const MLAA: Self = Self::post(PostAntialiasing::Mlaa);
    /// SMAA 1x (High).
    pub const SMAA_1X: Self = Self::post(PostAntialiasing::SMAA);
    /// SMAA T2x (High): SMAA 1x plus a two-frame temporal resolve.
    pub const SMAA_T2X: Self = Self {
        samples: Msaa::X1,
        upscale: Upscale::SmaaT2x,
        post: PostAntialiasing::SMAA,
    };
    /// SMAA S2x (High): MSAA 2× with each sample antialiased by SMAA.
    pub const SMAA_S2X: Self = Self {
        samples: Msaa::X2,
        upscale: Upscale::Bilinear,
        post: PostAntialiasing::SMAA,
    };
    /// SMAA 4x (High): S2x and T2x together.
    pub const SMAA_4X: Self = Self {
        samples: Msaa::X2,
        upscale: Upscale::SmaaT2x,
        post: PostAntialiasing::SMAA,
    };

    /// A temporal upscaler, one sample, no post filter.
    pub const fn temporal(kind: UpscalerKind) -> Self {
        Self {
            samples: Msaa::X1,
            upscale: Upscale::Temporal(kind),
            post: PostAntialiasing::Off,
        }
    }

    /// A post filter alone.
    pub const fn post(post: PostAntialiasing) -> Self {
        Self {
            samples: Msaa::X1,
            upscale: Upscale::Bilinear,
            post,
        }
    }

    /// MSAA with `samples`, nothing else.
    pub const fn msaa(samples: Msaa) -> Self {
        Self {
            samples,
            upscale: Upscale::Bilinear,
            post: PostAntialiasing::Off,
        }
    }

    /// A spatial scaler alone (see [`Upscale`]).
    pub const fn spatial(upscale: Upscale) -> Self {
        Self {
            samples: Msaa::X1,
            upscale,
            post: PostAntialiasing::Off,
        }
    }

    /// This with `samples`.
    pub const fn with_samples(self, samples: Msaa) -> Self {
        Self { samples, ..self }
    }

    /// This with `post`.
    pub const fn with_post(self, post: PostAntialiasing) -> Self {
        Self { post, ..self }
    }

    /// The temporal upscaler, if any.
    pub fn upscaler(self) -> Option<UpscalerKind> {
        self.upscale.upscaler()
    }

    /// Whether the view keeps a history (jitter, motion vectors).
    pub fn is_temporal(self) -> bool {
        self.upscale.is_temporal()
    }

    /// Whether SMAA runs on each of two samples (S2x): two samples and SMAA, unless step 4
    /// resamples the image (FSR 1, a temporal upscaler other than SMAA T2x), which needs
    /// one image: then SMAA 1x runs on the resolved one.
    pub fn smaa_s2x(self) -> bool {
        self.samples == Msaa::X2
            && matches!(self.post, PostAntialiasing::Smaa(_))
            && !matches!(self.upscale, Upscale::Fsr1 | Upscale::Temporal(_))
    }

    /// The checks that do not depend on the GPU (see [`InvalidAntialiasing`]):
    /// `native` says whether the view renders at the output size.
    pub fn validate(self, native: bool) -> Result<(), InvalidAntialiasing> {
        match self.upscale {
            Upscale::SmaaT2x if !matches!(self.post, PostAntialiasing::Smaa(_)) => {
                return Err(InvalidAntialiasing::SmaaT2xWithoutSmaa);
            }
            Upscale::SmaaT2x if !native => return Err(InvalidAntialiasing::SmaaT2xScaled),
            Upscale::Temporal(kind @ (UpscalerKind::Fsr | UpscalerKind::Dlss))
                if self.samples != Msaa::X1 =>
            {
                return Err(InvalidAntialiasing::MsaaIntoUpscaler(kind));
            }
            _ => {}
        }
        Ok(())
    }
}

impl fmt::Display for Antialiasing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::OFF {
            return f.write_str("off");
        }
        let mut parts = 0;
        let mut part = |f: &mut fmt::Formatter<'_>, s: &dyn fmt::Display| {
            parts += 1;
            if parts > 1 {
                f.write_str(" + ")?;
            }
            write!(f, "{s}")
        };
        if self.samples != Msaa::X1 {
            part(f, &format_args!("MSAA {}×", self.samples.count()))?;
        }
        if self.upscale != Upscale::Bilinear {
            part(f, &self.upscale.name())?;
        }
        if self.post != PostAntialiasing::Off {
            part(f, &self.post.name())?;
        }
        Ok(())
    }
}

/// Why a combination cannot run (from [`Antialiasing::validate`] and
/// [`crate::FrameRenderer::check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidAntialiasing {
    /// The GPU cannot render the scene's targets with this many samples (its limits and
    /// formats: [`AntialiasingSupport::msaa`]).
    SamplesUnsupported(Msaa),
    /// SMAA T2x is SMAA 1x plus a temporal resolve: it needs
    /// [`PostAntialiasing::Smaa`].
    SmaaT2xWithoutSmaa,
    /// SMAA T2x does not upscale: render at the output size (`Quality::Native`, no
    /// `render_scale`).
    SmaaT2xScaled,
    /// FSR 3.1 and DLSS reconstruct from aliased, jittered samples (both vendors' guides
    /// ask for MSAA off): MSAA's resolve would hand them pre-filtered pixels.
    MsaaIntoUpscaler(UpscalerKind),
}

impl fmt::Display for InvalidAntialiasing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SamplesUnsupported(m) => write!(
                f,
                "MSAA {}× is not supported by this GPU for the scene's targets",
                m.count()
            ),
            Self::SmaaT2xWithoutSmaa => {
                f.write_str("SMAA T2x needs SMAA after tone mapping (post: PostAntialiasing::Smaa)")
            }
            Self::SmaaT2xScaled => {
                f.write_str("SMAA T2x does not upscale: render at the output size")
            }
            Self::MsaaIntoUpscaler(kind) => write!(
                f,
                "{} expects aliased, jittered input: turn MSAA off with it",
                kind.name()
            ),
        }
    }
}

impl std::error::Error for InvalidAntialiasing {}

/// What this GPU offers ([`crate::FrameRenderer::antialiasing_support`]): a game
/// intersects it with what it wants to offer its players.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AntialiasingSupport {
    /// Sample counts the scene's targets can have (always [`Msaa::X1`]).
    pub msaa: [bool; 4],
    /// Temporal upscalers that run here (TAAU always; FSR when built in; DLSS with
    /// Streamline), by [`UpscalerKind`]: TAAU, FSR, DLSS.
    pub upscalers: [bool; 3],
}

impl AntialiasingSupport {
    /// Whether `samples` is supported.
    pub fn supports_samples(&self, samples: Msaa) -> bool {
        self.msaa[samples.flags().as_raw().trailing_zeros() as usize]
    }

    /// Whether the temporal upscaler `kind` runs here.
    pub fn supports_upscaler(&self, kind: UpscalerKind) -> bool {
        self.upscalers[match kind {
            UpscalerKind::Taau => 0,
            UpscalerKind::Fsr => 1,
            UpscalerKind::Dlss => 2,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_combinations_are_refused() {
        assert_eq!(Antialiasing::SMAA_T2X.validate(true), Ok(()));
        assert_eq!(Antialiasing::SMAA_4X.validate(true), Ok(()));
        assert_eq!(
            Antialiasing::SMAA_T2X.validate(false),
            Err(InvalidAntialiasing::SmaaT2xScaled)
        );
        assert_eq!(
            Antialiasing {
                post: PostAntialiasing::Fxaa,
                ..Antialiasing::SMAA_T2X
            }
            .validate(true),
            Err(InvalidAntialiasing::SmaaT2xWithoutSmaa)
        );
        for kind in [UpscalerKind::Fsr, UpscalerKind::Dlss] {
            assert_eq!(
                Antialiasing::temporal(kind)
                    .with_samples(Msaa::X4)
                    .validate(true),
                Err(InvalidAntialiasing::MsaaIntoUpscaler(kind))
            );
        }
        // TAAU over MSAA, MSAA with a post filter, spatial scaling with MSAA: fine.
        for valid in [
            Antialiasing::TAAU.with_samples(Msaa::X4),
            Antialiasing::msaa(Msaa::X4).with_post(PostAntialiasing::SMAA),
            Antialiasing::spatial(Upscale::Fsr1).with_samples(Msaa::X8),
            Antialiasing::spatial(Upscale::Integer).with_post(PostAntialiasing::Fxaa),
        ] {
            assert_eq!(valid.validate(false), Ok(()), "{valid}");
        }
        assert!(
            InvalidAntialiasing::MsaaIntoUpscaler(UpscalerKind::Dlss)
                .to_string()
                .contains("DLSS")
        );
    }

    #[test]
    fn names_compose() {
        assert_eq!(Antialiasing::OFF.to_string(), "off");
        assert_eq!(Antialiasing::TAAU.to_string(), "TAAU");
        assert_eq!(
            Antialiasing::SMAA_4X.to_string(),
            "MSAA 2× + SMAA T2x + SMAA high"
        );
        assert_eq!(
            Antialiasing::msaa(Msaa::X4)
                .with_post(PostAntialiasing::Fxaa)
                .to_string(),
            "MSAA 4× + FXAA"
        );
    }

    #[test]
    fn sample_counts_round_trip() {
        for m in Msaa::ALL {
            assert_eq!(Msaa::from_count(m.count()), Some(m));
            assert_eq!(m.flags().as_raw(), m.count());
        }
        assert_eq!(Msaa::from_count(3), None);
        let support = AntialiasingSupport {
            msaa: [true, true, true, false],
            upscalers: [true, false, false],
        };
        assert!(support.supports_samples(Msaa::X4));
        assert!(!support.supports_samples(Msaa::X8));
        assert!(!support.supports_upscaler(UpscalerKind::Fsr));
    }
}
