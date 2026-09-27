//! Texture compression research (ADR 0041): block_compression's CPU encoders (a Rust port
//! of Intel's ISPC Texture Compressor kernels) on a glTF scene's textures, per usage:
//! color and masked color as BC7 (and BC1 for reference), normal maps as BC5, data as
//! BC7. Reports the PSNR of level 0 against the uncompressed texels and the encoding rate
//! on one thread, per preset. Prints Markdown.
//!
//! `cargo run --release -- ../../test-assets/Sponza/Sponza.gltf`

use block_compression::decode::decompress_blocks_as_rgba8;
use block_compression::encode::compress_rgba8;
use block_compression::{BC7Settings, CompressionVariant};
use engine_assets::{AlphaMode, TextureUsage, build_mips, decode_image, import_gltf, read_texture};
use std::time::Instant;

/// Texels of a level padded to whole 4 × 4 blocks by repeating the edge.
fn padded(rgba: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let (pw, ph) = (width.div_ceil(4) * 4, height.div_ceil(4) * 4);
    let mut out = vec![0u8; (pw * ph * 4) as usize];
    for y in 0..ph {
        for x in 0..pw {
            let (sx, sy) = (x.min(width - 1), y.min(height - 1));
            let s = ((sy * width + sx) * 4) as usize;
            let d = ((y * pw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    (out, pw, ph)
}

/// PSNR over the given channels.
fn psnr(a: &[u8], b: &[u8], channels: &[usize]) -> f64 {
    let mut sum = 0.0f64;
    let mut n = 0usize;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for &c in channels {
            let d = f64::from(pa[c]) - f64::from(pb[c]);
            sum += d * d;
            n += 1;
        }
    }
    let mse = sum / n.max(1) as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

struct Row {
    usage: &'static str,
    codec: String,
    mtexels: f64,
    seconds: f64,
    psnr_sum: f64,
    count: usize,
    worst: f64,
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| String::from("../../test-assets/Sponza/Sponza.gltf"));
    let bytes = std::fs::read(&path).unwrap();
    let dir = std::path::Path::new(&path).parent();
    let scene = import_gltf(&bytes, dir).unwrap();
    let mut uses: Vec<(usize, TextureUsage)> = Vec::new();
    for m in &scene.materials {
        let color = match m.alpha {
            AlphaMode::Mask(bits) => TextureUsage::ColorMasked {
                cutoff: f32::from_bits(bits),
            },
            _ => TextureUsage::Color,
        };
        for (t, usage) in [
            (m.base_color_texture, color),
            (m.emissive_texture, TextureUsage::Color),
            (m.metallic_roughness_texture, TextureUsage::Data),
            (m.occlusion_texture, TextureUsage::Data),
            (m.normal_texture, TextureUsage::Normal),
        ] {
            if let Some(t) = t
                && !uses.contains(&(t, usage))
            {
                uses.push((t, usage));
            }
        }
    }

    let bc7 = |name: &'static str, s: BC7Settings| (name, CompressionVariant::BC7(s));
    let mut rows: Vec<Row> = Vec::new();
    for &(texture, usage) in &uses {
        let encoded = read_texture(&scene.textures[texture].source, dir).unwrap();
        let image = decode_image(&encoded).unwrap();
        let mips = build_mips(&image, usage);
        let has_alpha = image.rgba.chunks_exact(4).any(|p| p[3] < 255);
        let (label, channels, codecs): (
            &'static str,
            Vec<usize>,
            Vec<(&'static str, CompressionVariant)>,
        ) = match usage {
            TextureUsage::Color | TextureUsage::ColorMasked { .. } if has_alpha => (
                "color with alpha",
                vec![0, 1, 2, 3],
                vec![
                    bc7("BC7 alpha ultrafast", BC7Settings::alpha_ultrafast()),
                    bc7("BC7 alpha very fast", BC7Settings::alpha_very_fast()),
                    bc7("BC7 alpha fast", BC7Settings::alpha_fast()),
                    bc7("BC7 alpha basic", BC7Settings::alpha_basic()),
                    ("BC3", CompressionVariant::BC3),
                ],
            ),
            TextureUsage::Color | TextureUsage::ColorMasked { .. } => (
                "color",
                vec![0, 1, 2],
                vec![
                    bc7("BC7 ultra fast", BC7Settings::opaque_ultra_fast()),
                    bc7("BC7 very fast", BC7Settings::opaque_very_fast()),
                    bc7("BC7 fast", BC7Settings::opaque_fast()),
                    bc7("BC7 basic", BC7Settings::opaque_basic()),
                    ("BC1", CompressionVariant::BC1),
                ],
            ),
            TextureUsage::Data => (
                "data",
                vec![0, 1, 2],
                vec![
                    bc7("BC7 very fast", BC7Settings::opaque_very_fast()),
                    bc7("BC7 fast", BC7Settings::opaque_fast()),
                    bc7("BC7 basic", BC7Settings::opaque_basic()),
                    ("BC1", CompressionVariant::BC1),
                ],
            ),
            TextureUsage::Normal => (
                "normal",
                vec![0, 1],
                vec![
                    ("BC5", CompressionVariant::BC5),
                    bc7("BC7 fast", BC7Settings::opaque_fast()),
                ],
            ),
        };
        let (level0, w, h) = padded(&mips.levels[0], mips.width, mips.height);
        for (name, codec) in codecs {
            let mut blocks = vec![0u8; codec.blocks_byte_size(w, h)];
            let start = Instant::now();
            compress_rgba8(codec, &level0, &mut blocks, w, h, w * 4);
            let seconds = start.elapsed().as_secs_f64();
            let mut decoded = vec![0u8; level0.len()];
            decompress_blocks_as_rgba8(codec, w, h, &blocks, &mut decoded);
            // BC4/BC5 decode to R / RG: compare the channels they keep.
            let p = psnr(&level0, &decoded, &channels);
            let mtexels = f64::from(w) * f64::from(h) / 1e6;
            match rows
                .iter_mut()
                .find(|r| r.usage == label && r.codec == name)
            {
                Some(r) => {
                    r.mtexels += mtexels;
                    r.seconds += seconds;
                    r.psnr_sum += p;
                    r.count += 1;
                    r.worst = r.worst.min(p);
                }
                None => rows.push(Row {
                    usage: label,
                    codec: name.to_owned(),
                    mtexels,
                    seconds,
                    psnr_sum: p,
                    count: 1,
                    worst: p,
                }),
            }
        }
    }
    println!(
        "### block_compression {} on {path}, level 0, one thread",
        "0.10.0"
    );
    println!();
    println!("| usage | codec | textures | Mtexels | Mtexels/s | mean PSNR dB | worst PSNR dB |");
    println!("|---|---|---:|---:|---:|---:|---:|");
    for r in &rows {
        println!(
            "| {} | {} | {} | {:.1} | {:.1} | {:.2} | {:.2} |",
            r.usage,
            r.codec,
            r.count,
            r.mtexels,
            r.mtexels / r.seconds,
            r.psnr_sum / r.count as f64,
            r.worst
        );
    }
}
