//! Screen-space subsurface scattering (ADR 0059): light that enters skin, wax, marble or
//! leaves spreads under the surface and comes out around where it entered.
//!
//! A material with [`crate::material::Subsurface`] names one of the renderer's
//! [`SubsurfaceProfile`]s: how far light spreads per color channel. The forward
//! renderer draws such materials in a pass of their own, "forward subsurface", which writes
//! the specular light and emission into the color target as usual, and the diffuse light
//! (per unit albedo) with the pixel's depth into a second target, the exit albedo into a
//! third. The "subsurface scatter" pass then adds, at every such pixel, the albedo times
//! the diffuse light of the pixels around it weighted by the profile: a disk of samples
//! whose radii are importance-sampled from the profile, rotated per pixel, weighted by
//! the profile at their 3D distance (so light does not cross depth discontinuities) and
//! renormalized per channel. Pixels of other materials are never sampled. The scatter pass
//! draws the subsurface materials' geometry again, depth tested for equality, so it runs
//! for their pixels only.
//!
//! Transmission, light passing through thin parts (ears, leaves, fingers), is computed in
//! the forward pass from the same profile: `T(d) = ∫ R(√(ρ² + d²)) dA` over a plane at
//! distance `d`, the thickness of the material or, for thick objects, the distance to the
//! first occluder towards the light in the shadow map.
//!
//! The profile is a sum of Gaussians (d'Eon and Luebke 2007, the skin profile of their
//! GPU Gems 3 chapter): every quantity the renderer needs (reflectance at a distance, the
//! energy within a radius, the transmission through a slab, the response to a lit
//! half-plane and the radii to sample) comes from [`SubsurfaceProfile`]'s methods and the
//! shaders' `profile_*` functions, so the shape can be swapped in one place (ADR 0059).

use engine_core::half;
use engine_rhi::ash::vk;

/// Format of the diffuse light the forward subsurface pass writes for the scatter pass:
/// rgb the diffuse light per unit albedo (times the mask and the entry albedo), a the
/// pixel's depth from the near plane (0 where no subsurface material was drawn).
pub const IRRADIANCE_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// Format of the exit albedo (rgb) and profile index (a, `index / 255`): 8 bits per
/// channel, like a G-buffer's albedo.
pub const ALBEDO_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;

/// Gaussians a profile may have.
pub const MAX_GAUSSIANS: usize = 6;
/// Profiles a renderer may have (a material names one in 8 bits).
pub const MAX_PROFILES: usize = 255;
/// Samples per pixel of each [`SubsurfaceQuality`].
const BUDGETS: [usize; 4] = [8, 20, 40, 80];
/// Where each budget's kernel starts in a profile's kernel.
const KERNEL_OFFSETS: [usize; 4] = [0, 8, 28, 68];
/// Kernel entries of a profile: every budget's.
const KERNEL_LEN: usize = 148;
const _: () = assert!(KERNEL_OFFSETS[3] + BUDGETS[3] == KERNEL_LEN);
/// Smallest variance (world units²): a zero radius is a Gaussian of 1 µm in a metre-scaled
/// scene, far below a pixel, and keeps the shader's divisions finite.
const MIN_VARIANCE: f64 = 1e-12;
/// The golden angle, π(3 − √5): consecutive samples of the spiral turn by it.
const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;

/// One Gaussian of a profile: `weight / (2π v) · exp(−r² / 2v)` per channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfileGaussian {
    /// Variance `v` in profile units² (millimetres² for the presets).
    pub variance: f32,
    /// Weight per channel (the profile's weights of each channel add up to 1).
    pub weight: [f32; 3],
}

/// A diffusion profile: how much of the light entering a surface comes out at each
/// distance, per color channel, as a sum of Gaussians. Its weights are normalized, so it
/// only moves light around: a surface lit evenly looks exactly as without scattering.
///
/// Distances are in profile units times [`Self::scale`] world units: the presets are in
/// millimetres with a scale of 0.001 (a metre-scaled scene).
///
/// ```
/// use engine_render::subsurface::SubsurfaceProfile;
/// let skin = SubsurfaceProfile::skin();
/// // Everything that enters comes out somewhere...
/// assert!((skin.cdf(1.0)[0] - 1.0).abs() < 1e-6);
/// // ... red light further away than blue.
/// assert!(skin.cdf(0.001)[0] < skin.cdf(0.001)[2]);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubsurfaceProfile {
    gaussians: [ProfileGaussian; MAX_GAUSSIANS],
    count: usize,
    scale: f32,
}

