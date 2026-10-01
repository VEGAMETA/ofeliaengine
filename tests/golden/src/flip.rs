//! LDR-FLIP (Andersson, Nilsson, Akenine-Möller, Oskarsson, Åström, Fairchild, "FLIP: A
//! Difference Evaluator for Alternating Images", HPG 2020): the perceived difference
//! between two images when a viewer flips between them, per pixel in 0..=1.
//!
//! A port of the CPU path of NVIDIA's reference implementation (`src/cpp/FLIP.h`,
//! BSD-3-Clause, credited in THIRD_PARTY_NOTICES.md): the same constants, filters and
//! order of operations, so the mean over its test pair matches the published 0.159691.
//!
//! Two halves, combined per pixel as `color ^ (1 - feature)`:
//! - **color:** both images in YCxCz (an opponent space), blurred by the contrast
//!   sensitivity of the eye at the viewing distance, then the HyAB distance in
//!   Hunt-adjusted L*a*b*, remapped so that small differences stay small;
//! - **features:** edges and points (first and second derivatives of a Gaussian on the
//!   luminance) present in one image and not the other.

/// Pixels per degree of visual angle: how far the viewer sits. FLIP's default, 0.7 m
/// from a 0.7 m wide 3840-pixel monitor: about 67.
pub fn pixels_per_degree(distance_m: f32, resolution_x: f32, monitor_width_m: f32) -> f32 {
    distance_m * (resolution_x / monitor_width_m) * (core::f32::consts::PI / 180.0)
}

/// FLIP's default viewing conditions.
pub const DEFAULT_PPD: f32 = 67.020_65;

// FLIP's constants (`xFLIPConstants`).
const GQC: f32 = 0.7;
const GPC: f32 = 0.4;
const GPT: f32 = 0.95;
const GW: f32 = 0.082;
const GQF: f32 = 0.5;

// The contrast sensitivity functions as sums of Gaussians (`xGaussianConstants`): the
// achromatic, red-green and blue-yellow channels.
const A1: [f32; 3] = [1.0, 1.0, 34.1];
const B1: [f32; 3] = [0.0047, 0.0053, 0.04];
const A2: [f32; 3] = [0.0, 0.0, 13.5];
const B2: [f32; 3] = [1.0e-5, 1.0e-5, 0.025];

const ILLUMINANT: [f32; 3] = [0.950_428_5, 1.0, 1.088_900_4];
const INV_ILLUMINANT: [f32; 3] = [1.052_156_9, 1.0, 0.918_357_7];

type Color = [f32; 3];

/// An error map and its summary.
#[derive(Debug, Clone)]
pub struct FlipResult {
    /// Width of the map.
    pub width: usize,
    /// Height of the map.
    pub height: usize,
    /// Per-pixel error in 0..=1, rows top to bottom.
    pub error: Vec<f32>,
}

impl FlipResult {
    /// The mean error: FLIP's headline number.
    pub fn mean(&self) -> f32 {
        let sum: f64 = self.error.iter().map(|&e| f64::from(e)).sum();
        (sum / self.error.len().max(1) as f64) as f32
    }

    /// The error below which `fraction` of the pixels lie (0.99: the 99th percentile).
    pub fn percentile(&self, fraction: f32) -> f32 {
        if self.error.is_empty() {
            return 0.0;
        }
        let mut sorted = self.error.clone();
        sorted.sort_unstable_by(f32::total_cmp);
        let index = ((sorted.len() - 1) as f32 * fraction.clamp(0.0, 1.0)).round() as usize;
        sorted[index]
    }

    /// The largest error.
    pub fn max(&self) -> f32 {
        self.error.iter().copied().fold(0.0, f32::max)
    }

