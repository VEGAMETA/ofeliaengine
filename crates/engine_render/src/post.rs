//! Post-processing passes. For now: tonemapping from an HDR target to the display format
//! (a [`ToneCurve`] with exposure), as one fullscreen-triangle pass in the graph, also used
//! to composite viewports into sub-rectangles of the output.
//!
//! HDR output (ADR 0038): [`OutputLevels`] describes how the swapchain reaches the display
//! (SDR, scRGB or HDR10, the luminance of reference white and of the peak); the tone mapper
//! rolls highlights off towards the display's peak and writes the swapchain's encoding.

use crate::graph::{
    Access, Buffer, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc,
};
use crate::postfx::BloomOutput;
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, Device, GraphicsPipelineDesc, OutputColor, Pipeline, RhiError, SamplerKind,
    ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/tonemap.vertexMain.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/tonemap.fragmentMain.spv");
const FRAGMENT_PERCEPTUAL: &[u8] = include_bytes!("../shaders/tonemap.fragmentPerceptual.spv");
const FRAGMENT_CATMULL_ROM: &[u8] = include_bytes!("../shaders/tonemap.fragmentCatmullRom.spv");
const FRAGMENT_LANCZOS3: &[u8] = include_bytes!("../shaders/tonemap.fragmentLanczos3.spv");
const FRAGMENT_PERCEPTUAL_CATMULL_ROM: &[u8] =
    include_bytes!("../shaders/tonemap.fragmentPerceptualCatmullRom.spv");
const FRAGMENT_PERCEPTUAL_LANCZOS3: &[u8] =
    include_bytes!("../shaders/tonemap.fragmentPerceptualLanczos3.spv");
const LANCZOS3_ACROSS: &[u8] = include_bytes!("../shaders/tonemap.lanczos3Across.spv");

/// The HDR format scenes render into before tonemapping.
pub const HDR_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// `Push` in `tonemap.slang` (40 bytes).
#[repr(C)]
struct Push {
    source: u32,
    sampler: u32,
    exposure: f32,
    flags: u32,
    peak: f32,
    white: f32,
    bloom: u32,
    bloom_intensity: f32,
    auto_exposure: u64,
}

const _: () = assert!(core::mem::size_of::<Push>() == 40);

const FLAG_PASS_THROUGH: u32 = 1;
const FLAG_ENCODE_PQ: u32 = 2;
const FLAG_BLOOM: u32 = 4;
const FLAG_AUTO_EXPOSURE: u32 = 8;
const FLAG_PBR_NEUTRAL: u32 = 16;
const FLAG_AGX: u32 = 32;
const FLAG_ACES: u32 = 64;

/// The tone curve: how scene values map to what the display shows (ADR 0048).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToneCurve {
    /// Uchimura's curve (Gran Turismo Sport, CEDEC 2017), per channel: a toe in the darks,
    /// a linear section that keeps mid-tones as they are (0.18 stays 0.18), and a shoulder
    /// that approaches the display's peak smoothly. Bright colors whiten as their channels
    /// reach the peak, as on film. The same mid-tones in SDR and HDR, with the peak where
    /// the display's is.
    #[default]
    Gt,
    /// Khronos PBR Neutral: hue-preserving, made for product visualization (the colors of
    /// materials under neutral light, not a filmic look).
    PbrNeutral,
    /// AgX (Troy Sobotka; Blender 4's default): a log encoding and a sigmoid in a gently
    /// inset gamut, so very bright saturated colors go to white along their hue instead of
    /// turning yellow or cyan per channel. Soft, low contrast. SDR only: on HDR outputs
    /// it maps to reference white and leaves the extra range unused.
    Agx,
    /// Stephen Hill's fit of ACES 1.0 (RRT + sRGB ODT), as in MJP's Baking Lab: the
    /// familiar film look of many games, contrasty, with saturated highlights that shift
    /// hue. Mid-grey comes out darker (0.18 → 0.11: about +0.7 EV of exposure matches the
    /// other curves). SDR only, like [`Self::Agx`].
    Aces,
}

/// `m · v` for a row-major 3 × 3 matrix.
fn mat3(m: &[[f32; 3]; 3], [r, g, b]: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * r + row[1] * g + row[2] * b)
}

