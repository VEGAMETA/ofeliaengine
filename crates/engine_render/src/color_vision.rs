//! Color vision deficiencies (ADR 0097, docs/research/color-vision.md): what a player with
//! protanomaly, deuteranomaly or tritanomaly sees, for the developer to check the game
//! against (`Simulate`), and a correction that moves what they cannot tell apart into
//! what they can (`Correct`, daltonization).
//!
//! Both are a transform of the displayed color (linear BT.709, after the tone curve) that
//! the tone mapper's graded LUT holds ([`crate::grading::GradingLut`]): a view with one pays
//! the graded tone mapper's LUT lookup, a view without pays nothing.
//!
//! - Protan and deutan: Machado, Oliveira and Fernandes's matrices (IEEE TVCG 2009), at
//!   their severities 0, 0.1, …, 1, interpolated between (as the authors suggest);
//! - tritan: Brettel, Viénot and Mollon's two half-planes (JOSA A 1997), whose constants in
//!   linear RGB are libDaltonLens's (public domain, THIRD_PARTY_NOTICES.md), mixed with the
//!   original color by the severity: Machado's model was not made for tritans;
//! - the correction: Fidaner, Lin and Ozguven's (2005), the color plus its error against
//!   the simulation, moved into the channels the deficiency keeps.

/// The kind of color vision deficiency: which cones are anomalous or missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Deficiency {
    /// The long-wavelength cones (red): protanomaly, protanopia at full severity.
    Protan,
    /// The medium-wavelength cones (green): deuteranomaly, deuteranopia at full severity.
    Deutan,
    /// The short-wavelength cones (blue): tritanomaly, tritanopia at full severity.
    Tritan,
}

/// What a view does with a deficiency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorVisionMode {
    /// Corrects for it (daltonization): what a player with it turns on.
    #[default]
    Correct,
    /// Shows what a viewer with it sees: for the developer to check the game.
    Simulate,
}

/// A view's color vision setting. The default does nothing and costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ColorVision {
    /// The deficiency, or none.
    pub deficiency: Option<Deficiency>,
    /// How strong it is, 0 (none) to 1 (dichromacy: the cones missing).
    pub severity: f32,
    /// Correct for it or show it.
    pub mode: ColorVisionMode,
}

/// The transform as `grading.slang` takes it: `keep · c + take · sim(c)`, where `sim` is
/// `sim_a · c` on the side of `plane` where `dot(plane, c) > 0`, `sim_b · c` on the other
/// (blended over [`SEPARATION_BAND`]), mixed with `c` by `mix`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Transform {
    pub(crate) sim_a: [[f32; 3]; 3],
    pub(crate) sim_b: [[f32; 3]; 3],
    pub(crate) plane: [f32; 3],
    pub(crate) mix: f32,
    pub(crate) keep: [[f32; 3]; 3],
    pub(crate) take: [[f32; 3]; 3],
}

const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
const ZERO: [[f32; 3]; 3] = [[0.0; 3]; 3];

/// Machado et al.'s simulation matrices for protanomaly, severity 0, 0.1, …, 1 (rows; linear
/// RGB). The table from the paper's page (checked 2026-10-15).
const PROTAN: [[[f32; 3]; 3]; 11] = [
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    [
        [0.856_167, 0.182_038, -0.038_205],
        [0.029_342, 0.955_115, 0.015_544],
        [-0.002_880, -0.001_563, 1.004_443],
    ],
    [
        [0.734_766, 0.334_872, -0.069_637],
        [0.051_840, 0.919_198, 0.028_963],
        [-0.004_928, -0.004_209, 1.009_137],
    ],
    [
        [0.630_323, 0.465_641, -0.095_964],
        [0.069_181, 0.890_046, 0.040_773],
        [-0.006_308, -0.007_724, 1.014_032],
    ],
    [
        [0.539_009, 0.579_343, -0.118_352],
        [0.082_546, 0.866_121, 0.051_332],
        [-0.007_136, -0.011_959, 1.019_095],
    ],
    [
        [0.458_064, 0.679_578, -0.137_642],
        [0.092_785, 0.846_313, 0.060_902],
        [-0.007_494, -0.016_807, 1.024_301],
    ],
    [
        [0.385_450, 0.769_005, -0.154_455],
        [0.100_526, 0.829_802, 0.069_673],
        [-0.007_442, -0.022_190, 1.029_632],
    ],
    [
        [0.319_627, 0.849_633, -0.169_261],
        [0.106_241, 0.815_969, 0.077_790],
        [-0.007_025, -0.028_051, 1.035_076],
    ],
    [
        [0.259_411, 0.923_008, -0.182_420],
        [0.110_296, 0.804_340, 0.085_364],
        [-0.006_276, -0.034_346, 1.040_622],
    ],
    [
        [0.203_876, 0.990_338, -0.194_214],
        [0.112_975, 0.794_542, 0.092_483],
        [-0.005_222, -0.041_043, 1.046_265],
    ],
    [
        [0.152_286, 1.052_583, -0.204_868],
        [0.114_503, 0.786_281, 0.099_216],
        [-0.003_882, -0.048_116, 1.051_998],
    ],
];