    /// The map as 8-bit RGB through FLIP's own convention (black to yellow, the "magma"
    /// colour map approximated by a few stops), for looking at differences.
    pub fn heatmap_rgb8(&self) -> Vec<u8> {
        const STOPS: [[f32; 3]; 5] = [
            [0.0, 0.0, 0.016],
            [0.317, 0.071, 0.486],
            [0.716, 0.215, 0.475],
            [0.986, 0.535, 0.382],
            [0.987, 0.991, 0.750],
        ];
        let mut out = Vec::with_capacity(self.error.len() * 3);
        for &e in &self.error {
            let t = e.clamp(0.0, 1.0) * (STOPS.len() - 1) as f32;
            let i = (t as usize).min(STOPS.len() - 2);
            let f = t - i as f32;
            for (a, b) in STOPS[i].iter().zip(&STOPS[i + 1]) {
                out.push(((a + (b - a) * f) * 255.0 + 0.5) as u8);
            }
        }
        out
    }
}

/// sRGB-encoded channel (0..=1) to linear.
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_rgb_to_xyz(c: Color) -> Color {
    let a = [
        [
            10_135_552.0 / 24_577_794.0,
            8_788_810.0 / 24_577_794.0,
            4_435_075.0 / 24_577_794.0,
        ],
        [
            2_613_072.0 / 12_288_897.0,
            8_788_810.0 / 12_288_897.0,
            887_015.0 / 12_288_897.0,
        ],
        [
            1_425_312.0 / 73_733_382.0,
            8_788_810.0 / 73_733_382.0,
            70_074_185.0 / 73_733_382.0,
        ],
    ];
    mul(a, c)
}

fn xyz_to_linear_rgb(c: Color) -> Color {
    let a = [
        [3.241_003_3, -1.537_398_9, -0.498_615_87],
        [-0.969_224_3, 1.875_930_1, 0.041_554_224],
        [0.055_639_423, -0.204_011_2, 1.057_148_9],
    ];
    mul(a, c)
}

fn mul(a: [[f32; 3]; 3], c: Color) -> Color {
    [
        a[0][0] * c[0] + a[0][1] * c[1] + a[0][2] * c[2],
        a[1][0] * c[0] + a[1][1] * c[1] + a[1][2] * c[2],
        a[2][0] * c[0] + a[2][1] * c[1] + a[2][2] * c[2],
    ]
}

