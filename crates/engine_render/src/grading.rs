//! Color grading (ADR 0094, docs/research/color-grading.md): the standard controls of a
//! colorist, applied to the scene-linear image before the tone curve. The grading and the
//! curve are one 3D LUT ([`GradingLut`]), built on the GPU when either changes; the tone
//! mapper's graded entry points sample it once per pixel in place of the curve
//! ([`crate::post::Composite::grading`]). PBR Neutral couples the channels, which a LUT
//! interpolates badly: with it the LUT holds the grading alone, in ACEScct, and the curve
//! follows. Without grading the tone mapper is what it was: its plain entry points have
//! none of this.
//!
//! The chain, in Filament's order (Apache-2.0; the operations are published standards),
//! in linear BT.709 unless stated:
//!
//! 1. white balance (a von Kries adaptation in CIECAT16 from a white point moved along
//!    and across the daylight locus) and the channel mixer, one 3 × 3 matrix;
//! 2. shadows, mid-tones and highlights: color multipliers weighted by smoothsteps of
//!    luminance over four tonal range points (DaVinci Resolve's defaults);
//! 3. in ACEScct (Academy S-2016-001): the ASC CDL (slope, offset, power) and contrast
//!    around mid-grey (Resolve's convention);
//! 4. vibrance (saturation scaled by how unsaturated a color is) and saturation, around
//!    luminance;
//! 5. negative values to 0, then the tone curve.
//!
//! [`ColorGrading::apply`] is the CPU description of steps 1–5 (without the curve), as
//! [`crate::post::ToneCurve::apply`] is of the curves.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::environment::{Image, ImageKind};
use crate::graph::{Access, FrameGraph, ShaderStages, Texture};
use crate::post::ToneCurve;
use crate::upload::UploadRing;
use core::cell::Cell;
use engine_rhi::ash::vk;
use engine_rhi::{Allocator, Bindless, Device, RhiError};

const GRADE_LUT: &[u8] = include_bytes!("../shaders/grading.gradeLut.spv");

/// Texels per side of the graded LUT (ADR 0094: measured against the curves).
pub const LUT_SIZE: u32 = 48;

/// The LUT's format: the tone-mapped color in units of reference white (the graded one in
/// ACEScct with PBR Neutral).
const LUT_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// BT.709 luminance weights.
const LUMINANCE: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// ACEScct's value of mid-grey (0.18).
pub const MID_GREY_ACESCCT: f32 = 0.413_588_4;

/// Linear (ACES) to ACEScct (Academy S-2016-001): a linear toe below 2⁻⁷, then
/// (log2(x) + 9.72) / 17.52. The LUT's coordinate covers 0..1 (linear up to 222.86).
pub fn acescct_encode(x: f32) -> f32 {
    if x <= 0.007_812_5 {
        10.540_238 * x + 0.072_905_53
    } else {
        (x.log2() + 9.72) / 17.52
    }
}

/// ACEScct back to linear.
pub fn acescct_decode(c: f32) -> f32 {
    if c <= 0.155_251_14 {
        (c - 0.072_905_53) / 10.540_238
    } else {
        (c * 17.52 - 9.72).exp2()
    }
}

/// The grading of a view (ADR 0094). The default changes nothing (`is_neutral`), and a
/// neutral grading costs nothing: the tone mapper then evaluates its curve as without it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorGrading {
    /// White balance: temperature (−1 cooler … 1 warmer: the white point assumed for the
    /// scene moves along the daylight locus, bluer for warmer) and tint (−1 green …
    /// 1 magenta: across it). 0, 0: none.
    pub white_balance: [f32; 2],
    /// Channel mixer: the output's red, green and blue as weights of the input's
    /// channels (rows). Identity: none.
    pub channel_mixer: [[f32; 3]; 3],
    /// Color multiplier of the shadows (1: none).
    pub shadows: [f32; 3],
    /// Color multiplier of the mid-tones (1: none).
    pub midtones: [f32; 3],
    /// Color multiplier of the highlights (1: none).
    pub highlights: [f32; 3],
    /// Where the shadows fade out (the first two) and the highlights in (the last two),
    /// in linear luminance: DaVinci Resolve's defaults 0, 0.333, 0.55, 1.
    pub tonal_ranges: [f32; 4],
    /// The ASC CDL in ACEScct, per channel: `(v · slope + offset)^power`. Its slope
    /// (1: none).
    pub slope: [f32; 3],
    /// Its offset (0: none).
    pub offset: [f32; 3],
    /// Its power (1: none).
    pub power: [f32; 3],
    /// Contrast around mid-grey in ACEScct (1: none).
    pub contrast: f32,
    /// Saturation of the least saturated colors, less of the most saturated (1: none).
    pub vibrance: f32,
    /// Saturation around luminance (1: none, 0: grey).
    pub saturation: f32,
}

