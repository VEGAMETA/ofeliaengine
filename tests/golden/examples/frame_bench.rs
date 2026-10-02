//! What the frame renderer's choices cost and give (ADR 0072), on Sponza at 1920 × 1080
//! through the golden test bed: GPU time per antialiasing mode and render size, and per
//! ambient occlusion preset, with the image's mean FLIP against a reference.
//!
//! - **Antialiasing:** off at the output size and at 67 % (stretched by the tone mapper),
//!   TAA, TAAU at 67 / 59 / 50 %, FSR 3.1 Native AA / Quality / Performance, and DLSS where
//!   Streamline is (`ENGINE_STREAMLINE_DIR`). Temporal modes accumulate 32 frames with the
//!   camera still. Reference: the frame rendered at 3840 × 2160 without antialiasing and
//!   averaged 2 × 2 (in linear light).
//! - **Ambient occlusion**: none, Low, Medium, High, Ultra, without antialiasing at the
//!   output size and with TAAU from 67 % (AO runs at the render size). Reference: 16 slices
//!   × 16 samples per side, with the same antialiasing.
//!
//! `cargo run -p engine_golden --release --example frame_bench [-- --aa | --ao]`
//! (`ENGINE_ADAPTER=radeon` for the iGPU; `VK_LOADER_LAYERS_DISABLE=~implicit~`)

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_frame::{Antialiasing, Quality};
use engine_golden::flip::{DEFAULT_PPD, flip, srgb_to_linear};
use engine_golden::scene::{RenderOptions, Rendered, Shot, Skip, Source, render};
use engine_render::ao::{AoQuality, AoSettings};
use std::path::PathBuf;

const TIMED: usize = 100;

fn linear(r: &Rendered) -> Vec<[f32; 3]> {
    r.rgba8
        .chunks_exact(4)
        .map(|p| {
            [
                srgb_to_linear(f32::from(p[0]) / 255.0),
                srgb_to_linear(f32::from(p[1]) / 255.0),
                srgb_to_linear(f32::from(p[2]) / 255.0),
            ]
        })
        .collect()
}

/// A 2 × 2 box filter in linear light.
fn downsample(r: &Rendered) -> Vec<[f32; 3]> {
    let (w, h) = (r.width as usize, r.height as usize);
    let full = linear(r);
    let mut out = Vec::with_capacity(w / 2 * h / 2);
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let mut c = [0.0f32; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = full[(2 * y + dy) * w + 2 * x + dx];
                for k in 0..3 {
                    c[k] += p[k] * 0.25;
                }
            }
            out.push(c);
        }
    }
    out
}