impl SubsurfaceProfile {
    /// Human skin: the six-Gaussian fit of d'Eon and Luebke ("Advanced Techniques for
    /// Realistic Real-Time Skin Rendering", GPU Gems 3, chapter 14, 2007) to a three-layer
    /// skin model, variances in mm², at 1 mm = 0.001 world units.
    pub fn skin() -> Self {
        const SKIN: [(f32, [f32; 3]); 6] = [
            (0.0064, [0.233, 0.455, 0.649]),
            (0.0484, [0.100, 0.336, 0.344]),
            (0.187, [0.118, 0.198, 0.0]),
            (0.567, [0.113, 0.007, 0.007]),
            (1.99, [0.358, 0.004, 0.0]),
            (7.41, [0.078, 0.0, 0.0]),
        ];
        let gaussians = SKIN.map(|(variance, weight)| ProfileGaussian { variance, weight });
        match Self::new(&gaussians) {
            Some(profile) => profile.with_scale(0.001),
            None => unreachable!("the skin profile is valid"),
        }
    }

    /// One Gaussian per channel whose light comes out within `radius` (profile units) for
    /// 99% of it, at 1 unit = 0.001 world units: a simple profile for wax, marble, jade or
    /// fruit. A zero radius does not scatter that channel.
    pub fn from_radius(radius: [f32; 3]) -> Self {
        // 1 − exp(−r² / 2v) = 0.99.
        let variance = |r: f32| r * r / (2.0 * 100f32.ln());
        let gaussians: [ProfileGaussian; 3] = core::array::from_fn(|c| {
            let mut weight = [0.0; 3];
            weight[c] = 1.0;
            ProfileGaussian {
                variance: variance(radius[c].max(0.0)),
                weight,
            }
        });
        match Self::new(&gaussians) {
            Some(profile) => profile.with_scale(0.001),
            None => unreachable!("one Gaussian per channel is valid"),
        }
    }

    /// A profile from up to [`MAX_GAUSSIANS`] Gaussians, each channel's weights normalized
    /// to add up to 1, at a scale of 1. `None` when there are none or too many, a variance
    /// or weight is negative or not finite, or a channel has no weight at all.
    pub fn new(gaussians: &[ProfileGaussian]) -> Option<Self> {
        if gaussians.is_empty() || gaussians.len() > MAX_GAUSSIANS {
            return None;
        }
        let valid = |x: f32| x.is_finite() && x >= 0.0;
        if !gaussians
            .iter()
            .all(|g| valid(g.variance) && g.weight.iter().all(|&w| valid(w)))
        {
            return None;
        }
        let mut sums = [0.0f64; 3];
        for g in gaussians {
            for (sum, w) in sums.iter_mut().zip(g.weight) {
                *sum += f64::from(w);
            }
        }
        if sums.iter().any(|&s| s <= 0.0) {
            return None;
        }
        let mut out = [ProfileGaussian {
            variance: 0.0,
            weight: [0.0; 3],
        }; MAX_GAUSSIANS];
        for (o, g) in out.iter_mut().zip(gaussians) {
            *o = ProfileGaussian {
                variance: g.variance,
                weight: core::array::from_fn(|c| (f64::from(g.weight[c]) / sums[c]) as f32),
            };
        }
        Some(Self {
            gaussians: out,
            count: gaussians.len(),
            scale: 1.0,
        })
    }

    /// The same profile with profile units of `world_units` world units (0.001 for
    /// millimetres in a metre-scaled scene; larger for a stylized, softer look). 0 turns
    /// scattering off while keeping the material's other properties.
    pub fn with_scale(mut self, world_units: f32) -> Self {
        self.scale = if world_units.is_finite() {
            world_units.max(0.0)
        } else {
            0.0
        };
        self
    }

    /// World units per profile unit.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The Gaussians, in profile units, weights normalized.
    pub fn gaussians(&self) -> &[ProfileGaussian] {
        &self.gaussians[..self.count]
    }