impl Default for ColorGrading {
    fn default() -> Self {
        Self {
            white_balance: [0.0, 0.0],
            channel_mixer: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            shadows: [1.0; 3],
            midtones: [1.0; 3],
            highlights: [1.0; 3],
            tonal_ranges: [0.0, 0.333, 0.55, 1.0],
            slope: [1.0; 3],
            offset: [0.0; 3],
            power: [1.0; 3],
            contrast: 1.0,
            vibrance: 1.0,
            saturation: 1.0,
        }
    }
}

/// `m · v` for a row-major 3 × 3 matrix.
fn mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

/// `a · b` for row-major 3 × 3 matrices.
fn mat_mul(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    [0, 1, 2].map(|r| [0, 1, 2].map(|c| a[r][0] * b[0][c] + a[r][1] * b[1][c] + a[r][2] * b[2][c]))
}

/// The inverse of a row-major 3 × 3 matrix (in f64: the constants' precision is kept).
fn inverse(m: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let c =
        |r0: usize, c0: usize, r1: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    [
        [c(1, 1, 2, 2), -c(0, 1, 2, 2), c(0, 1, 1, 2)],
        [-c(1, 0, 2, 2), c(0, 0, 2, 2), -c(0, 0, 1, 2)],
        [c(1, 0, 2, 1), -c(0, 0, 2, 1), c(0, 0, 1, 1)],
    ]
    .map(|row| row.map(|v| v / det))
}

fn mul64(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    [0, 1, 2].map(|r| [0, 1, 2].map(|c| a[r][0] * b[0][c] + a[r][1] * b[1][c] + a[r][2] * b[2][c]))
}

fn mul64v(a: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|r| a[r][0] * v[0] + a[r][1] * v[1] + a[r][2] * v[2])
}

/// Linear BT.709 to CIE XYZ (D65).
const XYZ_FROM_BT709: [[f64; 3]; 3] = [
    [0.412_390_8, 0.357_584_3, 0.180_480_8],
    [0.212_639_0, 0.715_168_7, 0.072_192_3],
    [0.019_330_8, 0.119_194_8, 0.950_532_2],
];

/// CIE XYZ to CIECAT16's LMS (Li et al. 2017).
const CAT16: [[f64; 3]; 3] = [
    [0.401_288, 0.650_173, -0.051_461],
    [-0.250_268, 1.204_414, 0.045_854],
    [-0.002_079, 0.048_952, 0.953_127],
];

/// D65's chromaticity.
const D65: [f64; 2] = [0.312_71, 0.329_02];

/// The CIE daylight locus: y of the illuminant with chromaticity x.
fn daylight_y(x: f64) -> f64 {
    -3.0 * x * x + 2.87 * x - 0.275
}

