//! Texture import speed (ADR 0040): every texture of a glTF scene read, decoded and given
//! its mip chain for the use its materials make of it, on one thread, stage by stage.
//! Prints Markdown: per format, the decode rate; per usage, the mip rate.
//!
//! `cargo run -p engine_assets --release --example texture_bench -- test-assets/Sponza/Sponza.gltf`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::{AlphaMode, TextureUsage, build_mips, decode_image, import_gltf, read_texture};
use std::time::Instant;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| String::from("test-assets/Sponza/Sponza.gltf"));
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
        for (texture, usage) in [
            (m.base_color_texture, color),
            (m.emissive_texture, TextureUsage::Color),
            (m.metallic_roughness_texture, TextureUsage::Data),
            (m.occlusion_texture, TextureUsage::Data),
            (m.normal_texture, TextureUsage::Normal),
        ] {
            if let Some(t) = texture
                && !uses.contains(&(t, usage))
            {
                uses.push((t, usage));
            }
        }
    }

    // (label, megatexels, seconds) per stage.
    let mut decode: Vec<(&'static str, f64, f64)> = Vec::new();
    let mut mips: Vec<(&'static str, f64, f64)> = Vec::new();
    let add = |list: &mut Vec<(&'static str, f64, f64)>, label, mtexels, seconds| match list
        .iter_mut()
        .find(|(l, ..)| *l == label)
    {
        Some(entry) => {
            entry.1 += mtexels;
            entry.2 += seconds;
        }
        None => list.push((label, mtexels, seconds)),
    };
    for &(texture, usage) in &uses {
        let encoded = read_texture(&scene.textures[texture].source, dir).unwrap();
        let format = if encoded.starts_with(b"\x89PNG") {
            "PNG"
        } else {
            "JPEG"
        };
        let start = Instant::now();
        let image = decode_image(&encoded).unwrap();
        let decoded = start.elapsed().as_secs_f64();
        let mtexels = f64::from(image.width) * f64::from(image.height) / 1e6;
        add(&mut decode, format, mtexels, decoded);
        let start = Instant::now();
        let levels = build_mips(&image, usage);
        let built = start.elapsed().as_secs_f64();
        assert!(!levels.levels.is_empty());
        let label = match usage {
            TextureUsage::Color => "color",
            TextureUsage::ColorMasked { .. } => "masked color",
            TextureUsage::Data => "data",
            TextureUsage::Normal => "normal",
        };
        add(&mut mips, label, mtexels, built);
    }

    println!("### Texture import, one thread: {path}");
    println!();
    println!("| stage | kind | Mtexels | ms | Mtexels/s |");
    println!("|---|---|---:|---:|---:|");
    let mut total = 0.0;
    for (stage, list) in [("decode", &decode), ("mips", &mips)] {
        for &(label, mtexels, seconds) in list {
            total += seconds;
            println!(
                "| {stage} | {label} | {mtexels:.1} | {:.0} | {:.1} |",
                seconds * 1e3,
                mtexels / seconds
            );
        }
    }
    println!();
    println!("{} textures, {:.0} ms in total", uses.len(), total * 1e3);

    // All of it at once on the job system, largest first.
    let jobs = engine_core::jobs::ThreadPool::builder().build().unwrap();
    let start = Instant::now();
    jobs.parallel_for(uses.len(), 1, |_, range| {
        for k in range {
            let (texture, usage) = uses[k];
            let encoded = read_texture(&scene.textures[texture].source, dir).unwrap();
            let image = decode_image(&encoded).unwrap();
            assert!(!build_mips(&image, usage).levels.is_empty());
        }
    });
    let wall = start.elapsed().as_secs_f64();
    println!(
        "{} threads: {:.0} ms ({:.1}× one thread)",
        jobs.num_workers() + 1,
        wall * 1e3,
        total / wall
    );
    // The same with plain threads, for comparison.
    let threads = jobs.num_workers() + 1;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(texture, usage)) = uses.get(k) else {
                        break;
                    };
                    let encoded = read_texture(&scene.textures[texture].source, dir).unwrap();
                    let image = decode_image(&encoded).unwrap();
                    assert!(!build_mips(&image, usage).levels.is_empty());
                }
            });
        }
    });
    println!(
        "{threads} std threads: {:.0} ms",
        start.elapsed().as_secs_f64() * 1e3
    );
}