/// AgX's inset matrix (Blender's, for Rec.2020 primaries) applied after BT.709 → BT.2020,
/// folded into one matrix; row-major, as `tonemap.slang`.
const AGX_IN: [[f32; 3]; 3] = [
    [0.544_814_8, 0.373_786_5, 0.081_398_2],
    [0.140_416_76, 0.754_137_1, 0.105_444_7],
    [0.088_810_54, 0.178_871_52, 0.732_317_6],
];
/// AgX's outset matrix (the inverse of Blender's `AgXOutsetMatrixInv`), in Rec.2020.
const AGX_OUT: [[f32; 3]; 3] = [
    [1.127_100_6, -0.110_606_64, -0.016_493_939],
    [-0.141_329_76, 1.157_823_7, -0.016_493_939],
    [-0.141_329_76, -0.110_606_64, 1.251_936_4],
];
/// BT.2020 → BT.709 primaries, both linear (the inverse of ITU-R BT.2087's matrix).
const BT709_FROM_BT2020: [[f32; 3]; 3] = [
    [1.660_490_3, -0.587_639_1, -0.072_851_6],
    [-0.124_549_97, 1.132_899_9, -0.008_347_964],
    [-0.018_151_12, -0.100_578_72, 1.118_73],
];
/// AgX's log range: log2(0.18 · 2^−10) .. log2(0.18 · 2^6.5).
const AGX_MIN_EV: f32 = -12.473_93;
const AGX_MAX_EV: f32 = 4.026_069;

/// AgX's default contrast sigmoid, a polynomial fit on the log-encoded value 0..1
/// (Wrensch's minimal AgX, Rec.2020 coefficients as in Filament).
fn agx_sigmoid(x: f32) -> f32 {
    let x2 = x * x;
    let x4 = x2 * x2;
    let x6 = x4 * x2;
    -17.86 * x6 * x + 78.01 * x6 - 126.7 * x4 * x + 92.06 * x4 - 28.72 * x2 * x + 4.361 * x2
        - 0.1718 * x
        + 0.002_857
}

/// AgX on a linear BT.709 color (as `tonemap.slang`).
fn agx(rgb: [f32; 3]) -> [f32; 3] {
    let v = mat3(&AGX_IN, rgb.map(|c| c.max(0.0)));
    let v = v.map(|c| {
        let t = (c.max(1e-10).log2() - AGX_MIN_EV) / (AGX_MAX_EV - AGX_MIN_EV);
        agx_sigmoid(t.clamp(0.0, 1.0))
    });
    let v = mat3(&AGX_OUT, v).map(|c| c.max(0.0).powf(2.2));
    // Colors AgX leaves outside BT.709 (it works in Rec.2020) are clipped to the display.
    mat3(&BT709_FROM_BT2020, v).map(|c| c.clamp(0.0, 1.0))
}

/// Linear sRGB → XYZ → D65 to D60 → AP1 → RRT saturation (Hill's fit of ACES 1.0).
const ACES_IN: [[f32; 3]; 3] = [
    [0.597_19, 0.354_58, 0.048_23],
    [0.076_00, 0.908_34, 0.015_66],
    [0.028_40, 0.133_83, 0.837_77],
];
/// ODT saturation → XYZ → D60 to D65 → linear sRGB.
const ACES_OUT: [[f32; 3]; 3] = [
    [1.604_75, -0.531_08, -0.073_67],
    [-0.102_08, 1.108_13, -0.006_05],
    [-0.003_27, -0.072_76, 1.076_02],
];

/// Hill's ACES fit on a linear BT.709 color (as `tonemap.slang`).
fn aces(rgb: [f32; 3]) -> [f32; 3] {
    let v = mat3(&ACES_IN, rgb.map(|c| c.max(0.0)));
    let v = v.map(|c| {
        let a = c * (c + 0.024_578_6) - 0.000_090_537;
        let b = c * (0.983_729 * c + 0.432_951) + 0.238_081;
        a / b
    });
    mat3(&ACES_OUT, v).map(|c| c.clamp(0.0, 1.0))
}