/// The white balance: a von Kries adaptation in CIECAT16 from the white point
/// `temperature` and `tint` give (moved from D65 along and across the daylight locus, as
/// Filament does) to D65, in linear BT.709. Identity at 0, 0.
pub fn white_balance(temperature: f32, tint: f32) -> [[f32; 3]; 3] {
    let k = f64::from(temperature.clamp(-1.0, 1.0));
    let t = f64::from(tint.clamp(-1.0, 1.0));
    if k == 0.0 && t == 0.0 {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let x = D65[0] - k * if k < 0.0 { 0.0214 } else { 0.066 };
    // Along the locus from D65 (D65 itself lies a hair off it), then across.
    let y = D65[1] + daylight_y(x) - daylight_y(D65[0]) + t * 0.066;
    let xyz = |x: f64, y: f64| [x / y, 1.0, (1.0 - x - y) / y];
    let source = mul64v(&CAT16, xyz(x, y));
    let target = mul64v(&CAT16, xyz(D65[0], D65[1]));
    let scale = [
        [target[0] / source[0], 0.0, 0.0],
        [0.0, target[1] / source[1], 0.0],
        [0.0, 0.0, target[2] / source[2]],
    ];
    let to_lms = mul64(&CAT16, &XYZ_FROM_BT709);
    let m = mul64(&inverse(&to_lms), &mul64(&scale, &to_lms));
    m.map(|row| row.map(|v| v as f32))
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl ColorGrading {
    /// Whether it changes nothing.
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    /// White balance, then the channel mixer, as one row-major matrix (linear BT.709).
    pub fn matrix(&self) -> [[f32; 3]; 3] {
        let [temperature, tint] = self.white_balance;
        mat_mul(&self.channel_mixer, &white_balance(temperature, tint))
    }

    /// The grading of a scene-linear BT.709 color, before the tone curve (the CPU
    /// description of `grade` in `grading.slang`).
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut v = mul(&self.matrix(), rgb).map(|c| c.max(0.0));
        // Shadows, mid-tones, highlights.
        let y = dot(v, LUMINANCE);
        let [r0, r1, r2, r3] = self.tonal_ranges;
        let s = 1.0 - smoothstep(r0, r1, y);
        let h = smoothstep(r2, r3, y);
        let m = 1.0 - s - h;
        v = [0, 1, 2]
            .map(|i| v[i] * (s * self.shadows[i] + m * self.midtones[i] + h * self.highlights[i]));
        // In ACEScct: the CDL, then contrast.
        v = [0, 1, 2].map(|i| {
            let mut c = acescct_encode(v[i]) * self.slope[i] + self.offset[i];
            if c > 0.0 {
                c = c.powf(self.power[i]);
            }
            let c = (c - MID_GREY_ACESCCT) * self.contrast + MID_GREY_ACESCCT;
            acescct_decode(c)
        });
        // Vibrance, then saturation, around luminance.
        let y = dot(v, LUMINANCE);
        let high = v[0].max(v[1]).max(v[2]);
        let low = v[0].min(v[1]).min(v[2]);
        let saturated = if high > 1e-6 {
            (high - low) / high
        } else {
            0.0
        };
        let amount = 1.0 + (self.vibrance - 1.0) * (1.0 - saturated.clamp(0.0, 1.0));
        v = v.map(|c| y + amount * (c - y));
        let y = dot(v, LUMINANCE);
        v.map(|c| (y + self.saturation * (c - y)).max(0.0))
    }

    /// The parameters `gradeLut` reads.
    fn params(&self) -> GradingParams {
        let m = self.matrix();
        let v3 = |v: [f32; 3]| [v[0], v[1], v[2], 0.0];
        GradingParams {
            matrix: [v3(m[0]), v3(m[1]), v3(m[2])],
            shadows: v3(self.shadows),
            midtones: v3(self.midtones),
            highlights: v3(self.highlights),
            ranges: self.tonal_ranges,
            slope: v3(self.slope),
            offset: v3(self.offset),
            power: v3(self.power),
            misc: [self.contrast, self.vibrance, self.saturation, 0.0],
        }
    }

    /// A hash of every field (what a LUT was built from; every frame, without allocating).
    fn key(&self) -> u64 {
        let colors = [
            self.shadows,
            self.midtones,
            self.highlights,
            self.slope,
            self.offset,
            self.power,
        ];
        self.white_balance
            .iter()
            .chain(self.channel_mixer.iter().flatten())
            .chain(colors.iter().flatten())
            .chain(&self.tonal_ranges)
            .chain(&[self.contrast, self.vibrance, self.saturation])
            .fold(0xcbf2_9ce4_8422_2325, |h, w| mix(h, u64::from(w.to_bits())))
    }
}

/// FxHash's step (as the cascade cache's).
fn mix(hash: u64, word: u64) -> u64 {
    (hash.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// `GradingParams` in `grading.slang` (176 bytes).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct GradingParams {
    matrix: [[f32; 4]; 3],
    shadows: [f32; 4],
    midtones: [f32; 4],
    highlights: [f32; 4],
    ranges: [f32; 4],
    slope: [f32; 4],
    offset: [f32; 4],
    power: [f32; 4],
    misc: [f32; 4],
}

/// `Push` in `grading.slang` (24 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    lut: u32,
    size: u32,
    curve: u32,
    peak: f32,
    params: u64,
}

const _: () = assert!(core::mem::size_of::<GradingParams>() == 176);
const _: () = assert!(core::mem::size_of::<Push>() == 24);

/// `CURVE_*` in `grading.slang` (the tone mapper's flags for the same curves).
fn curve_flag(curve: ToneCurve) -> u32 {
    match curve {
        ToneCurve::Gt => 0,
        ToneCurve::PbrNeutral => 16,
        ToneCurve::Agx => 32,
        ToneCurve::Aces => 64,
    }
}

/// A view's grading and tone curve as one 3D LUT (ADR 0094): `LUT_SIZE`³ half floats,
/// indexed by the ACEScct encoding of the exposed scene-linear color (from black), holding
/// the tone-mapped color in units of reference white (the graded color in ACEScct with
/// PBR Neutral). Built by a compute pass in the frames whose grading, curve or peak
/// changed; read by the tone mapper's graded entry points
/// ([`crate::post::Composite::grading`]).
pub struct GradingLut {
    device: engine_rhi::ash::Device,
    shader: ComputeShader<Push>,
    image: Image,
    /// What the LUT holds: a hash of the grading, the curve and the peak.
    built: Cell<Option<u64>>,
}

impl core::fmt::Debug for GradingLut {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GradingLut")
            .field("built", &self.built.get())
            .finish_non_exhaustive()
    }
}

