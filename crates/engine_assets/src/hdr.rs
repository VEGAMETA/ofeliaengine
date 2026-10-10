//! HDR panoramas (ADR 0083): Radiance `.hdr` files (RGBE), the format most HDRI
//! environments come in, by our own decoder and encoder; OpenEXR files through the `exr`
//! crate ([`decode_exr`]; Poly Haven's and Blender's are PIZ compressed), turned into RGBE;
//! and the search for the sun in a panorama, so that a directional light can replace it
//! (its shadows, and a lighting cube without its spike).
//!
//! - **Header:** a `#?` signature line (`#?RADIANCE`, `#?RGBE`), lines of `KEY=value`
//!   (`FORMAT=32-bit_rle_rgbe`; `EXPOSURE`, `GAMMA` and the rest are read past), an empty
//!   line, then the resolution `-Y <height> +X <width>`: rows top to bottom, pixels left to
//!   right, the only orientation in practice (the others are refused).
//! - **Scanlines:** four bytes per pixel (red, green and blue mantissas, a shared
//!   exponent), each line flat or, for widths 8 to 32,767, run-length encoded channel by
//!   channel (a line starting with `2, 2` and its width). Radiance's older run encoding
//!   (repeat markers among flat pixels) is not read, as in most readers.
//!
//! Pixels stay RGBE, 4 bytes each: the renderer decodes them on the GPU, keeping their full
//! range (an unclipped sun exceeds half floats' 65,504).
//!
//! **Directions** are the engine's (+Y up, the default camera looking along −Z): the
//! panorama's middle column faces −Z, three quarters of its width +X, its top row +Y
//! ([`panorama_uv`]).
//!
//! No code was copied: the format is Greg Ward's ("Real Pixels", Graphics Gems II, 1991,
//! and Radiance's file format documentation). See docs/research/hdri-environments.md.

use engine_core::math::Vec3;

/// A Radiance HDR image: RGBE pixels, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HdrImage {
    /// Pixels per row.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width × height` pixels, row after row: red, green and blue mantissas and a shared
    /// exponent ([`rgbe_to_linear`]).
    pub pixels: Vec<[u8; 4]>,
}

impl HdrImage {
    /// The linear radiance of pixel `(x, y)`.
    ///
    /// # Panics
    ///
    /// If the pixel is outside the image.
    pub fn radiance(&self, x: u32, y: u32) -> Vec3 {
        assert!(x < self.width && y < self.height, "pixel outside the image");
        let p = self.pixels[y as usize * self.width as usize + x as usize];
        Vec3::from_array(rgbe_to_linear(p))
    }
}

/// Why a panorama could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HdrError {
    /// No `#?` signature line ([`decode_panorama`]: neither a Radiance nor an OpenEXR
    /// signature).
    Signature,
    /// A pixel format other than `32-bit_rle_rgbe`.
    Format(String),
    /// A resolution line other than `-Y height +X width`, or an empty or oversized image
    /// (above 2^28 pixels).
    Resolution(String),
    /// The file ends inside the header or the pixels.
    Truncated,
    /// A run-length encoded scanline is malformed (its row).
    Scanline(u32),
    /// The OpenEXR decoder failed, or the file has no layer with red, green and blue
    /// channels.
    Exr(String),
}

impl core::fmt::Display for HdrError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Signature => write!(f, "not a Radiance HDR file (no `#?` signature)"),
            Self::Format(format) => {
                write!(
                    f,
                    "unsupported pixel format `{format}` (only 32-bit_rle_rgbe)"
                )
            }
            Self::Resolution(line) => write!(
                f,
                "unsupported resolution `{line}` (only `-Y height +X width`, up to 2^28 pixels)"
            ),
            Self::Truncated => write!(f, "the file ends early"),
            Self::Scanline(row) => write!(f, "malformed run-length encoded scanline (row {row})"),
            Self::Exr(e) => write!(f, "OpenEXR decoding failed: {e}"),
        }
    }
}