/// The same for deuteranomaly.
const DEUTAN: [[[f32; 3]; 3]; 11] = [
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    [
        [0.866_435, 0.177_704, -0.044_139],
        [0.049_567, 0.939_063, 0.011_370],
        [-0.003_453, 0.007_233, 0.996_220],
    ],
    [
        [0.760_729, 0.319_078, -0.079_807],
        [0.090_568, 0.889_315, 0.020_117],
        [-0.006_027, 0.013_325, 0.992_702],
    ],
    [
        [0.675_425, 0.433_850, -0.109_275],
        [0.125_303, 0.847_755, 0.026_942],
        [-0.007_950, 0.018_572, 0.989_378],
    ],
    [
        [0.605_511, 0.528_560, -0.134_071],
        [0.155_318, 0.812_366, 0.032_316],
        [-0.009_376, 0.023_176, 0.986_200],
    ],
    [
        [0.547_494, 0.607_765, -0.155_259],
        [0.181_692, 0.781_742, 0.036_566],
        [-0.010_410, 0.027_275, 0.983_136],
    ],
    [
        [0.498_864, 0.674_741, -0.173_604],
        [0.205_199, 0.754_872, 0.039_929],
        [-0.011_131, 0.030_969, 0.980_162],
    ],
    [
        [0.457_771, 0.731_899, -0.189_670],
        [0.226_409, 0.731_012, 0.042_579],
        [-0.011_595, 0.034_333, 0.977_261],
    ],
    [
        [0.422_823, 0.781_057, -0.203_881],
        [0.245_752, 0.709_602, 0.044_646],
        [-0.011_843, 0.037_423, 0.974_421],
    ],
    [
        [0.392_952, 0.823_610, -0.216_562],
        [0.263_559, 0.690_210, 0.046_232],
        [-0.011_910, 0.040_281, 0.971_630],
    ],
    [
        [0.367_322, 0.860_646, -0.227_968],
        [0.280_085, 0.672_501, 0.047_413],
        [-0.011_820, 0.042_940, 0.968_881],
    ],
];

/// Brettel et al.'s tritanopia in linear RGB (libDaltonLens's constants): the projection on
/// the side of the separation plane where `dot(normal, c) ≥ 0`, and on the other side.
const TRITAN_A: [[f32; 3]; 3] = [
    [1.012_77, 0.135_48, -0.148_26],
    [-0.012_43, 0.868_12, 0.144_31],
    [0.075_89, 0.805_00, 0.119_11],
];
const TRITAN_B: [[f32; 3]; 3] = [
    [0.936_78, 0.189_79, -0.126_57],
    [0.061_54, 0.815_26, 0.123_20],
    [-0.375_62, 1.127_67, 0.247_96],
];
const TRITAN_PLANE: [f32; 3] = [0.039_01, -0.027_88, -0.011_13];

/// Brettel's half-planes meet on their separation plane, with a kink there that a LUT
/// interpolates across (greys came out up to 3 / 255 off): the two projections are
/// blended where `dot(plane, c) / (r + g + b)` lies within this of 0. Both give the same
/// color on the plane, and the blend departs from either by at most about 2 % of
/// `r + g + b` (measured 2026-10-15: greys within 1.5 / 255 through the LUT; 0.005 left
/// 3 / 255).
pub(crate) const SEPARATION_BAND: f32 = 0.02;