    /// Each Gaussian's world-space variance and weights, in f64.
    fn world(&self) -> impl Iterator<Item = (f64, [f64; 3])> + '_ {
        let s = f64::from(self.scale);
        self.gaussians().iter().map(move |g| {
            (
                (f64::from(g.variance) * s * s).max(MIN_VARIANCE),
                g.weight.map(f64::from),
            )
        })
    }

    /// Reflectance per channel at distance `r` (world units) from where unit power
    /// entered, per unit area (it integrates to 1 over the plane).
    pub fn evaluate(&self, r: f32) -> [f32; 3] {
        let r2 = f64::from(r) * f64::from(r);
        let mut out = [0.0f64; 3];
        for (v, w) in self.world() {
            let g = (-r2 / (2.0 * v)).exp() / (2.0 * core::f64::consts::PI * v);
            for (o, w) in out.iter_mut().zip(w) {
                *o += w * g;
            }
        }
        out.map(|x| x as f32)
    }

    /// The share of each channel's light that comes out within `r` world units.
    pub fn cdf(&self, r: f32) -> [f32; 3] {
        let r2 = f64::from(r) * f64::from(r);
        let mut out = [0.0f64; 3];
        for (v, w) in self.world() {
            let inside = -(-r2 / (2.0 * v)).exp_m1();
            for (o, w) in out.iter_mut().zip(w) {
                *o += w * inside;
            }
        }
        out.map(|x| x as f32)
    }

    /// Transmission per channel through a slab `d` world units thick: the profile
    /// integrated over a plane at distance `d`, `Σ wᵢ exp(−d² / 2vᵢ)` (1 at `d = 0`).
    pub fn transmission(&self, d: f32) -> [f32; 3] {
        let d2 = f64::from(d) * f64::from(d);
        let mut out = [0.0f64; 3];
        for (v, w) in self.world() {
            let t = (-d2 / (2.0 * v)).exp();
            for (o, w) in out.iter_mut().zip(w) {
                *o += w * t;
            }
        }
        out.map(|x| x as f32)
    }

    /// The light coming out at signed distance `x` (world units) from the edge of an
    /// evenly lit half-plane (`x > 0` inside it), for unit light: `Σ wᵢ Φ(x / √vᵢ)`.
    /// 0.5 at the edge, 1 deep inside.
    pub fn half_plane(&self, x: f32) -> [f32; 3] {
        let mut out = [0.0f64; 3];
        for (v, w) in self.world() {
            let phi = 0.5 * erfc(-f64::from(x) / (2.0 * v).sqrt());
            for (o, w) in out.iter_mut().zip(w) {
                *o += w * phi;
            }
        }
        out.map(|x| x as f32)
    }

    /// Density over the plane (per world unit²) of the scatter pass's sample radii: the
    /// profile averaged over the channels.
    pub fn sample_density(&self, r: f32) -> f32 {
        let e = self.evaluate(r);
        (e[0] + e[1] + e[2]) / 3.0
    }

    /// The radius (world units) within which a share `u` (0 … 1) of the channel-averaged
    /// profile's light comes out: the inverse of its CDF, the scatter pass's sample radii.
    pub fn sample_radius(&self, u: f32) -> f32 {
        let u = f64::from(u.clamp(0.0, 1.0));
        let averaged = |r: f64| -> f64 {
            self.world()
                .map(|(v, w)| (w[0] + w[1] + w[2]) / 3.0 * -(-r * r / (2.0 * v)).exp_m1())
                .sum()
        };
        // Bisection between 0 and a radius holding everything (10 standard deviations
        // of the widest Gaussian).
        let widest = self.world().map(|(v, _)| v).fold(0.0, f64::max);
        let (mut lo, mut hi) = (0.0f64, 10.0 * widest.sqrt());
        if u >= 1.0 {
            return hi as f32;
        }
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            if averaged(mid) < u {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (0.5 * (lo + hi)) as f32
    }

    /// The profile as the shaders read it.
    pub(crate) fn gpu(&self) -> GpuProfile {
        let mut gaussians = [[0.0f32; 4]; MAX_GAUSSIANS];
        for (out, (v, w)) in gaussians.iter_mut().zip(self.world()) {
            let amplitude = 1.0 / (2.0 * core::f64::consts::PI * v);
            *out = [
                (w[0] * amplitude) as f32,
                (w[1] * amplitude) as f32,
                (w[2] * amplitude) as f32,
                (-core::f64::consts::LOG2_E / (2.0 * v)) as f32,
            ];
        }
        let mut kernel = [[0u32; 4]; KERNEL_LEN];
        let mut radii = [0.0f32; 4];
        for (b, (&n, &offset)) in BUDGETS.iter().zip(&KERNEL_OFFSETS).enumerate() {
            // Stratified in the CDF: sample k holds the share k/n … (k + 1)/n of the
            // light, at its middle, on a golden-angle spiral (Vogel 1979).
            for k in 0..n {
                let r = self.sample_radius((k as f32 + 0.5) / n as f32);
                let density = f64::from(self.sample_density(r));
                let weight = if density > 0.0 {
                    1.0 / (n as f64 * density)
                } else {
                    0.0
                };
                let angle = k as f64 * GOLDEN_ANGLE;
                let w = self
                    .evaluate(r)
                    .map(|e| half::from_f32((f64::from(e) * weight) as f32));
                kernel[offset + k] = [
                    ((f64::from(r) * angle.cos()) as f32).to_bits(),
                    ((f64::from(r) * angle.sin()) as f32).to_bits(),
                    u32::from(w[0]) | u32::from(w[1]) << 16,
                    u32::from(w[2]),
                ];
                radii[b] = radii[b].max(r);
            }
        }
        // Each channel's mean variance, for the scatter pass's depth falloff.
        let mut mean = [0.0f64; 3];
        for (v, w) in self.world() {
            for (m, w) in mean.iter_mut().zip(w) {
                *m += w * v;
            }
        }
        GpuProfile {
            gaussians,
            falloff: mean
                .map(|m| (-core::f64::consts::LOG2_E / (2.0 * m.max(MIN_VARIANCE))) as f32),
            count: self.count as u32,
            radii,
            kernel,
        }
    }
}