fn sponza(name: &'static str) -> Shot {
    Shot {
        size: [1920, 1080],
        ..Shot::new(name, Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
    }
}

fn run(shot: &Shot) -> Option<Rendered> {
    match render(
        shot,
        RenderOptions {
            validation: false,
            timed_frames: TIMED,
        },
    ) {
        Ok(r) => Some(r),
        Err(Skip::NoDlss(e)) => {
            println!("{}: skipped, no DLSS ({e})", shot.name);
            None
        }
        Err(e) => panic!("{}: {e:?}", shot.name),
    }
}

/// The passes worth showing: the scene's, the antialiaser's, the post chain's.
fn group(r: &Rendered, names: &[&str]) -> f64 {
    r.pass_us
        .iter()
        .filter(|(name, _)| names.iter().any(|n| name.starts_with(n)))
        .map(|(_, us)| us)
        .fold(0.0, |a, b| a + b)
        / 1000.0
}

/// Every pass, ms (the GPU's busy time; the frame end to end also counts idle gaps when
/// other work shares the GPU).
fn total(r: &Rendered) -> f64 {
    r.pass_us.values().fold(0.0, |a, b| a + b) / 1000.0
}

fn antialiasing() {
    let mut reference = sponza("reference");
    reference.size = [3840, 2160];
    let reference = run(&reference).unwrap();
    println!("Adapter: {}\n", reference.adapter);
    let reference = downsample(&reference);
    let modes: [(&str, Antialiasing, Quality); 11] = [
        ("off", Antialiasing::OFF, Quality::Native),
        ("off, 67 % (stretched)", Antialiasing::OFF, Quality::Quality),
        ("TAA", Antialiasing::TAAU, Quality::Native),
        ("TAAU 67 %", Antialiasing::TAAU, Quality::Quality),
        ("TAAU 59 %", Antialiasing::TAAU, Quality::Balanced),
        ("TAAU 50 %", Antialiasing::TAAU, Quality::Performance),
        ("FSR Native AA", Antialiasing::FSR, Quality::Native),
        ("FSR Quality (67 %)", Antialiasing::FSR, Quality::Quality),
        (
            "FSR Performance (50 %)",
            Antialiasing::FSR,
            Quality::Performance,
        ),
        ("DLAA", Antialiasing::DLSS, Quality::Native),
        ("DLSS Quality (67 %)", Antialiasing::DLSS, Quality::Quality),
    ];
    println!(
        "| mode | passes ms | GPU frame ms | scene passes ms | antialiasing ms | post ms | mean FLIP vs 2×2 SSAA |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for (label, antialiasing, quality) in modes {
        let shot = Shot {
            antialiasing,
            quality,
            ..sponza("mode")
        };
        let Some(r) = run(&shot) else {
            continue;
        };
        let image = linear(&r);
        let error = flip(&reference, &image, 1920, 1080, DEFAULT_PPD).mean();
        let aa = group(&r, &["taau", "fsr", "dlss", "exposure value"]);
        let post = group(&r, &["bloom", "tonemap"]);
        let scene = total(&r) - aa - post;
        println!(
            "| {label} | {:.2} | {:.2} | {scene:.2} | {aa:.2} | {post:.2} | {error:.4} |",
            total(&r),
            r.frame_us / 1000.0
        );
    }
    println!();
}

/// Ambient occlusion presets with `antialiasing` at `quality`: AO runs at the render size.
fn ambient_occlusion(antialiasing: Antialiasing, quality: Quality, label: &str) {
    let with = |ao: Option<AoQuality>| Shot {
        ao: ao.map(|quality| AoSettings {
            quality,
            ..AoSettings::default()
        }),
        antialiasing,
        quality,
        ..sponza("ao")
    };
    let reference = run(&with(Some(AoQuality::Custom {
        slices: 16,
        steps: 16,
    })))
    .unwrap();
    println!(
        "Adapter: {}; {label}
",
        reference.adapter
    );
    let reference = linear(&reference);
    println!(
        "| ambient occlusion | passes ms | GPU frame ms | AO passes ms | depth prepass ms | mean FLIP vs 16×16 |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    let presets: [(&str, Option<AoQuality>); 5] = [
        ("none", None),
        ("Low (1×2)", Some(AoQuality::Low)),
        ("Medium (2×2)", Some(AoQuality::Medium)),
        ("High (3×3)", Some(AoQuality::High)),
        ("Ultra (9×3)", Some(AoQuality::Ultra)),
    ];
    for (name, ao) in presets {
        let r = run(&with(ao)).unwrap();
        let error = flip(&reference, &linear(&r), 1920, 1080, DEFAULT_PPD).mean();
        println!(
            "| {name} | {:.2} | {:.2} | {:.2} | {:.2} | {error:.4} |",
            total(&r),
            r.frame_us / 1000.0,
            group(&r, &["ao"]),
            group(&r, &["depth prepass"]),
        );
    }
    println!();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let only_ao = args.iter().any(|a| a == "--ao");
    let only_aa = args.iter().any(|a| a == "--aa");
    if !only_ao {
        antialiasing();
    }
    if !only_aa {
        ambient_occlusion(Antialiasing::OFF, Quality::Native, "no antialiasing, 1080p");
        ambient_occlusion(Antialiasing::TAAU, Quality::Quality, "TAAU from 67 %");
    }
}
