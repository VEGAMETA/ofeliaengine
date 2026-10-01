//! Golden images: where they live, how a render is compared with one, and how they are
//! updated (ADR 0062).
//!
//! - One image per scene and GPU vendor: `tests/golden/images/<vendor>/<name>.png`
//!   (8-bit sRGB RGB). Drivers of one vendor render alike; vendors differ in rounding,
//!   texture filtering and BC decoding, so each has its own.
//! - `ENGINE_GOLDEN_UPDATE=1` writes the render as the new golden image instead of
//!   comparing (commit it with the change that altered the picture, after looking at it).
//! - Every comparison writes the render and FLIP's error map to `target/golden/<vendor>/`
//!   for inspection.
//! - A missing golden image passes with a note, unless `ENGINE_REQUIRE_GOLDEN=1`.

use crate::flip::{DEFAULT_PPD, FlipResult, flip, srgb_to_linear};
use crate::scene::{Rendered, vendor_name};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// How different a render may be from its golden image.
#[derive(Debug, Clone, Copy)]
pub struct Tolerance {
    /// Largest mean FLIP error.
    pub mean: f32,
    /// Largest FLIP error of the 99.9th percentile pixel: a small area that changed a lot.
    pub p999: f32,
}

impl Default for Tolerance {
    /// Same vendor, possibly another driver version: nothing visible.
    fn default() -> Self {
        Self {
            mean: 0.01,
            p999: 0.25,
        }
    }
}

/// What a comparison found.
#[derive(Debug)]
pub enum Outcome {
    /// Within the tolerance.
    Matched(FlipResult),
    /// Beyond it.
    Different(FlipResult),
    /// `ENGINE_GOLDEN_UPDATE=1`: written as the new golden image.
    Updated(PathBuf),
    /// No golden image for this vendor yet.
    Missing(PathBuf),
}

/// The golden image of `name` for `vendor`.
pub fn golden_path(vendor: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("images")
        .join(vendor)
        .join(format!("{name}.png"))
}

fn output_dir(vendor: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/golden")
        .join(vendor)
}

/// Writes 8-bit RGB (`rgb.len() == width × height × 3`) as a PNG.
///
/// # Errors
///
/// File or encoder errors.
pub fn write_png(path: &Path, width: u32, height: u32, rgb: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(path)?), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
    writer
        .write_image_data(rgb)
        .map_err(std::io::Error::other)?;
    writer.finish().map_err(std::io::Error::other)
}

/// Reads an 8-bit RGB or RGBA PNG as RGB: `(width, height, pixels)`.
///
/// # Errors
///
/// File or decoder errors, or another pixel format.
pub fn read_png(path: &Path) -> std::io::Result<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(std::io::BufReader::new(File::open(path)?));
    let mut reader = decoder.read_info().map_err(std::io::Error::other)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| std::io::Error::other("PNG too large"))?;
    let mut buffer = vec![0; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(std::io::Error::other)?;
    let pixels = &buffer[..info.buffer_size()];
    let rgb = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgb, png::BitDepth::Eight) => pixels.to_vec(),
        (png::ColorType::Rgba, png::BitDepth::Eight) => pixels
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        other => return Err(std::io::Error::other(format!("unsupported PNG {other:?}"))),
    };
    Ok((info.width, info.height, rgb))
}

/// 8-bit sRGB RGB to linear RGB for FLIP.
pub fn to_linear(rgb: &[u8]) -> Vec<[f32; 3]> {
    let table: Vec<f32> = (0..=255u8)
        .map(|v| srgb_to_linear(f32::from(v) / 255.0))
        .collect();
    rgb.chunks_exact(3)
        .map(|p| {
            [
                table[p[0] as usize],
                table[p[1] as usize],
                table[p[2] as usize],
            ]
        })
        .collect()
}

/// Compares `rendered` with its golden image `name` (or updates it, see the module).
///
/// # Panics
///
/// If the golden image cannot be read or written, or its size differs from the render's.
pub fn compare(rendered: &Rendered, name: &str, tolerance: Tolerance) -> Outcome {
    let vendor = vendor_name(rendered.vendor_id);
    let (w, h) = (rendered.width, rendered.height);
    let rgb: Vec<u8> = rendered
        .rgba8
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    let out = output_dir(&vendor);
    write_png(&out.join(format!("{name}.png")), w, h, &rgb).expect("write the render");
    let golden = golden_path(&vendor, name);
    if std::env::var("ENGINE_GOLDEN_UPDATE").is_ok_and(|v| v == "1") {
        write_png(&golden, w, h, &rgb).expect("write the golden image");
        return Outcome::Updated(golden);
    }
    if !golden.exists() {
        return Outcome::Missing(golden);
    }
    let (gw, gh, reference) = read_png(&golden).expect("read the golden image");
    assert_eq!(
        (gw, gh),
        (w, h),
        "{}: size differs from the render",
        golden.display()
    );
    let result = flip(
        &to_linear(&reference),
        &to_linear(&rgb),
        w as usize,
        h as usize,
        DEFAULT_PPD,
    );
    write_png(
        &out.join(format!("{name}.flip.png")),
        w,
        h,
        &result.heatmap_rgb8(),
    )
    .expect("write the error map");
    if result.mean() <= tolerance.mean && result.percentile(0.999) <= tolerance.p999 {
        Outcome::Matched(result)
    } else {
        Outcome::Different(result)
    }
}