/// Where the correction moves each channel's error (rows: the channels it adds to; Fidaner
/// et al.'s for red-green deficiencies): what a protan or deutan loses between red and
/// green goes into green and blue, what a tritan loses of blue into red and green.
const SHIFT_RED_GREEN: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [0.7, 1.0, 0.0], [0.7, 0.0, 1.0]];
const SHIFT_BLUE: [[f32; 3]; 3] = [[1.0, 0.0, 0.7], [0.0, 1.0, 0.7], [0.0, 0.0, 0.0]];

impl ColorVision {
    /// Whether it changes nothing (no deficiency, or severity 0): the view then pays
    /// nothing.
    pub fn is_off(&self) -> bool {
        self.deficiency.is_none() || self.severity <= 0.0
    }

    /// The transform for `grading.slang` (identity when off).
    pub(crate) fn transform(&self) -> Transform {
        let off = Transform {
            sim_a: IDENTITY,
            sim_b: IDENTITY,
            plane: [0.0; 3],
            mix: 0.0,
            keep: IDENTITY,
            take: ZERO,
        };
        let Some(deficiency) = self.deficiency.filter(|_| !self.is_off()) else {
            return off;
        };
        let severity = self.severity.clamp(0.0, 1.0);
        let (sim_a, sim_b, plane, mix, shift) = match deficiency {
            Deficiency::Protan | Deficiency::Deutan => {
                let table = if deficiency == Deficiency::Protan {
                    &PROTAN
                } else {
                    &DEUTAN
                };
                let m = interpolate(table, severity);
                (m, m, [0.0; 3], 1.0, SHIFT_RED_GREEN)
            }
            Deficiency::Tritan => (TRITAN_A, TRITAN_B, TRITAN_PLANE, severity, SHIFT_BLUE),
        };
        let (keep, take) = match self.mode {
            ColorVisionMode::Simulate => (ZERO, IDENTITY),
            // c + E (c − sim c) = (I + E) c − E sim c.
            ColorVisionMode::Correct => (add(&IDENTITY, &shift), scale(&shift, -1.0)),
        };
        Transform {
            sim_a,
            sim_b,
            plane,
            mix,
            keep,
            take,
        }
    }

    /// The transform of a displayed color (linear BT.709; never below 0, a correction may
    /// pass the curve's peak): the CPU description of `color_vision` in `grading.slang`.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let t = self.transform();
        let x = dot(t.plane, rgb) / (rgb[0] + rgb[1] + rgb[2]).max(1e-6);
        let u = ((x + SEPARATION_BAND) / (2.0 * SEPARATION_BAND)).clamp(0.0, 1.0);
        let side = u * u * (3.0 - 2.0 * u);
        let (a, b) = (mul(&t.sim_a, rgb), mul(&t.sim_b, rgb));
        let projected: [f32; 3] = core::array::from_fn(|i| b[i] + side * (a[i] - b[i]));
        let sim: [f32; 3] = core::array::from_fn(|i| rgb[i] + t.mix * (projected[i] - rgb[i]));
        let a = mul(&t.keep, rgb);
        let b = mul(&t.take, sim);
        core::array::from_fn(|i| (a[i] + b[i]).max(0.0))
    }

    /// A hash of what it does (for the LUT's key), without allocating.
    pub(crate) fn key(&self) -> u64 {
        if self.is_off() {
            return 0;
        }
        let deficiency = match self.deficiency {
            Some(Deficiency::Protan) => 1u64,
            Some(Deficiency::Deutan) => 2,
            Some(Deficiency::Tritan) => 3,
            None => 0,
        };
        let mode = match self.mode {
            ColorVisionMode::Correct => 1u64,
            ColorVisionMode::Simulate => 2,
        };
        (deficiency << 40) ^ (mode << 36) ^ u64::from(self.severity.clamp(0.0, 1.0).to_bits())
    }
}

/// `table` (severities 0, 0.1, …, 1) at `severity`, linearly between its entries.
fn interpolate(table: &[[[f32; 3]; 3]; 11], severity: f32) -> [[f32; 3]; 3] {
    let x = severity * 10.0;
    let k = (x.floor() as usize).min(9);
    let f = x - k as f32;
    core::array::from_fn(|r| {
        core::array::from_fn(|c| table[k][r][c] + f * (table[k + 1][r][c] - table[k][r][c]))
    })
}