impl std::error::Error for HdrError {}

/// Largest image read: 2^28 pixels (1 GiB of RGBE, a 23,170-pixel square).
const MAX_PIXELS: u64 = 1 << 28;

/// Decodes a Radiance HDR file.
///
/// # Errors
///
/// If the bytes are not an RGBE Radiance file this decoder reads ([`HdrError`]).
pub fn decode_hdr(bytes: &[u8]) -> Result<HdrImage, HdrError> {
    engine_profiler::zone!("assets::decode_hdr");
    if !bytes.starts_with(b"#?") {
        return Err(HdrError::Signature);
    }
    let mut at = 0;
    let mut line = || -> Result<&[u8], HdrError> {
        let rest = &bytes[at..];
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or(HdrError::Truncated)?;
        at += end + 1;
        let l = &rest[..end];
        Ok(l.strip_suffix(b"\r").unwrap_or(l))
    };
    if !line()?.starts_with(b"#?") {
        return Err(HdrError::Signature);
    }
    loop {
        let l = line()?;
        if l.is_empty() {
            break;
        }
        if let Some(format) = l.strip_prefix(b"FORMAT=") {
            let format = String::from_utf8_lossy(format).trim().to_string();
            if format != "32-bit_rle_rgbe" {
                return Err(HdrError::Format(format));
            }
        }
    }
    let resolution = String::from_utf8_lossy(line()?).trim().to_string();
    let bad = || HdrError::Resolution(resolution.clone());
    let parts: Vec<&str> = resolution.split_whitespace().collect();
    let (height, width) = match parts.as_slice() {
        ["-Y", h, "+X", w] => (
            h.parse::<u32>().map_err(|_| bad())?,
            w.parse::<u32>().map_err(|_| bad())?,
        ),
        _ => return Err(bad()),
    };
    let count = u64::from(width) * u64::from(height);
    if count == 0 || count > MAX_PIXELS {
        return Err(bad());
    }
    let data = &bytes[at..];
    // The densest run-length encoding takes 2 bytes per 127 pixels of each channel: a file
    // shorter than that is truncated (and no memory is reserved for what it claims).
    if (data.len() as u64).saturating_mul(16) < count {
        return Err(HdrError::Truncated);
    }
    let mut pixels = vec![[0u8; 4]; count as usize];
    let mut pos = 0;
    for (row, line) in pixels.chunks_exact_mut(width as usize).enumerate() {
        read_scanline(data, &mut pos, line, row as u32)?;
    }
    Ok(HdrImage {
        width,
        height,
        pixels,
    })
}

/// The first bytes of an OpenEXR file.
const EXR_MAGIC: [u8; 4] = [0x76, 0x2f, 0x31, 0x01];

/// Decodes a panorama: a Radiance HDR file ([`decode_hdr`]) or an OpenEXR file
/// ([`decode_exr`]), told apart by their signatures.
///
/// # Errors
///
/// If the bytes are neither, or cannot be decoded ([`HdrError`]).
pub fn decode_panorama(bytes: &[u8]) -> Result<HdrImage, HdrError> {
    if bytes.starts_with(&EXR_MAGIC) {
        decode_exr(bytes)
    } else {
        decode_hdr(bytes)
    }
}