/// The complementary error function (W. J. Cody's rational Chebyshev approximation via
/// Numerical Recipes' `erfcc`, relative error below 1.2e-7).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87
                                        + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// `SubsurfaceProfile` in `forward.slang` (2496 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct GpuProfile {
    /// rgb: weight / (2π v); w: −log2(e) / 2v (world units).
    gaussians: [[f32; 4]; MAX_GAUSSIANS],
    /// Per channel −log2(e) / 2v̄, v̄ the channel's mean variance: how the scatter pass's
    /// weights fall off with the depth between two pixels.
    falloff: [f32; 3],
    count: u32,
    /// The largest sample radius of each budget.
    radii: [f32; 4],
    /// Per budget, its samples: offset on the golden-angle spiral (x, y as `f32` bits) and
    /// the profile there over n · density (three halves).
    kernel: [[u32; 4]; KERNEL_LEN],
}

const _: () = assert!(size_of::<GpuProfile>() == 2496);

impl core::fmt::Debug for GpuProfile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuProfile")
            .field("gaussians", &&self.gaussians[..self.count as usize])
            .field("radii", &self.radii)
            .finish_non_exhaustive()
    }
}

/// Samples per pixel of the scatter pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubsurfaceQuality {
    /// 8 samples.
    Low,
    /// 20 samples.
    #[default]
    Medium,
    /// 40 samples.
    High,
    /// 80 samples.
    Ultra,
}

impl SubsurfaceQuality {
    /// Samples per pixel.
    pub fn samples(self) -> u32 {
        BUDGETS[self.index()] as u32
    }

    fn index(self) -> usize {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Ultra => 3,
        }
    }
}

/// Subsurface scattering's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubsurfaceSettings {
    /// Samples per pixel.
    pub quality: SubsurfaceQuality,
}

impl SubsurfaceSettings {
    /// The budget's index (`BUDGET_*` in `forward.slang`).
    pub(crate) fn budget(&self) -> u32 {
        self.quality.index() as u32
    }
}

/// How many pixels a world unit covers at a depth: `pixels_per_unit / (w_scale · depth +
/// w_offset)`, with depth measured from the near plane along the view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Footprint {
    pub pixels_per_unit: f32,
    pub w_scale: f32,
    pub w_offset: f32,
}

