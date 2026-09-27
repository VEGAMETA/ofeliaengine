//! Block compression of textures (ADR 0041): the GPU reads 4 × 4 blocks of 16 bytes (BC7,
//! BC5) instead of 64 bytes of RGBA8, a quarter of the memory and bandwidth.
//!
//! - **Color** (sRGB, with or without alpha) and **data** (metallic-roughness, occlusion):
//!   BC7, all its modes for the kind of content.
//! - **Normal maps:** BC5, X and Y at full precision each; shaders rebuild Z.
//!
//! Encoders: block_compression 0.10 (MIT), a Rust port of Intel's ISPC Texture
//! Compressor kernels, run on the job system by rows of blocks. Levels smaller than a
//! block are padded by repeating their edge texels.

use crate::texture_import::{TextureImage, TextureUsage};
use block_compression::encode::compress_rgba8;
use block_compression::{BC7Settings, CompressionVariant};
use engine_core::jobs::ThreadPool;

/// How a texture's texels are stored on the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextureFormat {
    /// 4 bytes per texel, sRGB color.
    Rgba8Srgb,
    /// 4 bytes per texel, linear data.
    Rgba8Unorm,
    /// BC7 blocks (16 bytes per 4 × 4), sRGB color.
    Bc7Srgb,
    /// BC7 blocks, linear data.
    Bc7Unorm,
    /// BC5 blocks (16 bytes per 4 × 4): two channels, red and green.
    Bc5Unorm,
}

impl TextureFormat {
    /// Whether texels are stored in 4 × 4 blocks.
    pub fn is_block_compressed(self) -> bool {
        matches!(self, Self::Bc7Srgb | Self::Bc7Unorm | Self::Bc5Unorm)
    }

    /// Whether the GPU decodes sRGB to linear when sampling.
    pub fn is_srgb(self) -> bool {
        matches!(self, Self::Rgba8Srgb | Self::Bc7Srgb)
    }

    /// Bytes of a `width × height` level.
    pub fn level_bytes(self, width: u32, height: u32) -> usize {
        if self.is_block_compressed() {
            (width.div_ceil(4) * height.div_ceil(4) * 16) as usize
        } else {
            (width * height * 4) as usize
        }
    }

    /// A stable number for cache keys and files.
    pub fn code(self) -> u32 {
        match self {
            Self::Rgba8Srgb => 1,
            Self::Rgba8Unorm => 2,
            Self::Bc7Srgb => 3,
            Self::Bc7Unorm => 4,
            Self::Bc5Unorm => 5,
        }
    }

    /// The format of [`Self::code`].
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::Rgba8Srgb,
            2 => Self::Rgba8Unorm,
            3 => Self::Bc7Srgb,
            4 => Self::Bc7Unorm,
            5 => Self::Bc5Unorm,
            _ => return None,
        })
    }
}

/// A texture ready for the GPU: every level in `format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedTexture {
    /// How texels are stored.
    pub format: TextureFormat,
    /// Width of level 0.
    pub width: u32,
    /// Height of level 0.
    pub height: u32,
    /// Levels from the full size down to 1 × 1, each `format.level_bytes(w, h)` long.
    pub levels: Vec<Vec<u8>>,
}

/// The encoder, its version and settings: part of every cache key of encoded textures.
pub const ENCODER: &str = "block_compression 0.10.0; BC7 opaque/alpha very fast; BC5";

/// The format a texture of `usage` is compressed to.
pub fn compressed_format(usage: TextureUsage) -> TextureFormat {
    match usage {
        TextureUsage::Color | TextureUsage::ColorMasked { .. } => TextureFormat::Bc7Srgb,
        TextureUsage::Data => TextureFormat::Bc7Unorm,
        TextureUsage::Normal => TextureFormat::Bc5Unorm,
    }
}

/// Block rows compressed per job: enough work per chunk to amortize scheduling.
const ROWS_PER_JOB: usize = 8;

/// Compresses every level of `texture` for its `usage` (see the module documentation), in
/// parallel on `jobs` when given.
pub fn compress(
    texture: &TextureImage,
    usage: TextureUsage,
    jobs: Option<&ThreadPool>,
) -> EncodedTexture {
    engine_profiler::zone!("assets::compress_texture");
    let format = compressed_format(usage);
    let variant = match format {
        TextureFormat::Bc5Unorm => CompressionVariant::BC5,
        _ => {
            let alpha = matches!(
                usage,
                TextureUsage::Color | TextureUsage::ColorMasked { .. }
            ) && texture.levels[0].chunks_exact(4).any(|p| p[3] < 255);
            CompressionVariant::BC7(if alpha {
                BC7Settings::alpha_very_fast()
            } else {
                BC7Settings::opaque_very_fast()
            })
        }
    };
    let (mut w, mut h) = (texture.width.max(1), texture.height.max(1));
    let mut levels = Vec::with_capacity(texture.levels.len());
    for level in &texture.levels {
        levels.push(compress_level(variant, level, w, h, jobs));
        (w, h) = ((w / 2).max(1), (h / 2).max(1));
    }
    EncodedTexture {
        format,
        width: texture.width.max(1),
        height: texture.height.max(1),
        levels,
    }
}