/// Decodes an OpenEXR file: the first layer with red, green and blue channels (any pixel
/// type, any compression the `exr` crate reads), at its largest resolution, rows top to
/// bottom, its pixels turned into RGBE ([`linear_to_rgbe`]).
///
/// # Errors
///
/// If the decoder fails, no layer has the three channels, or the image is empty or above
/// 2^28 pixels.
pub fn decode_exr(bytes: &[u8]) -> Result<HdrImage, HdrError> {
    use exr::prelude::{ReadChannels, ReadLayers, ReadSpecificChannel, Vec2, read};
    engine_profiler::zone!("assets::decode_exr");
    let failed = |e: exr::error::Error| HdrError::Exr(e.to_string());
    // The header first: no memory is reserved for what a damaged one claims.
    let meta = exr::meta::MetaData::read_from_buffered(std::io::Cursor::new(bytes), false)
        .map_err(failed)?;
    for header in &meta.headers {
        let count = header.layer_size.area() as u64;
        if count == 0 || count > MAX_PIXELS {
            let size = header.layer_size;
            return Err(HdrError::Resolution(format!(
                "{} × {}",
                size.width(),
                size.height()
            )));
        }
    }
    let image = read()
        .no_deep_data()
        .largest_resolution_level()
        .specific_channels()
        .required("R")
        .required("G")
        .required("B")
        .collect_pixels(
            |size: Vec2<usize>, _| (size, vec![[0u8; 4]; size.area()]),
            |(size, pixels): &mut (Vec2<usize>, Vec<[u8; 4]>),
             at: Vec2<usize>,
             (r, g, b): (f32, f32, f32)| {
                pixels[at.y() * size.width() + at.x()] = linear_to_rgbe([r, g, b]);
            },
        )
        .first_valid_layer()
        .all_attributes()
        .from_buffered(std::io::Cursor::new(bytes))
        .map_err(failed)?;
    let (size, pixels) = image.layer_data.channel_data.pixels;
    Ok(HdrImage {
        width: size.width() as u32,
        height: size.height() as u32,
        pixels,
    })
}

/// Whether a line of `width` pixels may be run-length encoded.
fn rle_width(width: usize) -> bool {
    (8..32768).contains(&width)
}

/// One scanline into `line`: run-length encoded (`2, 2, width`) or flat.
fn read_scanline(
    data: &[u8],
    pos: &mut usize,
    line: &mut [[u8; 4]],
    row: u32,
) -> Result<(), HdrError> {
    let width = line.len();
    let take = |pos: &mut usize, n: usize| -> Result<&[u8], HdrError> {
        let slice = data.get(*pos..*pos + n).ok_or(HdrError::Truncated)?;
        *pos += n;
        Ok(slice)
    };
    let head = data.get(*pos..*pos + 4).ok_or(HdrError::Truncated)?;
    let encoded = rle_width(width)
        && head[0] == 2
        && head[1] == 2
        && head[2] & 0x80 == 0
        && (usize::from(head[2]) << 8 | usize::from(head[3])) == width;
    if !encoded {
        let flat = take(pos, 4 * width)?;
        for (pixel, bytes) in line.iter_mut().zip(flat.chunks_exact(4)) {
            pixel.copy_from_slice(bytes);
        }
        return Ok(());
    }
    *pos += 4;
    // Each channel in turn: runs (a count above 128, then the value) and literals (a count
    // up to 128, then the values).
    for channel in 0..4 {
        let mut x = 0;
        while x < width {
            let count = usize::from(take(pos, 1)?[0]);
            if count > 128 {
                let n = count - 128;
                let value = take(pos, 1)?[0];
                if x + n > width {
                    return Err(HdrError::Scanline(row));
                }
                for pixel in &mut line[x..x + n] {
                    pixel[channel] = value;
                }
                x += n;
            } else {
                if count == 0 || x + count > width {
                    return Err(HdrError::Scanline(row));
                }
                let literal = take(pos, count)?;
                for (pixel, &value) in line[x..x + count].iter_mut().zip(literal) {
                    pixel[channel] = value;
                }
                x += count;
            }
        }
    }
    Ok(())
}