impl Footprint {
    /// For a `height`-pixel target seen through `clip_from_world` (unjittered), whose view
    /// axis is `forward` through `near_center` (see `lights::ViewAxis`).
    pub(crate) fn new(
        clip_from_world: engine_core::math::Mat4,
        forward: engine_core::math::Vec3,
        near_center: engine_core::math::Vec3,
        height: u32,
    ) -> Self {
        // A displacement across the view moves NDC y by row1 · δ / w, and clip w grows
        // along the view by |row3.xyz| (0 for orthographic views).
        let row1 = clip_from_world.row(1).truncate();
        let across = row1 - row1.dot(forward) * forward;
        let row3 = clip_from_world.row(3);
        Self {
            pixels_per_unit: across.length() * height as f32 * 0.5,
            w_scale: row3.truncate().dot(forward),
            w_offset: row3.truncate().dot(near_center) + row3.w,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ∫₀^∞ 2πr f(r) dr by the midpoint rule on `r = t / (1 − t)`, which maps [0, 1) onto
    /// [0, ∞), in `n` steps.
    fn radial_integral(f: impl Fn(f64) -> f64, scale: f64, n: usize) -> f64 {
        let mut sum = 0.0;
        for k in 0..n {
            let t = (k as f64 + 0.5) / n as f64;
            let r = scale * t / (1.0 - t);
            let dr = scale / ((1.0 - t) * (1.0 - t)) / n as f64;
            sum += 2.0 * core::f64::consts::PI * r * f(r) * dr;
        }
        sum
    }

    fn profiles() -> [SubsurfaceProfile; 3] {
        [
            SubsurfaceProfile::skin(),
            SubsurfaceProfile::from_radius([4.0, 2.0, 1.0]),
            SubsurfaceProfile::skin().with_scale(0.05),
        ]
    }

    #[test]
    fn profiles_integrate_to_one() {
        for profile in profiles() {
            for c in 0..3 {
                // The narrowest Gaussians need a fine grid near 0: integrate each
                // Gaussian's contribution at its own scale.
                let total: f64 = profile
                    .world()
                    .map(|(v, w)| {
                        radial_integral(
                            |r| {
                                w[c] * (-r * r / (2.0 * v)).exp()
                                    / (2.0 * core::f64::consts::PI * v)
                            },
                            v.sqrt(),
                            20_000,
                        )
                    })
                    .sum();
                assert!(
                    (total - 1.0).abs() < 1e-5,
                    "{profile:?} channel {c}: {total}"
                );
                // And the f32 evaluation agrees with the Gaussians.
                let widest = profile.world().map(|(v, _)| v).fold(0.0, f64::max);
                let numeric = radial_integral(
                    |r| f64::from(profile.evaluate(r as f32)[c]),
                    widest.sqrt(),
                    200_000,
                );
                assert!((numeric - 1.0).abs() < 2e-3, "{profile:?} {c}: {numeric}");
            }
        }
    }

    #[test]
    fn the_cdf_is_the_integral_of_the_profile_and_inverts() {
        for profile in profiles() {
            let widest = profile.world().map(|(v, _)| v).fold(0.0, f64::max).sqrt();
            for r in [0.1, 0.5, 1.0, 2.0, 4.0].map(|k| k * widest) {
                let cdf = profile.cdf(r as f32);
                for c in 0..3 {
                    let numeric: f64 = profile
                        .world()
                        .map(|(v, w)| {
                            // ∫₀^r 2πρ G dρ with the midpoint rule.
                            let n = 20_000;
                            (0..n)
                                .map(|k| {
                                    let rho = (k as f64 + 0.5) / n as f64 * r;
                                    2.0 * core::f64::consts::PI
                                        * rho
                                        * w[c]
                                        * (-rho * rho / (2.0 * v)).exp()
                                        / (2.0 * core::f64::consts::PI * v)
                                        * (r / n as f64)
                                })
                                .sum::<f64>()
                        })
                        .sum();
                    assert!(
                        (f64::from(cdf[c]) - numeric).abs() < 1e-5,
                        "{c} at {r}: {cdf:?} vs {numeric}"
                    );
                }
            }
            // The sample radii invert the channel-averaged CDF.
            for k in 1..100 {
                let u = k as f32 / 100.0;
                let r = profile.sample_radius(u);
                let cdf = profile.cdf(r);
                let averaged = (cdf[0] + cdf[1] + cdf[2]) / 3.0;
                assert!((averaged - u).abs() < 1e-5, "{u} → {r} → {averaged}");
            }
        }
    }

    #[test]
    fn transmission_is_the_profile_integrated_over_a_slab() {
        for profile in profiles() {
            let widest = profile.world().map(|(v, _)| v).fold(0.0, f64::max).sqrt();
            for d in [0.0, 0.3, 1.0, 2.5].map(|k| k * widest) {
                let t = profile.transmission(d as f32);
                for c in 0..3 {
                    let numeric: f64 = profile
                        .world()
                        .map(|(v, w)| {
                            radial_integral(
                                |rho| {
                                    let r2 = rho * rho + d * d;
                                    w[c] * (-r2 / (2.0 * v)).exp()
                                        / (2.0 * core::f64::consts::PI * v)
                                },
                                v.sqrt(),
                                20_000,
                            )
                        })
                        .sum();
                    assert!(
                        (f64::from(t[c]) - numeric).abs() < 1e-5,
                        "{c} at {d}: {t:?} vs {numeric}"
                    );
                }
            }
        }
    }

    #[test]
    fn half_plane_response_is_the_profile_integrated_over_the_lit_half() {
        for profile in profiles() {
            let widest = profile.world().map(|(v, _)| v).fold(0.0, f64::max).sqrt();
            for x in [-2.0, -0.5, 0.0, 0.2, 1.0, 3.0].map(|k| k * widest) {
                let s = profile.half_plane(x as f32);
                for c in 0..3 {
                    // Each Gaussian's 1D marginal integrated over the lit side, t > −x.
                    let numeric: f64 = profile
                        .world()
                        .map(|(v, w)| {
                            let sigma = v.sqrt();
                            let (lo, hi) = (-x, 12.0 * sigma);
                            if lo >= hi {
                                return 0.0;
                            }
                            let n = 40_000;
                            let dt = (hi - lo) / n as f64;
                            w[c] * (0..n)
                                .map(|k| {
                                    let t = lo + (k as f64 + 0.5) * dt;
                                    (-t * t / (2.0 * v)).exp()
                                        / (2.0 * core::f64::consts::PI * v).sqrt()
                                        * dt
                                })
                                .sum::<f64>()
                        })
                        .sum();
                    assert!(
                        (f64::from(s[c]) - numeric).abs() < 1e-5,
                        "{c} at {x}: {s:?} vs {numeric}"
                    );
                }
            }
            let edge = profile.half_plane(0.0);
            assert!(edge.iter().all(|&e| (e - 0.5).abs() < 1e-6), "{edge:?}");
        }
    }

    #[test]
    fn kernels_integrate_each_channel() {
        for profile in profiles() {
            let gpu = profile.gpu();
            for (b, (&n, &offset)) in BUDGETS.iter().zip(&KERNEL_OFFSETS).enumerate() {
                let mut sums = [0.0f64; 3];
                let mut last = 0.0;
                for &[x, y, rg, b] in &gpu.kernel[offset..offset + n] {
                    let r = f32::from_bits(x).hypot(f32::from_bits(y));
                    assert!(r >= last, "radii grow");
                    last = r;
                    let w = [rg as u16, (rg >> 16) as u16, b as u16].map(half::to_f32);
                    for (s, w) in sums.iter_mut().zip(w) {
                        *s += f64::from(w);
                    }
                }
                assert!((gpu.radii[b] - last).abs() <= 1e-6 * last.max(1e-9));
                // The estimate of ∫R dA = 1 the samples make without renormalizing: the
                // shader divides by the weights it used, so this only bounds its noise.
                let tolerance = [0.25, 0.1, 0.05, 0.03][b];
                for (c, s) in sums.iter().enumerate() {
                    assert!(
                        (s - 1.0).abs() < tolerance,
                        "{n} samples, channel {c}: {s} ({profile:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn invalid_profiles_are_refused_and_weights_normalized() {
        let g = |variance, weight| ProfileGaussian { variance, weight };
        assert!(SubsurfaceProfile::new(&[]).is_none());
        assert!(SubsurfaceProfile::new(&[g(1.0, [1.0, 1.0, 0.0])]).is_none());
        assert!(SubsurfaceProfile::new(&[g(-1.0, [1.0; 3])]).is_none());
        assert!(SubsurfaceProfile::new(&[g(f32::NAN, [1.0; 3])]).is_none());
        assert!(SubsurfaceProfile::new(&[g(1.0, [1.0; 3]); 7]).is_none());
        let p = SubsurfaceProfile::new(&[g(1.0, [2.0, 1.0, 4.0]), g(2.0, [2.0, 3.0, 0.0])])
            .unwrap_or_else(|| unreachable!());
        let sums: [f32; 3] =
            core::array::from_fn(|c| p.gaussians().iter().map(|g| g.weight[c]).sum());
        assert!(sums.iter().all(|s| (s - 1.0).abs() < 1e-6), "{sums:?}");
        // A zero scale scatters nothing: all of it within any radius.
        let flat = SubsurfaceProfile::skin().with_scale(0.0);
        assert!(flat.cdf(1e-5).iter().all(|&c| c > 0.999_999));
        assert!(flat.gpu().radii.iter().all(|&r| r < 1e-4));
    }
}