/// Uchimura's curve with the published defaults (contrast 1, linear section from 0.22 over
/// 0.4 of the range, toe power 1.33, no pedestal) and a peak of `peak`.
fn gt(x: f32, peak: f32) -> f32 {
    let (a, m, l, c, b) = (1.0f32, 0.22f32, 0.4f32, 1.33f32, 0.0f32);
    let p = peak.max(m + 1e-3);
    let l0 = (p - m) * l / a;
    let s0 = m + l0;
    let s1 = m + a * l0;
    let c2 = a * p / (p - s1);
    let cp = -c2 / p;
    let w0 = if x < m {
        let t = x / m;
        1.0 - t * t * (3.0 - 2.0 * t)
    } else {
        0.0
    };
    let w2 = if x < s0 { 0.0 } else { 1.0 };
    let w1 = 1.0 - w0 - w2;
    let toe = m * (x / m).max(0.0).powf(c) + b;
    let shoulder = p - (p - s1) * (cp * (x - s0)).exp();
    let linear = m + a * (x - m);
    toe * w0 + linear * w1 + shoulder * w2
}

/// PBR Neutral rolled off towards `peak` (as `tonemap.slang`).
fn pbr_neutral([mut r, mut g, mut b]: [f32; 3], peak: f32) -> [f32; 3] {
    let start_compression = 0.8 - 0.04;
    let desaturation = 0.15;
    let x = r.min(g).min(b);
    let offset = if x < 0.08 { x - 6.25 * x * x } else { 0.04 };
    (r, g, b) = (r - offset, g - offset, b - offset);
    let brightest = r.max(g).max(b);
    let start = start_compression * peak;
    if brightest < start {
        return [r, g, b];
    }
    let d = peak - start;
    let new_peak = peak - d * d / (brightest + d - start);
    let k = new_peak / brightest;
    (r, g, b) = (r * k, g * k, b * k);
    let t = 1.0 - 1.0 / (desaturation * (brightest - new_peak) / peak + 1.0);
    let mix = |c: f32| c + (new_peak - c) * t;
    [mix(r), mix(g), mix(b)]
}

impl ToneCurve {
    /// The curve on a linear BT.709 color in units of reference white, rolling off
    /// towards `peak` (1 for SDR): what the tone map pass computes, for tools and tests.
    pub fn apply(self, rgb: [f32; 3], peak: f32) -> [f32; 3] {
        match self {
            Self::Gt => rgb.map(|c| gt(c.max(0.0), peak)),
            Self::PbrNeutral => pbr_neutral(rgb, peak),
            Self::Agx => agx(rgb),
            Self::Aces => aces(rgb),
        }
    }
}

/// Luminance of 1.0 in scRGB, in nits.
const SCRGB_WHITE_NITS: f32 = 80.0;
/// Luminance of the PQ signal's maximum, in nits.
const PQ_MAX_NITS: f32 = 10_000.0;

/// How output values relate to light (ADR 0038).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputEncoding {
    /// Linear, reference white written as `white`: 1 for SDR targets and intermediate
    /// textures, SDR white in scRGB units (nits / 80) for an scRGB swapchain.
    Linear {
        /// The value of reference white.
        white: f32,
    },
    /// HDR10: BT.2020 primaries, PQ-encoded, reference white at `white_nits`.
    Pq {
        /// Luminance of reference white in nits.
        white_nits: f32,
    },
}

impl OutputEncoding {
    /// SDR targets and intermediate textures: reference white is 1.
    pub const LINEAR: Self = Self::Linear { white: 1.0 };

    /// Encodes a linear BT.709 color given in units of reference white, as the tone map
    /// pass would write it (for clear colors). Alpha is kept.
    pub fn encode(self, [r, g, b, a]: [f32; 4]) -> [f32; 4] {
        match self {
            Self::Linear { white } => [r * white, g * white, b * white, a],
            Self::Pq { white_nits } => {
                let scale = white_nits / PQ_MAX_NITS;
                let [r, g, b] = bt2020_from_bt709([r, g, b]);
                [
                    pq_encode(r * scale),
                    pq_encode(g * scale),
                    pq_encode(b * scale),
                    a,
                ]
            }
        }
    }

    fn push(self) -> (f32, u32) {
        match self {
            Self::Linear { white } => (white, 0),
            Self::Pq { white_nits } => (white_nits / PQ_MAX_NITS, FLAG_ENCODE_PQ),
        }
    }
}