/// Encodes a Radiance HDR file: scanlines run-length encoded where the format allows
/// (widths 8 to 32,767), flat otherwise.
///
/// # Panics
///
/// If `image.pixels` does not hold `width × height` pixels.
pub fn encode_hdr(image: &HdrImage) -> Vec<u8> {
    let (width, height) = (image.width as usize, image.height as usize);
    assert_eq!(image.pixels.len(), width * height, "pixel count");
    let mut out = format!(
        "#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y {} +X {}\n",
        image.height, image.width
    )
    .into_bytes();
    if !rle_width(width) {
        out.extend(image.pixels.iter().flatten());
        return out;
    }
    let mut channel = vec![0u8; width];
    for line in image.pixels.chunks_exact(width) {
        out.extend([2, 2, (width >> 8) as u8, width as u8]);
        for c in 0..4 {
            for (value, pixel) in channel.iter_mut().zip(line) {
                *value = pixel[c];
            }
            encode_channel(&channel, &mut out);
        }
    }
    out
}

/// One channel of a scanline: runs of 3 or more equal bytes (up to 127) as a count above
/// 128 and the value, everything else as literals (up to 128).
fn encode_channel(values: &[u8], out: &mut Vec<u8>) {
    let run_at = |i: usize| {
        let v = values[i];
        values[i..]
            .iter()
            .take(127)
            .take_while(|&&x| x == v)
            .count()
    };
    let mut i = 0;
    while i < values.len() {
        let run = run_at(i);
        if run >= 3 {
            out.extend([128 + run as u8, values[i]]);
            i += run;
            continue;
        }
        // Literals up to the next run of 3 (or 128 of them).
        let start = i;
        while i < values.len() && i - start < 128 && run_at(i) < 3 {
            i += 1;
        }
        out.push((i - start) as u8);
        out.extend_from_slice(&values[start..i]);
    }
}

/// A shared-exponent pixel as linear RGB: the mantissas' centres, `(m + 0.5) / 256 × 2^(e − 128)`
/// (0 for exponent 0).
pub fn rgbe_to_linear(p: [u8; 4]) -> [f32; 3] {
    if p[3] == 0 {
        return [0.0; 3];
    }
    let scale = f32::powi(2.0, i32::from(p[3]) - 136);
    [
        (f32::from(p[0]) + 0.5) * scale,
        (f32::from(p[1]) + 0.5) * scale,
        (f32::from(p[2]) + 0.5) * scale,
    ]
}

/// Linear RGB as a shared-exponent pixel (negative values as 0, values beyond the format's
/// range saturated): the inverse of [`rgbe_to_linear`] within half a step of the largest
/// channel's mantissa.
pub fn linear_to_rgbe(c: [f32; 3]) -> [u8; 4] {
    let c = c.map(|v| if v > 0.0 { v } else { 0.0 });
    let max = c[0].max(c[1]).max(c[2]);
    // 2^e with max / 2^e in [0.5, 1): the exponent field of a normal float.
    if max < f32::MIN_POSITIVE {
        return [0; 4];
    }
    let e = ((max.to_bits() >> 23) & 0xFF) as i32 - 126;
    if e < -127 {
        return [0; 4];
    }
    if e > 127 {
        return [255, 255, 255, 255];
    }
    // In doubles: 2^(8 − e) exceeds floats for the smallest exponents.
    let scale = f64::powi(2.0, 8 - e);
    let m = c.map(|v| (f64::from(v) * scale).min(255.0) as u8);
    [m[0], m[1], m[2], (e + 128) as u8]
}

/// Where direction `d` falls on a panorama, `[u, v]` in 0..1: `u = 0.5 + atan2(x, −z) / 2π`
/// (the middle faces −Z, three quarters +X), `v = acos(y) / π` (the top row +Y).
pub fn panorama_uv(d: Vec3) -> [f32; 2] {
    let d = d.normalize_or(Vec3::NEG_Z);
    [
        0.5 + d.x.atan2(-d.z) / core::f32::consts::TAU,
        d.y.clamp(-1.0, 1.0).acos() / core::f32::consts::PI,
    ]
}