/// One RGBA8 level into blocks.
fn compress_level(
    variant: CompressionVariant,
    rgba: &[u8],
    width: u32,
    height: u32,
    jobs: Option<&ThreadPool>,
) -> Vec<u8> {
    let (bw, bh) = (width.div_ceil(4), height.div_ceil(4));
    let (pw, ph) = (bw * 4, bh * 4);
    // Pad to whole blocks by repeating the last column and row.
    let padded;
    let texels = if (pw, ph) == (width, height) {
        rgba
    } else {
        let mut out = vec![0u8; (pw * ph * 4) as usize];
        for y in 0..ph {
            let sy = y.min(height - 1);
            for x in 0..pw {
                let s = ((sy * width + x.min(width - 1)) * 4) as usize;
                let d = ((y * pw + x) * 4) as usize;
                out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
            }
        }
        padded = out;
        &padded
    };
    let row_bytes = (bw * 16) as usize;
    let mut blocks = vec![0u8; row_bytes * bh as usize];
    let texel_row = (pw * 4 * 4) as usize;
    let compress_rows = |first: usize, rows: usize, out: &mut [u8]| {
        compress_rgba8(
            variant,
            &texels[first * texel_row..(first + rows) * texel_row],
            out,
            pw,
            (rows * 4) as u32,
            pw * 4,
        );
    };
    match jobs {
        Some(jobs) if bh as usize > ROWS_PER_JOB => {
            // Each chunk writes its own rows of blocks.
            let chunks: Vec<&mut [u8]> = blocks.chunks_mut(row_bytes * ROWS_PER_JOB).collect();
            let chunks: Vec<std::sync::Mutex<&mut [u8]>> =
                chunks.into_iter().map(std::sync::Mutex::new).collect();
            jobs.parallel_for(chunks.len(), 1, |chunk, _| {
                let mut out = chunks[chunk].lock().unwrap_or_else(|e| e.into_inner());
                let rows = out.len() / row_bytes;
                compress_rows(chunk * ROWS_PER_JOB, rows, &mut out);
            });
        }
        _ => compress_rows(0, bh as usize, &mut blocks),
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texture_import::{Image, build_mips};
    use block_compression::decode::decompress_blocks_as_rgba8;

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

    fn psnr(a: &[u8], b: &[u8], channels: &[usize]) -> f64 {
        let (mut sum, mut n) = (0.0f64, 0usize);
        for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
            for &c in channels {
                let d = f64::from(pa[c]) - f64::from(pb[c]);
                sum += d * d;
                n += 1;
            }
        }
        10.0 * (255.0 * 255.0 / (sum / n as f64).max(1e-9)).log10()
    }

    /// A smooth gradient with a little texture, like a photograph.
    fn photo(x: u32, y: u32) -> [u8; 4] {
        let n = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) % 9;
        [
            (x * 255 / 63) as u8,
            (y * 255 / 63) as u8,
            (((x + y) * 2) as u8).wrapping_add(n as u8),
            255,
        ]
    }

    #[test]
    fn every_level_is_compressed_to_its_size() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        // 70 × 37: odd sizes, levels below a block.
        let mips = build_mips(&image(70, 37, photo), TextureUsage::Color);
        let encoded = compress(&mips, TextureUsage::Color, Some(&jobs));
        assert_eq!(encoded.format, TextureFormat::Bc7Srgb);
        assert_eq!(encoded.levels.len(), mips.levels.len());
        let (mut w, mut h) = (70, 37);
        for level in &encoded.levels {
            assert_eq!(level.len(), encoded.format.level_bytes(w, h));
            (w, h) = ((w / 2).max(1), (h / 2).max(1));
        }
    }

    #[test]
    fn quality_and_threads() {
        let jobs = ThreadPool::builder().workers(3).build().unwrap();
        let img = image(64, 64, photo);
        let mips = build_mips(&img, TextureUsage::Color);
        let parallel = compress(&mips, TextureUsage::Color, Some(&jobs));
        let serial = compress(&mips, TextureUsage::Color, None);
        assert_eq!(parallel, serial, "the same blocks with and without threads");
        let mut decoded = vec![0u8; 64 * 64 * 4];
        decompress_blocks_as_rgba8(
            CompressionVariant::BC7(BC7Settings::opaque_very_fast()),
            64,
            64,
            &parallel.levels[0],
            &mut decoded,
        );
        let p = psnr(&img.rgba, &decoded, &[0, 1, 2]);
        assert!(p > 40.0, "BC7: {p:.1} dB");

        // Normal maps keep X and Y.
        let normals = image(32, 32, |x, y| {
            let (nx, ny) = (x as f32 / 31.0 - 0.5, y as f32 / 31.0 - 0.5);
            let nz = (1.0 - nx * nx - ny * ny).sqrt();
            let b = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
            [b(nx), b(ny), b(nz), 255]
        });
        let mips = build_mips(&normals, TextureUsage::Normal);
        let encoded = compress(&mips, TextureUsage::Normal, None);
        assert_eq!(encoded.format, TextureFormat::Bc5Unorm);
        let mut decoded = vec![0u8; 32 * 32 * 4];
        decompress_blocks_as_rgba8(
            CompressionVariant::BC5,
            32,
            32,
            &encoded.levels[0],
            &mut decoded,
        );
        let p = psnr(&mips.levels[0], &decoded, &[0, 1]);
        assert!(p > 45.0, "BC5: {p:.1} dB");
    }
}
