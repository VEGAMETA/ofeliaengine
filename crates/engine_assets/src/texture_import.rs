//! Texture import (ADR 0040): PNG and JPEG decoded to RGBA8, and full mip chains filtered
//! the way each kind of texture needs.
//!
//! - **Color** (base color, emissive): sRGB decoded to linear light before filtering, and
//!   color weighted by alpha, so the black of cut-out texels does not bleed into the
//!   edges of foliage.
//! - **Masked color** (alpha-tested materials): as color, and every level's alpha is
//!   rescaled so that the share of texels at or above the cutoff stays that of the full
//!   size (Castaño, "Computing Alpha Mipmaps", 2010). Plain filtering thins foliage out in
//!   the distance.
//! - **Data** (metallic-roughness, occlusion): filtered per channel, linearly.
//! - **Normal maps**: filtered as vectors and renormalized.
//!
//! Levels are reduced with a Kaiser-windowed sinc (half-width 3 destination texels,
//! α = 4, NVTT's defaults; parameters only, no code taken). It keeps detail a 2 × 2 box
//! blurs away. Sizes halve with rounding down, as Vulkan's mip levels do; odd sizes are
//! resampled by their exact ratio. Edges clamp.

use engine_core::math::Vec4;
use std::io::Cursor;

/// What a texture holds, which decides how its levels are filtered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextureUsage {
    /// sRGB color with straight alpha.
    Color,
    /// sRGB color whose alpha is tested against `cutoff` (glTF's `MASK`).
    ColorMasked {
        /// Alpha cutoff.
        cutoff: f32,
    },
    /// Linear data, one value per channel.
    Data,
    /// Tangent-space normal map: RGB = XYZ × 0.5 + 0.5.
    Normal,
}

impl TextureUsage {
    /// Whether the texels are sRGB-encoded (the GPU view decodes them).
    pub fn srgb(self) -> bool {
        matches!(self, Self::Color | Self::ColorMasked { .. })
    }
}

/// A decoded image, RGBA8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixels, 4 bytes each, rows top to bottom.
    pub rgba: Vec<u8>,
}

/// A texture with every mip level, ready for upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureImage {
    /// Width of level 0.
    pub width: u32,
    /// Height of level 0.
    pub height: u32,
    /// Whether the texels are sRGB-encoded.
    pub srgb: bool,
    /// Levels from the full size down to 1 × 1, RGBA8 each.
    pub levels: Vec<Vec<u8>>,
}

/// Why an image could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// Neither PNG, JPEG nor Aseprite.
    UnknownFormat,
    /// The decoder failed.
    Decode(String),
}

impl core::fmt::Display for ImageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownFormat => write!(f, "not a PNG, JPEG or Aseprite image"),
            Self::Decode(e) => write!(f, "image decoding failed: {e}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Decodes a PNG, a JPEG or an Aseprite file (recognized by their signatures) to RGBA8; an
/// Aseprite file's frames come as a sheet ([`crate::aseprite::Aseprite::sheet`]).
pub fn decode_image(bytes: &[u8]) -> Result<Image, ImageError> {
    engine_profiler::zone!("assets::decode_image");
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        decode_jpeg(bytes)
    } else if crate::aseprite::is_aseprite(bytes) {
        crate::aseprite::Aseprite::read(bytes)?.sheet()
    } else {
        Err(ImageError::UnknownFormat)
    }
}