/// The direction through point `[u, v]` of a panorama: the inverse of [`panorama_uv`].
pub fn panorama_direction(uv: [f32; 2]) -> Vec3 {
    let phi = (uv[0] - 0.5) * core::f32::consts::TAU;
    let theta = uv[1] * core::f32::consts::PI;
    let (sin_theta, cos_theta) = theta.sin_cos();
    Vec3::new(sin_theta * phi.sin(), cos_theta, -sin_theta * phi.cos())
}

/// The sun of a panorama ([`find_sun`]): the light a directional light takes over, and the
/// sky to put where it was.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanoramaSun {
    /// Unit direction towards it (the mean of its pixels' directions, weighted by their
    /// light), in the panorama's frame ([`panorama_uv`]).
    pub direction: Vec3,
    /// Its illuminance at normal incidence, linear RGB in the panorama's units: the light
    /// above the sky around it, `∫ max(L − sky, 0) dΩ` over its cone. A directional light
    /// of this color replaces it.
    pub illuminance: Vec3,
    /// Angular radius of its cone, in radians: the disk and its glare, out to a pixel past
    /// where the light falls below [`SunSearch::falloff`] of the peak.
    pub cone: f32,
    /// The sky's radiance just outside the cone (the mean over a ring out to twice its
    /// radius): what a lighting cube puts inside it.
    pub sky: Vec3,
}

/// How [`find_sun`] tells a sun from the sky.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunSearch {
    /// The brightest pixel is a sun if its luminance is this many times the sky's around
    /// it (the median 10° to 20° away). Default 256: below that, it is a bright patch the
    /// lighting cube's filtering handles.
    pub min_contrast: f32,
    /// The cone ends where a ring of pixels around the peak averages below this fraction
    /// of its luminance. Default 1/1024.
    pub falloff: f32,
    /// The largest cone, in radians. Default 10°.
    pub max_cone: f32,
}

impl Default for SunSearch {
    fn default() -> Self {
        Self {
            min_contrast: 256.0,
            falloff: 1.0 / 1024.0,
            max_cone: 10f32.to_radians(),
        }
    }
}