/// How rendering reaches the display (ADR 0038): the swapchain's output, and the
/// luminance of reference white (SDR white: UI white, a diffuse white surface at exposure
/// 1) and of the brightest output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputLevels {
    /// What the swapchain holds.
    pub color: OutputColor,
    /// Luminance of reference white in nits (in HDR: Windows' SDR white level, or a
    /// game's brightness setting).
    pub white_nits: f32,
    /// Luminance of the brightest output in nits (the display's peak).
    pub peak_nits: f32,
}

impl OutputLevels {
    /// SDR output: reference white is the display's white, nothing is brighter.
    pub const SDR: Self = Self {
        color: OutputColor::Sdr,
        white_nits: SCRGB_WHITE_NITS,
        peak_nits: SCRGB_WHITE_NITS,
    };

    /// Where the tone mapper's roll-off ends, in units of reference white: 1 for SDR,
    /// the display's peak over reference white (at least 1) for HDR.
    pub fn peak(&self) -> f32 {
        if self.color.is_hdr() && self.white_nits > 0.0 {
            (self.peak_nits / self.white_nits).max(1.0)
        } else {
            1.0
        }
    }

    /// The encoding of passes that write the swapchain itself.
    pub fn swapchain_encoding(&self) -> OutputEncoding {
        match self.color {
            OutputColor::Sdr => OutputEncoding::LINEAR,
            OutputColor::ScRgb => OutputEncoding::Linear {
                white: self.white_nits / SCRGB_WHITE_NITS,
            },
            OutputColor::Hdr10 => OutputEncoding::Pq {
                white_nits: self.white_nits,
            },
        }
    }
}

impl Default for OutputLevels {
    fn default() -> Self {
        Self::SDR
    }
}

/// BT.709 → BT.2020 primaries, both linear (ITU-R BT.2087).
fn bt2020_from_bt709([r, g, b]: [f32; 3]) -> [f32; 3] {
    [
        0.627_404 * r + 0.329_282 * g + 0.043_313_6 * b,
        0.069_097 * r + 0.919_54 * g + 0.011_361_2 * b,
        0.016_391_6 * r + 0.088_013_2 * g + 0.895_595 * b,
    ]
}

/// SMPTE ST 2084 (PQ) inverse EOTF: luminance / 10000 nits → signal (as `tonemap.slang`).
fn pq_encode(y: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let p = y.clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

/// How [`Tonemap::add_composite_pass`] writes a source into (part of) an output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Composite {
    /// Target rectangle in output pixels, or `None` for the whole output.
    pub rect: Option<vk::Rect2D>,
    /// Sampler: linear for smooth scaling, nearest for pixel-exact integer scaling.
    pub sampler: SamplerKind,
    /// Linear multiplier before tone mapping.
    pub exposure: f32,
    /// Apply the tone mapper (HDR sources); `false` copies (LDR sources).
    pub tonemap: bool,
    /// Which tone curve.
    pub curve: ToneCurve,
    /// Where the tone mapper's roll-off ends, in units of reference white
    /// ([`OutputLevels::peak`]; 1 for SDR).
    pub peak: f32,
    /// How the output stores values.
    pub encoding: OutputEncoding,
    /// What happens to the output first. `Load` keeps earlier composites. A clear color
    /// is written as it is: encode it first ([`OutputEncoding::encode`]).
    pub load: LoadOp,
    /// Bloom of the source ([`crate::postfx::Bloom`]), mixed in before exposure.
    pub bloom: Option<BloomOutput>,
    /// An exposure measured on the GPU ([`crate::postfx::AutoExposure`]), multiplied
    /// with `exposure` (which then acts as compensation).
    pub auto_exposure: Option<Buffer>,
    /// A rectangle cleared to black before the source is drawn into `rect` (integer
    /// scaling's letterbox: the view's whole rectangle around the scaled image).
    pub letterbox: Option<vk::Rect2D>,
}

impl Default for Composite {
    fn default() -> Self {
        Self {
            rect: None,
            sampler: SamplerKind::LinearClamp,
            exposure: 1.0,
            tonemap: true,
            curve: ToneCurve::Gt,
            peak: 1.0,
            encoding: OutputEncoding::LINEAR,
            load: LoadOp::DontCare,
            bloom: None,
            auto_exposure: None,
            letterbox: None,
        }
    }
}