fn xyz_to_cielab(c: Color) -> Color {
    const DELTA: f32 = 6.0 / 29.0;
    const DELTA_CUBE: f32 = DELTA * DELTA * DELTA;
    const FACTOR: f32 = 1.0 / (3.0 * DELTA * DELTA);
    const TERM: f32 = 4.0 / 29.0;
    let f = |v: f32| {
        if v > DELTA_CUBE {
            v.powf(1.0 / 3.0)
        } else {
            FACTOR * v + TERM
        }
    };
    let x = f(c[0] * INV_ILLUMINANT[0]);
    let y = f(c[1] * INV_ILLUMINANT[1]);
    let z = f(c[2] * INV_ILLUMINANT[2]);
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn xyz_to_ycxcz(c: Color) -> Color {
    let x = c[0] * INV_ILLUMINANT[0];
    let y = c[1] * INV_ILLUMINANT[1];
    let z = c[2] * INV_ILLUMINANT[2];
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn ycxcz_to_xyz(c: Color) -> Color {
    let y = (c[0] + 16.0) / 116.0;
    let cx = c[1] / 500.0;
    let cz = c[2] / 200.0;
    [
        (y + cx) * ILLUMINANT[0],
        y * ILLUMINANT[1],
        (y - cz) * ILLUMINANT[2],
    ]
}

fn hunt(luminance: f32, chrominance: f32) -> f32 {
    0.01 * luminance * chrominance
}

fn hyab(a: Color, b: Color) -> f32 {
    (a[0] - b[0]).abs() + ((a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn max_distance() -> f32 {
    let green = xyz_to_cielab(linear_rgb_to_xyz([0.0, 1.0, 0.0]));
    let blue = xyz_to_cielab(linear_rgb_to_xyz([0.0, 0.0, 1.0]));
    let green = [green[0], hunt(green[0], green[1]), hunt(green[0], green[2])];
    let blue = [blue[0], hunt(blue[0], blue[1]), hunt(blue[0], blue[2])];
    hyab(green, blue).powf(GQC)
}

fn gaussian_ab(x2: f32, a: f32, b: f32) -> f32 {
    let pi = core::f32::consts::PI;
    a * (pi / b).sqrt() * (-pi * pi * x2 / b).exp()
}

fn gaussian_sqrt(x2: f32, a: f32, b: f32) -> f32 {
    let pi = core::f32::consts::PI;
    (a * (pi / b).sqrt()).sqrt() * (-pi * pi * x2 / b).exp()
}

/// The separable spatial filters: `[Y, Cx]` weights, and the two halves of the
/// blue-yellow channel's sum of Gaussians (FLIP's `separatedConvolutions.pdf`).
fn spatial_filters(ppd: f32) -> (Vec<[f32; 2]>, Vec<[f32; 2]>) {
    let pi_sq = core::f32::consts::PI * core::f32::consts::PI;
    let max_b = B1.iter().chain(B2.iter()).copied().fold(0.0, f32::max);
    let radius = (3.0 * (max_b / (2.0 * pi_sq)).sqrt() * ppd).ceil() as i32;
    let dx = 1.0 / ppd;
    let (mut ycx, mut cz) = (Vec::new(), Vec::new());
    let (mut sum_ycx, mut sum_cz) = ([0.0f32; 2], [0.0f32; 2]);
    for x in -radius..=radius {
        let ix = x as f32 * dx;
        let ix2 = ix * ix;
        let a = [
            gaussian_ab(ix2, A1[0], B1[0]),
            gaussian_ab(ix2, A1[1], B1[1]),
        ];
        let b = [
            gaussian_sqrt(ix2, A1[2], B1[2]),
            gaussian_sqrt(ix2, A2[2], B2[2]),
        ];
        for c in 0..2 {
            sum_ycx[c] += a[c];
            sum_cz[c] += b[c];
        }
        ycx.push(a);
        cz.push(b);
    }
    let norm_cz = 1.0 / (sum_cz[0] * sum_cz[0] + sum_cz[1] * sum_cz[1]).sqrt();
    for w in &mut ycx {
        w[0] /= sum_ycx[0];
        w[1] /= sum_ycx[1];
    }
    for w in &mut cz {
        w[0] *= norm_cz;
        w[1] *= norm_cz;
    }
    (ycx, cz)
}

/// The feature filter: Gaussian, first and second derivative weights.
fn feature_filter(ppd: f32) -> Vec<Color> {
    let std_dev = 0.5 * GW * ppd;
    let radius = (3.0 * std_dev).ceil() as i32;
    let mut filter = Vec::new();
    let (mut g_sum, mut dg_neg, mut dg_pos, mut ddg_neg, mut ddg_pos) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for x in -radius..=radius {
        let xf = x as f32;
        let g = (-(xf * xf) / (2.0 * std_dev * std_dev)).exp();
        g_sum += g;
        let dg = -xf * g;
        if dg > 0.0 {
            dg_pos += dg;
        } else {
            dg_neg -= dg;
        }
        let ddg = (xf * xf / (std_dev * std_dev) - 1.0) * g;
        if ddg > 0.0 {
            ddg_pos += ddg;
        } else {
            ddg_neg -= ddg;
        }
        filter.push([g, dg, ddg]);
    }
    for w in &mut filter {
        w[0] /= g_sum;
        w[1] /= if w[1] > 0.0 { dg_pos } else { dg_neg };
        w[2] /= if w[2] > 0.0 { ddg_pos } else { ddg_neg };
    }
    filter
}

/// Runs `f(row, out_row)` for every row, on all cores.
fn for_rows<T: Send>(
    height: usize,
    width: usize,
    out: &mut [T],
    f: impl Fn(usize, &mut [T]) + Sync,
) {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(height.max(1));
    let rows_per = height.div_ceil(threads.max(1)).max(1);
    std::thread::scope(|scope| {
        for (chunk, rows) in out.chunks_mut(rows_per * width).enumerate() {
            let f = &f;
            scope.spawn(move || {
                for (i, row) in rows.chunks_mut(width).enumerate() {
                    f(chunk * rows_per + i, row);
                }
            });
        }
    });
}

/// LDR-FLIP of `test` against `reference`, both linear RGB in 0..=1, `width × height`,
/// rows top to bottom, seen at `ppd` pixels per degree ([`DEFAULT_PPD`]).
///
/// # Panics
///
/// If the images' sizes differ from `width × height`.
pub fn flip(
    reference: &[Color],
    test: &[Color],
    width: usize,
    height: usize,
    ppd: f32,
) -> FlipResult {
    assert_eq!(reference.len(), width * height, "reference size");
    assert_eq!(test.len(), width * height, "test size");
    let to_ycxcz = |c: &Color| xyz_to_ycxcz(linear_rgb_to_xyz(*c));
    let reference: Vec<Color> = reference.iter().map(to_ycxcz).collect();
    let test: Vec<Color> = test.iter().map(to_ycxcz).collect();
    let (w, h) = (width as i32, height as i32);

    // Color: spatial filter along x, then along y with the difference.
    let (filter_ycx, filter_cz) = spatial_filters(ppd);
    let half = (filter_ycx.len() / 2) as i32;
    // Per pixel: reference YCx, reference Cz halves, test YCx, test Cz halves.
    let mut stage = vec![[0.0f32; 8]; width * height];
    for_rows(height, width, &mut stage, |y, row| {
        for (x, out) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 8];
            for ix in -half..=half {
                let xx = (x as i32 + ix).clamp(0, w - 1) as usize;
                let wy = filter_ycx[(ix + half) as usize];
                let wz = filter_cz[(ix + half) as usize];
                let r = reference[y * width + xx];
                let t = test[y * width + xx];
                acc[0] += wy[0] * r[0];
                acc[1] += wy[1] * r[1];
                acc[2] += wz[0] * r[2];
                acc[3] += wz[1] * r[2];
                acc[4] += wy[0] * t[0];
                acc[5] += wy[1] * t[1];
                acc[6] += wz[0] * t[2];
                acc[7] += wz[1] * t[2];
            }
            *out = acc;
        }
    });
    let cmax = max_distance();
    let pccmax = GPC * cmax;
    let mut color = vec![0.0f32; width * height];
    for_rows(height, width, &mut color, |y, row| {
        for (x, out) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 8];
            for iy in -half..=half {
                let yy = (y as i32 + iy).clamp(0, h - 1) as usize;
                let wy = filter_ycx[(iy + half) as usize];
                let wz = filter_cz[(iy + half) as usize];
                let s = stage[yy * width + x];
                for k in [0, 4] {
                    acc[k] += wy[0] * s[k];
                    acc[k + 1] += wy[1] * s[k + 1];
                    acc[k + 2] += wz[0] * s[k + 2];
                    acc[k + 3] += wz[1] * s[k + 3];
                }
            }
            let lab = |k: usize| {
                let ycxcz = [acc[k], acc[k + 1], acc[k + 2] + acc[k + 3]];
                let rgb = xyz_to_linear_rgb(ycxcz_to_xyz(ycxcz)).map(|c| c.clamp(0.0, 1.0));
                let lab = xyz_to_cielab(linear_rgb_to_xyz(rgb));
                [lab[0], hunt(lab[0], lab[1]), hunt(lab[0], lab[2])]
            };
            let mut difference = hyab(lab(0), lab(4)).powf(GQC);
            if difference < pccmax {
                difference *= GPT / pccmax;
            } else {
                difference = GPT + ((difference - pccmax) / (cmax - pccmax)) * (1.0 - GPT);
            }
            *out = difference;
        }
    });

    // Features: derivatives of a Gaussian on the normalized luminance.
    let filter = feature_filter(ppd);
    let half = (filter.len() / 2) as i32;
    // Per pixel: reference dx, ddx, gauss; test dx, ddx, gauss.
    let mut stage = vec![[0.0f32; 6]; width * height];
    for_rows(height, width, &mut stage, |y, row| {
        for (x, out) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 6];
            for ix in -half..=half {
                let xx = (x as i32 + ix).clamp(0, w - 1) as usize;
                let f = filter[(ix + half) as usize];
                let r = reference[y * width + xx][0] / 116.0 + 16.0 / 116.0;
                let t = test[y * width + xx][0] / 116.0 + 16.0 / 116.0;
                acc[0] += f[1] * r;
                acc[1] += f[2] * r;
                acc[2] += f[0] * r;
                acc[3] += f[1] * t;
                acc[4] += f[2] * t;
                acc[5] += f[0] * t;
            }
            *out = acc;
        }
    });
    let mut error = vec![0.0f32; width * height];
    let normalization = 1.0 / 2.0f32.sqrt();
    for_rows(height, width, &mut error, |y, row| {
        for (x, out) in row.iter_mut().enumerate() {
            // dx, ddx, dy, ddy for reference then test.
            let mut acc = [0.0f32; 8];
            for iy in -half..=half {
                let yy = (y as i32 + iy).clamp(0, h - 1) as usize;
                let f = filter[(iy + half) as usize];
                let s = stage[yy * width + x];
                for (k, o) in [(0usize, 0usize), (4, 3)] {
                    acc[k] += f[0] * s[o];
                    acc[k + 1] += f[0] * s[o + 1];
                    acc[k + 2] += f[1] * s[o + 2];
                    acc[k + 3] += f[2] * s[o + 2];
                }
            }
            let edge = |k: usize| (acc[k] * acc[k] + acc[k + 2] * acc[k + 2]).sqrt();
            let point = |k: usize| (acc[k + 1] * acc[k + 1] + acc[k + 3] * acc[k + 3]).sqrt();
            let edge_difference = (edge(0) - edge(4)).abs();
            let point_difference = (point(0) - point(4)).abs();
            let feature = (normalization * edge_difference.max(point_difference)).powf(GQF);
            *out = color[y * width + x].powf(1.0 - feature);
        }
    });
    FlipResult {
        width,
        height,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(c: Color, n: usize) -> Vec<Color> {
        vec![c; n]
    }

    #[test]
    fn identical_images_differ_by_nothing() {
        let (w, h) = (48, 32);
        let image: Vec<Color> = (0..w * h)
            .map(|i| {
                let v = (i % 7) as f32 / 7.0;
                [v, 1.0 - v, 0.5]
            })
            .collect();
        let result = flip(&image, &image, w, h, DEFAULT_PPD);
        assert_eq!(result.max(), 0.0);
    }

    #[test]
    fn green_against_blue_is_the_largest_difference() {
        let (w, h) = (32, 32);
        let result = flip(
            &flat([0.0, 1.0, 0.0], w * h),
            &flat([0.0, 0.0, 1.0], w * h),
            w,
            h,
            DEFAULT_PPD,
        );
        // The colour distance is FLIP's maximum; flat images have no features.
        assert!((result.mean() - 1.0).abs() < 1e-3, "{}", result.mean());
    }

    #[test]
    fn a_small_change_is_small_and_grows() {
        let (w, h) = (32, 32);
        let grey = flat([0.5, 0.5, 0.5], w * h);
        let a = flip(&grey, &flat([0.51, 0.5, 0.5], w * h), w, h, DEFAULT_PPD).mean();
        let b = flip(&grey, &flat([0.6, 0.5, 0.5], w * h), w, h, DEFAULT_PPD).mean();
        assert!(a > 0.0 && a < 0.05 && b > a, "{a} {b}");
    }

    #[test]
    fn the_default_viewing_conditions() {
        assert!((pixels_per_degree(0.7, 3840.0, 0.7) - DEFAULT_PPD).abs() < 1e-3);
    }
}