/// Finds the sun of a panorama (ADR 0083): the brightest pixel, if it outshines the sky
/// around it by [`SunSearch::min_contrast`]; its cone out to where rings of pixels around
/// it fall below [`SunSearch::falloff`] of its light; and the light within the cone above
/// the sky just outside it. `None` for skies without a sun (overcast, or a clipped one).
pub fn find_sun(image: &HdrImage, search: &SunSearch) -> Option<PanoramaSun> {
    engine_profiler::zone!("assets::find_sun");
    let (width, height) = (image.width as usize, image.height as usize);
    if width == 0 || height == 0 || image.pixels.len() != width * height {
        return None;
    }
    let scale: [f32; 256] = core::array::from_fn(|e| {
        if e == 0 {
            0.0
        } else {
            f32::powi(2.0, e as i32 - 136)
        }
    });
    let linear = |p: [u8; 4]| {
        let s = scale[usize::from(p[3])];
        Vec3::new(
            (f32::from(p[0]) + 0.5) * s,
            (f32::from(p[1]) + 0.5) * s,
            (f32::from(p[2]) + 0.5) * s,
        )
    };
    let luminance = |c: Vec3| c.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    let (peak_index, peak) = image
        .pixels
        .iter()
        .map(|&p| luminance(linear(p)))
        .enumerate()
        .fold(
            (0, 0.0f32),
            |best, (i, y)| if y > best.1 { (i, y) } else { best },
        );
    if peak <= 0.0 {
        return None;
    }
    // Rows' polar angles and columns' azimuths at pixel centres; a pixel's solid angle is
    // (2π / width)(π / height) sin θ.
    let step = core::f32::consts::PI / height as f32;
    let pixel_area = core::f32::consts::TAU / width as f32 * step;
    let columns: Vec<(f32, f32)> = (0..width)
        .map(|x| (((x as f32 + 0.5) / width as f32 - 0.5) * core::f32::consts::TAU).sin_cos())
        .collect();
    let row_of = |y: usize| ((y as f32 + 0.5) * step).sin_cos();
    let direction = |(sin_theta, cos_theta): (f32, f32), (sin_phi, cos_phi): (f32, f32)| {
        Vec3::new(sin_theta * sin_phi, cos_theta, -sin_theta * cos_phi)
    };
    let (peak_x, peak_y) = (peak_index % width, peak_index / width);
    let toward = direction(row_of(peak_y), columns[peak_x]);
    // Rows within `reach` of the peak (every column: near a pole a ring spans them all).
    let reach = search.max_cone.max(10f32.to_radians()) * 2.0;
    let first = ((peak_y as f32 + 0.5) - reach / step).floor().max(0.0) as usize;
    let last = (((peak_y as f32 + 0.5) + reach / step).ceil() as usize).min(height - 1);
    let min_cos = reach.cos();
    let visit = |f: &mut dyn FnMut(f32, Vec3, Vec3, f32)| {
        for y in first..=last {
            let row = row_of(y);
            let area = pixel_area * row.0;
            for (x, &column) in columns.iter().enumerate() {
                let d = direction(row, column);
                if d.dot(toward) < min_cos {
                    continue;
                }
                // The angle from the peak (exact for small ones, where it matters).
                let angle = 2.0 * ((d - toward).length() * 0.5).min(1.0).asin();
                f(angle, d, linear(image.pixels[y * width + x]), area);
            }
        }
    };
    // The sky around (the median luminance 10° to 20° away), and rings one pixel wide
    // around the peak (light and solid angle).
    let (near, far) = (10f32.to_radians(), 20f32.to_radians());
    let bins = (search.max_cone / step).ceil().max(1.0) as usize;
    let mut rings = vec![(0.0f32, 0.0f32); bins];
    let mut around = Vec::new();
    visit(&mut |angle, _, l, area| {
        if (near..far).contains(&angle) {
            around.push(luminance(l));
        }
        let bin = (angle / step) as usize;
        if bin < bins {
            rings[bin].0 += luminance(l) * area;
            rings[bin].1 += area;
        }
    });
    if around.is_empty() {
        return None;
    }
    let middle = around.len() / 2;
    let sky_level = *around.select_nth_unstable_by(middle, f32::total_cmp).1;
    if peak < search.min_contrast * sky_level.max(1e-30) {
        return None;
    }
    // The cone ends past the first ring that averages below the falloff: that ring can
    // still hold pixels the sun covers in part (most of it sky), so it is taken too.
    let threshold = peak * search.falloff;
    let cone = rings
        .iter()
        .enumerate()
        .skip(1)
        .find(|&(_, &(light, area))| area > 0.0 && light < threshold * area)
        .map_or(search.max_cone, |(k, _)| {
            ((k + 1) as f32 * step).min(search.max_cone)
        });
    // The sky just outside the cone, then the light above it inside.
    let outer = (2.0 * cone).max(cone + 3.0 * step);
    let (mut sky, mut sky_area) = (Vec3::ZERO, 0.0);
    visit(&mut |angle, _, l, area| {
        if (cone..outer).contains(&angle) {
            sky += l * area;
            sky_area += area;
        }
    });
    let sky = if sky_area > 0.0 {
        sky / sky_area
    } else {
        Vec3::ZERO
    };
    let (mut illuminance, mut weighted) = (Vec3::ZERO, Vec3::ZERO);
    visit(&mut |angle, d, l, area| {
        if angle < cone {
            let excess = (l - sky).max(Vec3::ZERO) * area;
            illuminance += excess;
            weighted += d * luminance(excess);
        }
    });
    Some(PanoramaSun {
        direction: weighted.normalize_or(toward),
        illuminance,
        cone,
        sky,
    })
}