/// How the tone mapper reads its source (ADR 0075): one tap through
/// [`Composite::sampler`], or a wider kernel that resamples a smaller source to the
/// output's size. The kernels read each texel on their own and limit the result to the
/// range of the four texels around the sample (no ringing, however bright the source).
/// Each is a pipeline of its own: a tone mapper that does not resample has none of their
/// code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Resample {
    /// One tap through the composite's sampler: nearest or bilinear.
    #[default]
    Sampler,
    /// Catmull-Rom (Keys's cubic convolution, a = −0.5): 4 × 4 texels. Sharper than
    /// bilinear; interpolates (texel centers keep their values).
    CatmullRom,
    /// Lanczos-3 (sinc windowed by sinc, 3 lobes): 6 × 6 texels, in two passes (it is
    /// separable: the rows into an image of the output's width, then the columns in the
    /// tone mapper; 12 reads for each output pixel instead of 36). The sharpest.
    Lanczos3,
}

/// Tonemapping into one output format.
#[derive(Debug)]
pub struct Tonemap {
    pipeline: Pipeline,
    /// A separable kernel's first pass (Lanczos-3's rows, into [`HDR_FORMAT`]).
    across: Option<Pipeline>,
}

impl Tonemap {
    /// Creates the pipeline for writing `output_format` (for example the swapchain's).
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        output_format: vk::Format,
    ) -> Result<Self, RhiError> {
        Self::with_fragment(device, bindless, output_format, FRAGMENT, c"fragmentMain")
    }

    /// A tone mapper for the antialiasing filters after it (ADR 0075,
    /// [`crate::postaa`]): it writes [`crate::postaa::PERCEPTUAL_FORMAT`] in a perceptual
    /// encoding (sRGB over the output's brightest value, or PQ as it is), which they
    /// decode into the output's.
    pub fn new_perceptual(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        Self::with_fragment(
            device,
            bindless,
            crate::postaa::PERCEPTUAL_FORMAT,
            FRAGMENT_PERCEPTUAL,
            c"fragmentPerceptual",
        )
    }

    /// A tone mapper that reads its source through `resample` ([`Resample`]), writing
    /// `output_format`, or with `output_format` `None` the perceptual image of
    /// [`Self::new_perceptual`].
    pub fn with_resample(
        device: &Device,
        bindless: &Bindless,
        output_format: Option<vk::Format>,
        resample: Resample,
    ) -> Result<Self, RhiError> {
        let (fragment, entry) = match (resample, output_format.is_some()) {
            (Resample::Sampler, true) => (FRAGMENT, c"fragmentMain"),
            (Resample::Sampler, false) => (FRAGMENT_PERCEPTUAL, c"fragmentPerceptual"),
            (Resample::CatmullRom, true) => (FRAGMENT_CATMULL_ROM, c"fragmentCatmullRom"),
            (Resample::CatmullRom, false) => (
                FRAGMENT_PERCEPTUAL_CATMULL_ROM,
                c"fragmentPerceptualCatmullRom",
            ),
            (Resample::Lanczos3, true) => (FRAGMENT_LANCZOS3, c"fragmentLanczos3"),
            (Resample::Lanczos3, false) => {
                (FRAGMENT_PERCEPTUAL_LANCZOS3, c"fragmentPerceptualLanczos3")
            }
        };
        let format = output_format.unwrap_or(crate::postaa::PERCEPTUAL_FORMAT);
        let mut tonemap = Self::with_fragment(device, bindless, format, fragment, entry)?;
        if resample == Resample::Lanczos3 {
            let across = Self::with_fragment(
                device,
                bindless,
                HDR_FORMAT,
                LANCZOS3_ACROSS,
                c"lanczos3Across",
            )?;
            tonemap.across = Some(across.pipeline);
        }
        Ok(tonemap)
    }

    fn with_fragment(
        device: &Device,
        bindless: &Bindless,
        output_format: vk::Format,
        fragment: &[u8],
        entry: &core::ffi::CStr,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let fragment = spirv_words(fragment).ok_or_else(invalid)?;
        let formats = [output_format];
        let pipeline = Pipeline::graphics(
            device,
            bindless.pipeline_layout(),
            &GraphicsPipelineDesc {
                color_formats: &formats,
                cull_mode: vk::CullModeFlags::NONE,
                depth_test: false,
                depth_write: false,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"vertexMain",
                    },
                    Some(ShaderStageDesc {
                        spirv: &fragment,
                        entry,
                    }),
                )
            },
        )?;
        Ok(Self {
            pipeline,
            across: None,
        })
    }

    /// Declares a pass that tonemaps `hdr` (scaled by `exposure`) into `output`, an SDR
    /// target ([`Tonemap::add_composite_pass`] writes HDR). The frame must execute with
    /// `bindless` so that `hdr` gets an index.
    pub fn add_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        hdr: Texture,
        output: Texture,
        exposure: f32,
    ) {
        self.declare(
            fg,
            bindless,
            "tonemap",
            hdr,
            output,
            Composite {
                exposure,
                ..Composite::default()
            },
        );
    }

    /// Declares a pass that writes `source` into `composite.rect` of `output`, tonemapped
    /// or copied, scaled with the chosen sampler (or the tone mapper's [`Resample`]
    /// kernel; bloom still goes through the sampler). Several composites into one output
    /// build split screens and picture-in-picture (the first clears, the others load).
    /// Without a rectangle it is a whole-target pass, named by what it does ("tonemap",
    /// "encode hdr10" or "copy") in captures and GPU timings.
    pub fn add_composite_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        source: Texture,
        output: Texture,
        composite: Composite,
    ) {
        let name = match (composite.rect, composite.tonemap, composite.encoding) {
            (Some(_), ..) => "composite",
            (None, true, _) => "tonemap",
            (None, false, OutputEncoding::Pq { .. }) => "encode hdr10",
            (None, false, OutputEncoding::Linear { .. }) => "copy",
        };
        // A separable kernel's rows first, at the output's width.
        let source = match &self.across {
            Some(across) => {
                let width = composite
                    .rect
                    .map_or_else(|| fg.texture_desc(output).width, |r| r.extent.width);
                let height = fg.texture_desc(source).height;
                let rows = fg.create_texture(
                    "resampled rows",
                    TextureDesc::new_2d(HDR_FORMAT, width, height),
                );
                Self::declare_across(fg, bindless, across, source, rows);
                rows
            }
            None => source,
        };
        self.declare(fg, bindless, name, source, output, composite);
    }

    /// The pass of [`Self::across`]: `source`'s rows resampled into `rows` (the output's
    /// width, the source's height).
    fn declare_across<'g>(
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        pipeline: &'g Pipeline,
        source: Texture,
        rows: Texture,
    ) {
        let size = fg.texture_desc(rows);
        fg.add_pass("resample rows")
            .read(source, Access::Sampled(ShaderStages::FRAGMENT))
            .color(rows, LoadOp::DontCare, StoreOp::Store)
            .execute(move |ctx| {
                let Some(index) = ctx.sampled_index(source) else {
                    return;
                };
                let push = Push {
                    source: index,
                    sampler: Bindless::sampler(SamplerKind::LinearClamp),
                    exposure: 1.0,
                    flags: FLAG_PASS_THROUGH,
                    peak: 1.0,
                    white: 1.0,
                    bloom: 0,
                    bloom_intensity: 0.0,
                    auto_exposure: 0,
                };
                // SAFETY: `Push` is plain `repr(C)` data without padding.
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&raw const push).cast::<u8>(),
                        core::mem::size_of::<Push>(),
                    )
                };
                let rect = vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: size.width,
                        height: size.height,
                    },
                };
                // SAFETY: recording inside the pass's rendering scope.
                unsafe {
                    pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    bindless.push_constants(ctx.cmd, bytes);
                    ctx.device.cmd_set_viewport(
                        ctx.cmd,
                        0,
                        &[vk::Viewport {
                            x: 0.0,
                            y: 0.0,
                            width: size.width as f32,
                            height: size.height as f32,
                            min_depth: 0.0,
                            max_depth: 1.0,
                        }],
                    );
                    ctx.device.cmd_set_scissor(ctx.cmd, 0, &[rect]);
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                }
            });
    }

    fn declare<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        name: &'static str,
        source: Texture,
        output: Texture,
        composite: Composite,
    ) {
        let pipeline = &self.pipeline;
        let mut pass = fg
            .add_pass(name)
            .read(source, Access::Sampled(ShaderStages::FRAGMENT));
        if let Some(bloom) = composite.bloom {
            pass = pass.read(bloom.texture, Access::Sampled(ShaderStages::FRAGMENT));
        }
        if let Some(exposure) = composite.auto_exposure {
            pass = pass.read(exposure, Access::StorageRead(ShaderStages::FRAGMENT));
        }
        pass.color(output, composite.load, StoreOp::Store)
            .execute(move |ctx| {
                let Some(index) = ctx.sampled_index(source) else {
                    return;
                };
                let (white, encode) = composite.encoding.push();
                let bloom = composite
                    .bloom
                    .and_then(|b| ctx.sampled_index(b.texture).map(|i| (i, b.intensity)));
                let auto_exposure = composite.auto_exposure.map(|b| ctx.address(b));
                let push = Push {
                    source: index,
                    sampler: Bindless::sampler(composite.sampler),
                    exposure: composite.exposure,
                    flags: if composite.tonemap {
                        0
                    } else {
                        FLAG_PASS_THROUGH
                    } | encode
                        | if bloom.is_some() { FLAG_BLOOM } else { 0 }
                        | match composite.curve {
                            ToneCurve::Gt => 0,
                            ToneCurve::PbrNeutral => FLAG_PBR_NEUTRAL,
                            ToneCurve::Agx => FLAG_AGX,
                            ToneCurve::Aces => FLAG_ACES,
                        }
                        | if auto_exposure.is_some() {
                            FLAG_AUTO_EXPOSURE
                        } else {
                            0
                        },
                    peak: composite.peak,
                    white,
                    bloom: bloom.map_or(0, |b| b.0),
                    bloom_intensity: bloom.map_or(0.0, |b| b.1),
                    auto_exposure: auto_exposure.unwrap_or(0),
                };
                // SAFETY: `Push` is plain `repr(C)` data without padding.
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&raw const push).cast::<u8>(),
                        core::mem::size_of::<Push>(),
                    )
                };
                let area = ctx.render_area();
                let rect = composite.rect.unwrap_or(area);
                if let Some(letterbox) = composite.letterbox {
                    // SAFETY: recording inside the pass's rendering scope; the rectangle
                    // lies within the render area (the target's).
                    unsafe {
                        ctx.device.cmd_clear_attachments(
                            ctx.cmd,
                            &[vk::ClearAttachment {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                color_attachment: 0,
                                clear_value: vk::ClearValue {
                                    color: vk::ClearColorValue {
                                        float32: [0.0, 0.0, 0.0, 1.0],
                                    },
                                },
                            }],
                            &[vk::ClearRect {
                                rect: letterbox,
                                base_array_layer: 0,
                                layer_count: 1,
                            }],
                        );
                    }
                }
                // SAFETY: recording inside the pass's rendering scope.
                unsafe {
                    pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    bindless.push_constants(ctx.cmd, bytes);
                    ctx.device.cmd_set_viewport(
                        ctx.cmd,
                        0,
                        &[vk::Viewport {
                            x: rect.offset.x as f32,
                            y: rect.offset.y as f32,
                            width: rect.extent.width as f32,
                            height: rect.extent.height as f32,
                            min_depth: 0.0,
                            max_depth: 1.0,
                        }],
                    );
                    ctx.device.cmd_set_scissor(ctx.cmd, 0, &[rect]);
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gt_keeps_mid_tones_and_rolls_off_smoothly_to_the_peak() {
        for peak in [1.0f32, 2.5, 5.0, 12.5] {
            // Mid-tones are kept, the same for every peak.
            assert!((gt(0.5, peak) - 0.5).abs() < 1e-6, "{peak}");
            // Monotonic, below the peak, reaching it far above.
            let mut last = 0.0;
            for k in 0..20_000 {
                let x = k as f32 * 0.005;
                let y = gt(x, peak);
                assert!(y >= last - 1e-6 && y <= peak + 1e-6, "{peak} {x} {y}");
                last = y;
            }
            assert!(gt(1000.0, peak) > peak * 0.999);
            // Continuous in value and slope where the shoulder starts.
            let s0 = 0.22 + (peak - 0.22) * 0.4;
            let e = 1e-3;
            assert!((gt(s0 - e, peak) - gt(s0 + e, peak)).abs() < 3e-3);
            let slope = (gt(s0 + 2.0 * e, peak) - gt(s0 + e, peak)) / e;
            assert!((slope - 1.0).abs() < 0.01, "{peak}: {slope}");
        }
        // The toe darkens the darks a little, black stays black.
        assert_eq!(gt(0.0, 1.0), 0.0);
        assert!(gt(0.05, 1.0) < 0.05);
        // Bright colors whiten: the ratio of channels shrinks towards the peak.
        let dim = ToneCurve::Gt.apply([0.4, 0.2, 0.1], 1.0);
        let bright = ToneCurve::Gt.apply([40.0, 20.0, 10.0], 1.0);
        assert!(bright[2] / bright[0] > dim[2] / dim[0]);
    }

    #[test]
    fn agx_and_aces_are_neutral_monotonic_and_bounded() {
        for curve in [ToneCurve::Agx, ToneCurve::Aces] {
            let mut last = -1.0;
            for k in 0..4_000 {
                let x = (k as f32 * 0.005 - 12.0).exp2();
                let [r, g, b] = curve.apply([x, x, x], 1.0);
                // Grey stays grey, rises with the input, stays within the display.
                assert!(
                    (r - g).abs() < 2e-4 && (g - b).abs() < 2e-4,
                    "{curve:?} {x}"
                );
                // AgX's sigmoid sums terms of about ±127 in f32: near white, where it is
                // almost flat, rounding (~3e-5) exceeds a step's rise.
                assert!(g >= last - 1e-4 && g <= 1.0 + 1e-6, "{curve:?} {x}: {g}");
                last = g;
            }
            assert!(last > 0.95, "{curve:?} reaches white: {last}");
            assert!(curve.apply([0.0; 3], 1.0)[1] < 1e-3, "{curve:?}: black");
        }
        // Mid-grey: AgX close to the others, ACES darker (documented on the variants).
        let agx_grey = ToneCurve::Agx.apply([0.18; 3], 1.0)[1];
        let aces_grey = ToneCurve::Aces.apply([0.18; 3], 1.0)[1];
        assert!((0.12..0.25).contains(&agx_grey), "{agx_grey}");
        assert!((0.09..0.13).contains(&aces_grey), "{aces_grey}");
        // AgX takes a very bright saturated color towards white along its hue: the
        // weakest channel catches up.
        let dim = ToneCurve::Agx.apply([0.3, 0.05, 0.02], 1.0);
        let bright = ToneCurve::Agx.apply([300.0, 50.0, 20.0], 1.0);
        assert!(bright[2] / bright[0] > dim[2] / dim[0] * 2.0);
    }

    #[test]
    fn pq_matches_known_levels() {
        // ST 2084 code values (10-bit full range) of common luminances.
        for (nits, code) in [
            (0.0, 0),
            (100.0, 520),
            (203.0, 594),
            (1000.0, 769),
            (10_000.0, 1023),
        ] {
            let signal = pq_encode(nits / PQ_MAX_NITS);
            assert_eq!((signal * 1023.0).round() as u32, code, "{nits} nits");
        }
    }

    #[test]
    fn levels_scale_reference_white() {
        let sdr = OutputLevels::SDR;
        assert_eq!(sdr.peak(), 1.0);
        assert_eq!(sdr.swapchain_encoding(), OutputEncoding::LINEAR);

        let scrgb = OutputLevels {
            color: OutputColor::ScRgb,
            white_nits: 200.0,
            peak_nits: 1000.0,
        };
        assert_eq!(scrgb.peak(), 5.0);
        assert_eq!(
            scrgb.swapchain_encoding(),
            OutputEncoding::Linear { white: 2.5 },
            "200 nits in scRGB (80 nits = 1)"
        );
        assert_eq!(
            scrgb.swapchain_encoding().encode([1.0, 0.5, 0.0, 1.0]),
            [2.5, 1.25, 0.0, 1.0]
        );
        // A display dimmer than the white level: no roll-off below reference white.
        let dim = OutputLevels {
            peak_nits: 150.0,
            ..scrgb
        };
        assert_eq!(dim.peak(), 1.0);

        let hdr10 = OutputLevels {
            color: OutputColor::Hdr10,
            ..scrgb
        };
        let [r, g, b, a] = hdr10.swapchain_encoding().encode([1.0, 1.0, 1.0, 0.5]);
        // White keeps equal channels through the primaries' conversion.
        let white = pq_encode(200.0 / PQ_MAX_NITS);
        for c in [r, g, b] {
            assert!((c - white).abs() < 1e-4, "{c} vs {white}");
        }
        assert_eq!(a, 0.5);
    }
}