fn mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    core::array::from_fn(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

fn add(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    core::array::from_fn(|r| core::array::from_fn(|c| a[r][c] + b[r][c]))
}

fn scale(a: &[[f32; 3]; 3], s: f32) -> [[f32; 3]; 3] {
    core::array::from_fn(|r| core::array::from_fn(|c| a[r][c] * s))
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simulate(deficiency: Deficiency, severity: f32) -> ColorVision {
        ColorVision {
            deficiency: Some(deficiency),
            severity,
            mode: ColorVisionMode::Simulate,
        }
    }

    fn correct(deficiency: Deficiency, severity: f32) -> ColorVision {
        ColorVision {
            deficiency: Some(deficiency),
            severity,
            mode: ColorVisionMode::Correct,
        }
    }

    fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    #[test]
    fn off_changes_nothing() {
        let colors = [
            [0.8, 0.1, 0.05],
            [0.1, 0.7, 0.2],
            [0.05, 0.1, 0.9],
            [0.5, 0.5, 0.5],
        ];
        for vision in [
            ColorVision::default(),
            simulate(Deficiency::Protan, 0.0),
            correct(Deficiency::Tritan, 0.0),
        ] {
            assert!(vision.is_off());
            assert_eq!(vision.key(), 0);
            for c in colors {
                assert_eq!(vision.apply(c), c);
            }
        }
    }

    #[test]
    fn simulation_keeps_greys_and_follows_the_published_tables() {
        // Every matrix row sums to 1 (within the table's rounding): greys stay grey.
        for deficiency in [Deficiency::Protan, Deficiency::Deutan, Deficiency::Tritan] {
            for severity in [0.3, 0.55, 1.0] {
                let grey = simulate(deficiency, severity).apply([0.4, 0.4, 0.4]);
                assert!(
                    grey.iter().all(|&g| (g - 0.4).abs() < 2e-3),
                    "{deficiency:?} {severity} {grey:?}"
                );
            }
        }
        // At a table's step, its matrix; between steps, between them.
        let red = [1.0, 0.0, 0.0];
        // (The table's −0.003 882 in blue: no light is below 0.)
        assert_eq!(
            simulate(Deficiency::Protan, 1.0).apply(red),
            [0.152_286, 0.114_503, 0.0]
        );
        let half = simulate(Deficiency::Deutan, 0.55).apply(red)[0];
        assert!(half < 0.547_494 && half > 0.498_864, "{half}");
    }

    #[test]
    fn a_dichromat_confuses_what_the_correction_tells_apart() {
        for deficiency in [Deficiency::Protan, Deficiency::Deutan, Deficiency::Tritan] {
            let seen = simulate(deficiency, 1.0);
            let t = seen.transform();
            // A confusion line: the direction a dichromat's projection loses (its null
            // vector), from a color on the separation plane's first side.
            let n = {
                let m = t.sim_a;
                let c = [
                    m[0][1] * m[1][2] - m[0][2] * m[1][1],
                    m[0][2] * m[1][0] - m[0][0] * m[1][2],
                    m[0][0] * m[1][1] - m[0][1] * m[1][0],
                ];
                let len = dot(c, c).sqrt();
                c.map(|x| x / len)
            };
            // Off the band where the half-planes blend (tritans): on the first one.
            let a = [0.8, 0.15, 0.1];
            let b: [f32; 3] = core::array::from_fn(|i| a[i] + 0.1 * n[i]);
            let off_band = |c: [f32; 3]| dot(t.plane, c) / (c[0] + c[1] + c[2]) >= SEPARATION_BAND;
            assert!(
                t.plane == [0.0; 3] || (off_band(a) && off_band(b)),
                "{deficiency:?}"
            );
            let plain = distance(seen.apply(a), seen.apply(b));
            assert!(
                plain < 0.1 * distance(a, b),
                "{deficiency:?} confuses {a:?} and {b:?}: {plain}"
            );
            let fixed = correct(deficiency, 1.0);
            let corrected = distance(seen.apply(fixed.apply(a)), seen.apply(fixed.apply(b)));
            assert!(
                corrected > 0.4 * distance(a, b) && corrected > 5.0 * plain,
                "{deficiency:?}: {plain} → {corrected}"
            );
        }
    }
}