fn decode_png(bytes: &[u8]) -> Result<Image, ImageError> {
    let error = |e: png::DecodingError| ImageError::Decode(e.to_string());
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // Palettes and low bit depths expanded, 16-bit channels reduced to 8.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(error)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| ImageError::Decode(String::from("image too large")))?;
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer).map_err(error)?;
    buffer.truncate(info.buffer_size());
    let (width, height) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => {
            return Err(ImageError::Decode(String::from("palette not expanded")));
        }
    };
    Ok(Image {
        width,
        height,
        rgba,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<Image, ImageError> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    let rgba = decoder
        .decode()
        .map_err(|e| ImageError::Decode(format!("{e:?}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| ImageError::Decode(String::from("no image information")))?;
    Ok(Image {
        width: u32::from(info.width),
        height: u32::from(info.height),
        rgba,
    })
}

/// sRGB 8-bit → linear.
fn srgb_to_linear_table() -> [f32; 256] {
    core::array::from_fn(|i| {
        let c = i as f32 / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    })
}

/// Linear → sRGB 8-bit (exact: the code whose linear value is closest), through 4096
/// linear steps. Codes' midpoints lie further apart than a step, so the nearest code
/// changes at most once inside one: each step keeps the code at its start and the first
/// value that takes the next. Made once (`get`).
struct LinearToSrgb {
    steps: Vec<(u8, f32)>,
}

impl LinearToSrgb {
    const STEPS: usize = 4096;

    /// The table, made the first time.
    fn get() -> &'static Self {
        static TABLE: std::sync::OnceLock<LinearToSrgb> = std::sync::OnceLock::new();
        TABLE.get_or_init(|| Self::new(&srgb_to_linear_table()))
    }

    fn new(decode: &[f32; 256]) -> Self {
        let steps = (0..=Self::STEPS)
            .map(|i| {
                // `v * STEPS` is exact (a power of two): step `i` is [i, i + 1) / STEPS.
                let start = i as f32 / Self::STEPS as f32;
                let end = (i + 1) as f32 / Self::STEPS as f32;
                let code = nearest_code(decode, start);
                // The first value of the step whose nearest code is the next, by halving
                // the step's floats (in order, as their bits are).
                let (mut lo, mut hi) = (start.to_bits(), end.to_bits());
                if nearest_code(decode, f32::from_bits(hi - 1)) == code {
                    return (code, f32::INFINITY);
                }
                while lo + 1 < hi {
                    let mid = lo + (hi - lo) / 2;
                    if nearest_code(decode, f32::from_bits(mid)) == code {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                (code, f32::from_bits(hi))
            })
            .collect();
        Self { steps }
    }

    fn encode(&self, v: f32) -> u8 {
        let v = v.clamp(0.0, 1.0);
        let (code, next) = self.steps[(v * Self::STEPS as f32) as usize];
        code + u8::from(v >= next)
    }
}

/// The sRGB code whose linear value (`decode`) is closest to `v` in [0, 1], ties to the
/// higher: from the largest code at or below `v`, up while the next is as close.
fn nearest_code(decode: &[f32; 256], v: f32) -> u8 {
    let mut code = decode.partition_point(|&d| d <= v).saturating_sub(1);
    while code < 255 && (decode[code + 1] - v).abs() <= (v - decode[code]).abs() {
        code += 1;
    }
    code as u8
}

/// Modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    let y = x * x / 4.0;
    for k in 1..50 {
        term *= y / (k * k) as f64;
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

/// Kaiser-windowed sinc, half-width `WIDTH` (in destination texels).
fn kaiser(x: f64) -> f64 {
    const WIDTH: f64 = 3.0;
    const ALPHA: f64 = 4.0;
    if x.abs() >= WIDTH {
        return 0.0;
    }
    let sinc = if x.abs() < 1e-9 {
        1.0
    } else {
        let px = core::f64::consts::PI * x;
        px.sin() / px
    };
    let r = x / WIDTH;
    sinc * bessel_i0(ALPHA * (1.0 - r * r).sqrt()) / bessel_i0(ALPHA)
}

/// Taps of each destination texel when resampling `source` texels to `target`: the first
/// source texel and its weights (normalized).
fn taps(source: u32, target: u32) -> Vec<(usize, Vec<f32>)> {
    const WIDTH: f64 = 3.0;
    let scale = f64::from(source) / f64::from(target);
    (0..target)
        .map(|d| {
            let center = (f64::from(d) + 0.5) * scale - 0.5;
            let radius = WIDTH * scale;
            let first = (center - radius).ceil() as i64;
            let last = (center + radius).floor() as i64;
            // Taps outside the image clamp to its edge: fold their weights onto it.
            let edge = i64::from(source) - 1;
            let lo = first.clamp(0, edge);
            let hi = last.clamp(0, edge);
            let mut weights = vec![0.0f64; (hi - lo + 1) as usize];
            for i in first..=last {
                let w = kaiser((i as f64 - center) / scale);
                weights[(i.clamp(0, edge) - lo) as usize] += w;
            }
            let sum: f64 = weights.iter().sum();
            let weights = weights.iter().map(|&w| (w / sum) as f32).collect();
            (lo as usize, weights)
        })
        .collect()
}

/// Resamples a `width × height` image of 4-channel floats to `target_w × target_h`,
/// separably: each row across, then each target row as the weighted sum of the rows it
/// takes, a whole row at a time (in the order of the taps: the sums of a texel at a time,
/// to the bit).
fn resample(
    src: &[[f32; 4]],
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
) -> Vec<[f32; 4]> {
    let horizontal = taps(width, target_w);
    let vertical = taps(height, target_h);
    let (width, target_w) = (width as usize, target_w as usize);
    let mut rows = vec![Vec4::ZERO; target_w * height as usize];
    for (line, row) in src.chunks_exact(width).zip(rows.chunks_exact_mut(target_w)) {
        for (texel, (first, weights)) in row.iter_mut().zip(&horizontal) {
            let mut sum = Vec4::ZERO;
            for (p, &w) in line[*first..*first + weights.len()].iter().zip(weights) {
                sum += Vec4::from_array(*p) * w;
            }
            *texel = sum;
        }
    }
    let mut out = vec![[0.0f32; 4]; target_w * target_h as usize];
    let mut sums = vec![Vec4::ZERO; target_w];
    for (target, (first, weights)) in out.chunks_exact_mut(target_w).zip(&vertical) {
        sums.fill(Vec4::ZERO);
        for (k, &w) in weights.iter().enumerate() {
            let row = &rows[(first + k) * target_w..(first + k + 1) * target_w];
            for (sum, &p) in sums.iter_mut().zip(row) {
                *sum += p * w;
            }
        }
        for (texel, sum) in target.iter_mut().zip(&sums) {
            *texel = sum.to_array();
        }
    }
    out
}

/// Share of texels whose alpha (`[3]`) times `scale` is at least `cutoff`.
fn coverage(texels: &[[f32; 4]], cutoff: f32, scale: f32) -> f32 {
    let above = texels.iter().filter(|t| t[3] * scale >= cutoff).count();
    above as f32 / texels.len().max(1) as f32
}

/// The alpha scale that brings a level's coverage closest to `target` (binary search).
fn coverage_scale(texels: &[[f32; 4]], cutoff: f32, target: f32) -> f32 {
    let (mut lo, mut hi) = (0.0f32, 4.0f32);
    for _ in 0..24 {
        let mid = (lo + hi) * 0.5;
        if coverage(texels, cutoff, mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) * 0.5
}

/// The full mip chain of `image` for `usage`.
pub fn build_mips(image: &Image, usage: TextureUsage) -> TextureImage {
    engine_profiler::zone!("assets::build_mips");
    let decode = srgb_to_linear_table();
    let encode = LinearToSrgb::get();
    let (width, height) = (image.width.max(1), image.height.max(1));

    // Working texels: linear, premultiplied color; normals as vectors.
    let mut level: Vec<[f32; 4]> = image
        .rgba
        .chunks_exact(4)
        .map(|p| {
            let unit = |v: u8| f32::from(v) / 255.0;
            match usage {
                TextureUsage::Color | TextureUsage::ColorMasked { .. } => {
                    let a = unit(p[3]);
                    [
                        decode[p[0] as usize] * a,
                        decode[p[1] as usize] * a,
                        decode[p[2] as usize] * a,
                        a,
                    ]
                }
                TextureUsage::Data => [unit(p[0]), unit(p[1]), unit(p[2]), unit(p[3])],
                TextureUsage::Normal => [
                    unit(p[0]) * 2.0 - 1.0,
                    unit(p[1]) * 2.0 - 1.0,
                    unit(p[2]) * 2.0 - 1.0,
                    1.0,
                ],
            }
        })
        .collect();
    let target_coverage = match usage {
        TextureUsage::ColorMasked { cutoff } => Some((cutoff, coverage(&level, cutoff, 1.0))),
        _ => None,
    };

    let store = |texels: &[[f32; 4]], alpha_scale: f32| -> Vec<u8> {
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        texels
            .iter()
            .flat_map(|t| match usage {
                TextureUsage::Color | TextureUsage::ColorMasked { .. } => {
                    let a = t[3].clamp(0.0, 1.0);
                    let un = |c: f32| if a > 1e-6 { c / a } else { 0.0 };
                    [
                        encode.encode(un(t[0])),
                        encode.encode(un(t[1])),
                        encode.encode(un(t[2])),
                        byte(a * alpha_scale),
                    ]
                }
                TextureUsage::Data => [byte(t[0]), byte(t[1]), byte(t[2]), byte(t[3])],
                TextureUsage::Normal => {
                    let len = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
                    let n = if len > 1e-6 {
                        [t[0] / len, t[1] / len, t[2] / len]
                    } else {
                        [0.0, 0.0, 1.0]
                    };
                    [
                        byte(n[0] * 0.5 + 0.5),
                        byte(n[1] * 0.5 + 0.5),
                        byte(n[2] * 0.5 + 0.5),
                        255,
                    ]
                }
            })
            .collect()
    };

    // Level 0 is the source itself.
    let mut levels = vec![image.rgba.clone()];
    let (mut w, mut h) = (width, height);
    while w > 1 || h > 1 {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        level = resample(&level, w, h, nw, nh);
        let scale = match target_coverage {
            Some((cutoff, target)) => {
                // Coverage of the filtered alpha, as the shader will test it.
                let straight: Vec<[f32; 4]> = level
                    .iter()
                    .map(|t| [0.0, 0.0, 0.0, t[3].clamp(0.0, 1.0)])
                    .collect();
                coverage_scale(&straight, cutoff, target)
            }
            None => 1.0,
        };
        levels.push(store(&level, scale));
        (w, h) = (nw, nh);
    }
    TextureImage {
        width,
        height,
        srgb: usage.srgb(),
        levels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resampling as it was first written, a texel at a time: the reference the faster
    /// one matches to the bit.
    fn resample_texel_by_texel(
        src: &[[f32; 4]],
        width: u32,
        height: u32,
        target_w: u32,
        target_h: u32,
    ) -> Vec<[f32; 4]> {
        let horizontal = taps(width, target_w);
        let vertical = taps(height, target_h);
        let mut rows = vec![[0.0f32; 4]; (target_w * height) as usize];
        for y in 0..height as usize {
            let line = &src[y * width as usize..(y + 1) * width as usize];
            for (x, (first, weights)) in horizontal.iter().enumerate() {
                let mut sum = [0.0f32; 4];
                for (k, &w) in weights.iter().enumerate() {
                    let p = line[first + k];
                    for c in 0..4 {
                        sum[c] += p[c] * w;
                    }
                }
                rows[y * target_w as usize + x] = sum;
            }
        }
        let mut out = vec![[0.0f32; 4]; (target_w * target_h) as usize];
        for (y, (first, weights)) in vertical.iter().enumerate() {
            for x in 0..target_w as usize {
                let mut sum = [0.0f32; 4];
                for (k, &w) in weights.iter().enumerate() {
                    let p = rows[(first + k) * target_w as usize + x];
                    for c in 0..4 {
                        sum[c] += p[c] * w;
                    }
                }
                out[y * target_w as usize + x] = sum;
            }
        }
        out
    }

    #[test]
    fn encoding_by_steps_finds_the_codes_the_walk_found() {
        let decode = srgb_to_linear_table();
        let encode = LinearToSrgb::get();
        // The walk as first written: from the largest code at or below each step's start.
        let coarse: Vec<u8> = (0..=LinearToSrgb::STEPS)
            .map(|i| {
                let v = i as f32 / LinearToSrgb::STEPS as f32;
                decode.partition_point(|&d| d <= v).saturating_sub(1) as u8
            })
            .collect();
        let walk = |v: f32| {
            let v = v.clamp(0.0, 1.0);
            let mut code = coarse[(v * LinearToSrgb::STEPS as f32) as usize] as usize;
            while code < 255 && (decode[code + 1] - v).abs() <= (v - decode[code]).abs() {
                code += 1;
            }
            code as u8
        };
        let steps = 1 << 20;
        for i in 0..=steps {
            let v = i as f32 / steps as f32;
            assert_eq!(encode.encode(v), walk(v), "{v}");
        }
        // The codes' own values and the midpoints between them, and outside [0, 1].
        for c in 0..255 {
            for v in [decode[c], (decode[c] + decode[c + 1]) * 0.5, decode[c + 1]] {
                for v in [
                    v,
                    f32::from_bits(v.to_bits() + 1),
                    f32::from_bits(v.to_bits().max(1) - 1),
                ] {
                    assert_eq!(encode.encode(v), walk(v), "{v}");
                }
            }
        }
        for v in [-1.0, 2.0, f32::NAN] {
            assert_eq!(encode.encode(v), walk(v), "{v}");
        }
    }

    /// Every float from 0 to 1 encodes as the nearest code (about a billion, a minute in
    /// release; run by hand when the table changes).
    #[test]
    #[ignore = "exhaustive, by hand"]
    fn every_value_in_zero_to_one_encodes_as_the_nearest_code() {
        let decode = srgb_to_linear_table();
        let encode = LinearToSrgb::get();
        let mut code = 0usize;
        for bits in 0..=1.0f32.to_bits() {
            let v = f32::from_bits(bits);
            // The nearest code only grows with v.
            while code < 255 && (decode[code + 1] - v).abs() <= (v - decode[code]).abs() {
                code += 1;
            }
            assert_eq!(encode.encode(v), code as u8, "{v}");
        }
    }

    #[test]
    fn resampling_a_row_at_a_time_matches_a_texel_at_a_time_to_the_bit() {
        // Noise from a small generator, signed (normal maps) and above one (premultiplied
        // sums), over sizes even, odd and thin.
        let mut state = 0x9e37_79b9_u32;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f32 / u32::MAX as f32) * 2.5 - 1.0
        };
        for (w, h) in [(64, 64), (37, 21), (1, 9), (16, 1), (130, 66)] {
            let src: Vec<[f32; 4]> = (0..w * h)
                .map(|_| [next(), next(), next(), next()])
                .collect();
            let (tw, th) = ((w / 2).max(1), (h / 2).max(1));
            let fast = resample(&src, w, h, tw, th);
            let reference = resample_texel_by_texel(&src, w, h, tw, th);
            let bits =
                |v: &[[f32; 4]]| -> Vec<u32> { v.iter().flatten().map(|f| f.to_bits()).collect() };
            assert_eq!(bits(&fast), bits(&reference), "{w}×{h}");
        }
    }

    fn image(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Image {
        let rgba = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .flat_map(|(x, y)| f(x, y))
            .collect();
        Image {
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn levels_halve_down_to_one_texel() {
        let mips = build_mips(&image(5, 3, |_, _| [9, 9, 9, 255]), TextureUsage::Data);
        let sizes: Vec<usize> = mips.levels.iter().map(Vec::len).collect();
        // 5×3, 2×1, 1×1.
        assert_eq!(sizes, [5 * 3 * 4, 2 * 4, 4]);
        for level in &mips.levels {
            assert!(
                level.chunks_exact(4).all(|p| p == [9, 9, 9, 255]),
                "uniform stays uniform"
            );
        }
    }

    #[test]
    fn color_is_averaged_in_linear_light() {
        // Black and white stripes one texel wide: the average light is 0.5, which is sRGB
        // code 188, not the 128 of averaging codes.
        let mips = build_mips(
            &image(64, 64, |x, _| {
                let v = if x % 2 == 0 { 0 } else { 255 };
                [v, v, v, 255]
            }),
            TextureUsage::Color,
        );
        let l = &mips.levels[3];
        let mean = l.chunks_exact(4).map(|p| f32::from(p[0])).sum::<f32>() / (l.len() / 4) as f32;
        assert!((mean - 188.0).abs() <= 1.5, "{mean}");
    }

    #[test]
    fn transparent_texels_do_not_darken_edges() {
        // Opaque red next to transparent black: the filtered red stays red.
        let mips = build_mips(
            &image(64, 64, |x, _| {
                if x < 32 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 0, 0]
                }
            }),
            TextureUsage::Color,
        );
        for level in &mips.levels[1..] {
            for p in level.chunks_exact(4) {
                if p[3] > 8 {
                    assert!(p[0] >= 250 && p[1] <= 2 && p[2] <= 2, "{p:?}");
                }
            }
        }
    }

    #[test]
    fn masks_keep_their_coverage() {
        // Thin leaves: 30% of texels opaque in scattered one-texel dots.
        let img = image(256, 256, |x, y| {
            let h = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) % 100;
            if h < 30 {
                [40, 160, 40, 255]
            } else {
                [40, 160, 40, 0]
            }
        });
        let cutoff = 0.5;
        let kept = build_mips(&img, TextureUsage::ColorMasked { cutoff });
        let plain = build_mips(&img, TextureUsage::Color);
        let share = |level: &[u8]| {
            let n = level
                .chunks_exact(4)
                .filter(|p| f32::from(p[3]) / 255.0 >= cutoff)
                .count();
            n as f32 / (level.len() / 4) as f32
        };
        for k in 1..6 {
            let with = share(&kept.levels[k]);
            assert!((with - 0.3).abs() < 0.03, "level {k}: coverage {with}");
        }
        // Without it the dots vanish (average alpha 0.3 < cutoff).
        assert!(share(&plain.levels[3]) < 0.05);
    }

    #[test]
    fn normal_maps_stay_unit_length() {
        let img = image(32, 32, |x, y| {
            // Tilted normals alternating left and right.
            let tilt: u8 = if (x + y) % 2 == 0 { 64 } else { 192 };
            [tilt, 128, 222, 255]
        });
        let mips = build_mips(&img, TextureUsage::Normal);
        for level in &mips.levels[1..] {
            for p in level.chunks_exact(4) {
                let n = [p[0], p[1], p[2]].map(|c| f32::from(c) / 255.0 * 2.0 - 1.0);
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                assert!((len - 1.0).abs() < 0.02, "{len}");
                // The tilts cancel: the average points straight out.
                assert!(n[0].abs() < 0.05, "{n:?}");
            }
        }
    }

    #[test]
    fn linear_to_srgb_is_exact() {
        let decode = srgb_to_linear_table();
        let encode = LinearToSrgb::get();
        for code in 0..=255u8 {
            assert_eq!(encode.encode(decode[code as usize]), code);
        }
        // Halfway between two codes rounds to the nearer one.
        let v = (decode[100] * 0.3 + decode[101] * 0.7).min(1.0);
        assert_eq!(encode.encode(v), 101);
    }

    #[test]
    fn decodes_png() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 3, 2);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 1, 2, 3, 4, 5, 6, 7, 8, 9])
                .unwrap();
        }
        let img = decode_image(&bytes).unwrap();
        assert_eq!((img.width, img.height), (3, 2));
        assert_eq!(&img.rgba[..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(&img.rgba[20..24], &[7, 8, 9, 255]);
        assert_eq!(decode_image(b"GIF89a"), Err(ImageError::UnknownFormat));
    }
}