impl GradingLut {
    /// Creates the LUT and its pipeline.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) -> Result<Self, RhiError> {
        let [shader] = ComputeSource::create_all(
            device,
            bindless,
            [ComputeSource::new(GRADE_LUT, c"gradeLut", &[])?],
        )?;
        let image = Image::new(
            device,
            allocator,
            bindless,
            LUT_FORMAT,
            LUT_SIZE,
            LUT_SIZE,
            1,
            ImageKind::Volume(LUT_SIZE),
        )?;
        Ok(Self {
            device: device.raw().clone(),
            shader,
            image,
            built: Cell::new(None),
        })
    }

    /// Declares the LUT's build when `grading`, `curve` or `peak` differ from what it
    /// holds, and returns it for [`crate::post::Composite::grading`] (whose `curve` and
    /// `peak` must be these). `None` when the upload ring has no room for its parameters
    /// (the composite then tone-maps without grading).
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        grading: &ColorGrading,
        curve: ToneCurve,
        peak: f32,
    ) -> Option<Texture> {
        let key = mix(
            mix(grading.key(), u64::from(curve_flag(curve))),
            u64::from(peak.to_bits()),
        );
        let stale = self.built.get() != Some(key);
        let params = if stale {
            Some(ring.push(&[grading.params()])?)
        } else {
            None
        };
        let lut = self.image.import(fg, "grading lut");
        if let Some(params) = params {
            let push = Push {
                lut: self.image.storage[0],
                size: LUT_SIZE,
                curve: curve_flag(curve),
                peak,
                params,
            };
            let built = &self.built;
            fg.add_compute("grading lut", bindless)
                .write(lut, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(&self.shader, Dispatch::Threads([LUT_SIZE; 3]), move |_| {
                    // It holds this once the pass is recorded.
                    built.set(Some(key));
                    Some(push)
                });
        }
        Some(lut)
    }

    /// Builds the LUT again in the next frame that grades (after its contents were
    /// lost).
    pub fn invalidate(&self) {
        self.built.set(None);
    }

    /// Frees the LUT.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it after `frame`.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless, frame: u64) {
        // SAFETY: guaranteed by the caller.
        unsafe { self.image.destroy(&self.device, allocator, bindless, frame) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3], tolerance: f32) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() <= tolerance * b[i].abs().max(1.0))
    }

    #[test]
    fn acescct_round_trips_and_keeps_mid_grey() {
        for x in [
            0.0,
            1e-4,
            0.005,
            0.007_812_5,
            0.0079,
            0.18,
            1.0,
            16.0,
            222.0,
        ] {
            let back = acescct_decode(acescct_encode(x));
            assert!((back - x).abs() <= 1e-5 * x.max(1.0), "{x} → {back}");
        }
        assert!((acescct_encode(0.18) - MID_GREY_ACESCCT).abs() < 1e-6);
        // Continuous where the toe meets the logarithm.
        let a = acescct_encode(0.007_812_5);
        let b = acescct_encode(0.007_812_6);
        assert!((a - b).abs() < 1e-5, "{a} {b}");
    }

    #[test]
    fn neutral_grading_changes_nothing() {
        let g = ColorGrading::default();
        assert!(g.is_neutral());
        assert_eq!(
            g.matrix(),
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        );
        for rgb in [
            [0.0, 0.0, 0.0],
            [0.18, 0.18, 0.18],
            [0.9, 0.2, 0.05],
            [12.0, 3.0, 40.0],
            [0.001, 0.02, 0.004],
        ] {
            let out = g.apply(rgb);
            assert!(close(out, rgb, 1e-4), "{rgb:?} → {out:?}");
        }
    }

    #[test]
    fn white_balance_keeps_its_white_and_warms_or_cools() {
        // The white point assumed for the scene becomes white.
        let warm = white_balance(0.5, 0.0);
        let grey = mul(&warm, [0.5, 0.5, 0.5]);
        assert!(grey[0] > grey[2], "warmer: more red than blue: {grey:?}");
        let cool = mul(&white_balance(-0.5, 0.0), [0.5, 0.5, 0.5]);
        assert!(cool[2] > cool[0], "cooler: more blue than red: {cool:?}");
        let magenta = mul(&white_balance(0.0, 0.5), [0.5, 0.5, 0.5]);
        assert!(
            magenta[1] < magenta[0] && magenta[1] < magenta[2],
            "{magenta:?}"
        );
        // A small change stays small (no jump off D65).
        let tiny = mul(&white_balance(1e-4, 0.0), [0.5, 0.5, 0.5]);
        assert!(close(tiny, [0.5, 0.5, 0.5], 1e-3), "{tiny:?}");
    }

    #[test]
    fn saturation_and_contrast_do_what_they_say() {
        let grey = ColorGrading {
            saturation: 0.0,
            ..ColorGrading::default()
        };
        let out = grey.apply([0.6, 0.3, 0.1]);
        assert!(
            (out[0] - out[1]).abs() < 1e-5 && (out[1] - out[2]).abs() < 1e-5,
            "{out:?}"
        );
        // Contrast keeps mid-grey, spreads the rest.
        let contrast = ColorGrading {
            contrast: 1.5,
            ..ColorGrading::default()
        };
        assert!(close(contrast.apply([0.18; 3]), [0.18; 3], 1e-4));
        assert!(contrast.apply([1.0; 3])[0] > 1.0);
        assert!(contrast.apply([0.05; 3])[0] < 0.05);
        // Vibrance moves a dull color more than a vivid one.
        let vibrance = ColorGrading {
            vibrance: 1.5,
            ..ColorGrading::default()
        };
        let spread = |c: [f32; 3]| c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2]);
        let dull = [0.20, 0.18, 0.16];
        let vivid = [0.40, 0.05, 0.02];
        let dull_gain = spread(vibrance.apply(dull)) / spread(dull);
        let vivid_gain = spread(vibrance.apply(vivid)) / spread(vivid);
        assert!(
            dull_gain > vivid_gain && vivid_gain >= 1.0,
            "{dull_gain} {vivid_gain}"
        );
    }

    #[test]
    fn the_cdl_and_the_ranges_reach_their_tones() {
        // Lifting the shadows leaves a bright color alone.
        let shadows = ColorGrading {
            shadows: [2.0; 3],
            ..ColorGrading::default()
        };
        assert!(shadows.apply([0.01; 3])[0] > 0.015);
        assert!(close(shadows.apply([2.0; 3]), [2.0; 3], 1e-4));
        // A slope above 1 brightens everything above ACEScct's 0.
        let slope = ColorGrading {
            slope: [1.1, 1.0, 1.0],
            ..ColorGrading::default()
        };
        let out = slope.apply([0.18; 3]);
        assert!(out[0] > 0.18 && (out[1] - 0.18).abs() < 1e-5, "{out:?}");
    }
}
