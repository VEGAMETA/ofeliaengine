//! What every antialiasing and scaling mode costs and gives (ADR 0075), on Sponza with
//! the full feature set (sky and atmosphere, height fog, ambient occlusion, the sun's
//! contact shadows, local lights with shadows, reflection probes and screen-space
//! reflections, bloom) through the golden test bed.
//!
//! For each mode:
//! - **cost**: the GPU's busy time over every pass (median of 100 frames), and the part
//!   the antialiasing adds (its own passes);
//! - **still**: mean FLIP and PSNR of a still frame against the reference: the frame at
//!   4 × 4 the size without antialiasing, averaged 4 × 4 in linear light;
//! - **moving**: the same for the last of `SEQUENCE` frames of a camera strafing slowly
//!   (temporal modes accumulate while it moves), against the reference at that position;
//! - **shimmer**: how much the image changes from frame to frame beyond what the
//!   reference changes: the mean over the sequence and the pixels of
//!   |(F[t] − F[t−1]) − (R[t] − R[t−1])| in display-encoded luma (0–255). Aliased edges
//!   crawl and flicker as the camera moves (high), temporal modes are stable but may lag
//!   (ghosting raises it too); the reference's own change is subtracted. With every
//!   feature on, the effects' own per-frame noise (ambient occlusion's and reflections'
//!   noise changes every frame under temporal modes, for them to average) counts too;
//!   **shimmer, plain** measures the same on Sponza lit by the sun and the sky alone,
//!   where only edges and textures can crawl.
//!
//! `cargo run -p engine_golden --release --example aa_bench [-- --quick] [-- --only <label>]`
//! (`ENGINE_ADAPTER=radeon` for the iGPU; `VK_LOADER_LAYERS_DISABLE=~implicit~`). `--quick`
//! renders 640 × 360 and fewer frames, for checking modes while developing.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::Vec3;
use engine_frame::{Antialiasing, Msaa, PostAntialiasing, Quality, SmaaQuality, Upscale};
use engine_golden::flip::{DEFAULT_PPD, flip, srgb_to_linear};
use engine_golden::scene::{RenderOptions, Rendered, Shot, Skip, Source, render, sponza_lights};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::reflections::SsrSettings;
use std::path::PathBuf;

/// Frames from the first temporal frame to the first capture: temporal modes have
/// converged (TAAU's 8 phases four times over).
const WARMUP: u32 = 32;
/// The camera's strafe per frame, metres (Sponza is about 30 m long): a fraction of a
/// pixel at 1080p near the camera, where aliasing crawls.
const MOTION: Vec3 = Vec3::new(0.0, 0.0, 0.004);
/// Supersampling of the reference per axis.
const REFERENCE_SCALE: u32 = 4;

struct Settings {
    size: [u32; 2],
    timed: usize,
    sequence: u32,
}

fn sponza(size: [u32; 2]) -> Shot {
    Shot {
        size,
        lights: sponza_lights(),
        local_shadows: true,
        ao: Some(AoSettings::default()),
        contact_shadows: true,
        probe_grid: Some([3, 1, 2]),
        ssr: Some(SsrSettings::default()),
        atmosphere: Some(Atmosphere::earth()),
        fog: Some(HeightFog {
            density: 0.02,
            base_height: 0.0,
            falloff: 0.3,
            ..HeightFog::default()
        }),
        capture_after: Some(WARMUP),
        ..Shot::new("aa", Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
    }
}

/// Sponza with the sun and the sky only: nothing stochastic, for the plain shimmer.
fn plain(size: [u32; 2]) -> Shot {
    Shot {
        size,
        capture_after: Some(WARMUP),
        ..Shot::new(
            "aa plain",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    }
}

/// The reference sequence of `base`: frames at 4 × 4 the size, averaged, and their luma.
fn reference_sequence(base: Shot, sequence: u32) -> (Vec<Vec<[f32; 3]>>, Vec<Vec<f32>>) {
    let big = [
        base.size[0] * REFERENCE_SCALE,
        base.size[1] * REFERENCE_SCALE,
    ];
    let k = REFERENCE_SCALE as usize;
    let r = run(
        &Shot {
            size: big,
            camera_motion: MOTION,
            captures: sequence,
            ..base
        },
        0,
    )
    .unwrap();
    let frames: Vec<Vec<[f32; 3]>> = r
        .frames
        .iter()
        .map(|f| downsample(f, big[0] as usize, big[1] as usize, k))
        .collect();
    let lumas = frames.iter().map(|f| luma(f)).collect();
    (frames, lumas)
}

/// `shot`'s sequence with the camera moving: its frames, linear.
fn sequence_of(shot: &Shot, sequence: u32) -> Option<Vec<Vec<[f32; 3]>>> {
    let r = run(
        &Shot {
            camera_motion: MOTION,
            captures: sequence,
            ..shot.clone()
        },
        0,
    )?;
    Some(r.frames.iter().map(|f| linear(f)).collect())
}

fn run(shot: &Shot, timed: usize) -> Option<Rendered> {
    match render(
        shot,
        RenderOptions {
            validation: false,
            timed_frames: timed,
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

fn linear(rgba8: &[u8]) -> Vec<[f32; 3]> {
    rgba8
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

/// A `k × k` box filter in linear light of a `w × h` frame.
fn downsample(rgba8: &[u8], w: usize, h: usize, k: usize) -> Vec<[f32; 3]> {
    let full = linear(rgba8);
    let (ow, oh) = (w / k, h / k);
    let weight = 1.0 / (k * k) as f32;
    let mut out = Vec::with_capacity(ow * oh);
    for y in 0..oh {
        for x in 0..ow {
            let mut c = [0.0f32; 3];
            for dy in 0..k {
                for dx in 0..k {
                    let p = full[(k * y + dy) * w + k * x + dx];
                    for i in 0..3 {
                        c[i] += p[i] * weight;
                    }
                }
            }
            out.push(c);
        }
    }
    out
}

fn encode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// PSNR in dB of display-encoded colors (0–1).
fn psnr(reference: &[[f32; 3]], test: &[[f32; 3]]) -> f64 {
    let mut sum = 0.0f64;
    for (r, t) in reference.iter().zip(test) {
        for i in 0..3 {
            let d = f64::from(encode(r[i]) - encode(t[i]));
            sum += d * d;
        }
    }
    let mse = sum / (reference.len() * 3) as f64;
    10.0 * (1.0 / mse.max(1e-12)).log10()
}

/// Display-encoded luma, 0–255.
fn luma(frame: &[[f32; 3]]) -> Vec<f32> {
    frame
        .iter()
        .map(|c| 255.0 * (0.2126 * encode(c[0]) + 0.7152 * encode(c[1]) + 0.0722 * encode(c[2])))
        .collect()
}

/// The mean of |(F[t] − F[t−1]) − (R[t] − R[t−1])| over the sequence.
fn shimmer(reference: &[Vec<f32>], test: &[Vec<f32>]) -> f64 {
    let mut sum = 0.0f64;
    let mut n = 0usize;
    for t in 1..reference.len().min(test.len()) {
        for i in 0..reference[t].len() {
            let dr = reference[t][i] - reference[t - 1][i];
            let df = test[t][i] - test[t - 1][i];
            sum += f64::from((df - dr).abs());
            n += 1;
        }
    }
    sum / n.max(1) as f64
}

/// The passes that are an antialiasing mode's own (MSAA's other cost is in the scene
/// passes: compare the total with "off").
fn aa_ms(r: &Rendered) -> f64 {
    const NAMES: [&str; 13] = [
        "taau",
        "fsr",
        "dlss",
        "exposure value",
        "fxaa",
        "smaa",
        "easu",
        "rcas",
        "msaa resolve",
        "subsurface resolve",
        "reflection inputs resolve",
        "temporal",
        "perceptual",
    ];
    r.pass_us
        .iter()
        .filter(|(name, _)| NAMES.iter().any(|n| name.starts_with(n)))
        .map(|(_, us)| us)
        .sum::<f64>()
        / 1000.0
}

fn total(r: &Rendered) -> f64 {
    r.pass_us.values().sum::<f64>() / 1000.0
}

fn modes() -> Vec<(&'static str, Antialiasing, Quality)> {
    use Antialiasing as A;
    let smaa = PostAntialiasing::Smaa(SmaaQuality::High);
    vec![
        ("off", A::OFF, Quality::Native),
        ("FXAA", A::FXAA, Quality::Native),
        (
            "SMAA 1x medium",
            A::post(PostAntialiasing::Smaa(SmaaQuality::Medium)),
            Quality::Native,
        ),
        ("MLAA", A::MLAA, Quality::Native),
        (
            "SMAA 1x low",
            A::post(PostAntialiasing::Smaa(SmaaQuality::Low)),
            Quality::Native,
        ),
        ("SMAA 1x high", A::SMAA_1X, Quality::Native),
        (
            "SMAA 1x ultra",
            A::post(PostAntialiasing::Smaa(SmaaQuality::Ultra)),
            Quality::Native,
        ),
        ("SMAA T2x", A::SMAA_T2X, Quality::Native),
        ("SMAA S2x", A::SMAA_S2X, Quality::Native),
        ("SMAA 4x", A::SMAA_4X, Quality::Native),
        ("MSAA 2×", A::msaa(Msaa::X2), Quality::Native),
        ("MSAA 4×", A::msaa(Msaa::X4), Quality::Native),
        ("MSAA 8×", A::msaa(Msaa::X8), Quality::Native),
        (
            "MSAA 4× + SMAA",
            A::msaa(Msaa::X4).with_post(smaa),
            Quality::Native,
        ),
        (
            "MSAA 4× + TAA",
            A::TAAU.with_samples(Msaa::X4),
            Quality::Native,
        ),
        ("TAA", A::TAAU, Quality::Native),
        ("TAAU 67 %", A::TAAU, Quality::Quality),
        ("TAAU 50 %", A::TAAU, Quality::Performance),
        ("FSR 3.1 Native AA", A::FSR, Quality::Native),
        ("FSR 3.1 Quality (67 %)", A::FSR, Quality::Quality),
        ("DLAA", A::DLSS, Quality::Native),
        ("DLSS Quality (67 %)", A::DLSS, Quality::Quality),
        ("bilinear 67 %", A::OFF, Quality::Quality),
        ("bilinear 50 %", A::OFF, Quality::Performance),
        (
            "nearest 67 %",
            A::spatial(Upscale::Nearest),
            Quality::Quality,
        ),
        (
            "nearest 50 %",
            A::spatial(Upscale::Nearest),
            Quality::Performance,
        ),
        (
            "integer 50 %",
            A::spatial(Upscale::Integer),
            Quality::Performance,
        ),
        (
            "FSR 1 RCAS only (100 %)",
            A::spatial(Upscale::Fsr1),
            Quality::Native,
        ),
        ("FSR 1 67 %", A::spatial(Upscale::Fsr1), Quality::Quality),
        (
            "FSR 1 50 %",
            A::spatial(Upscale::Fsr1),
            Quality::Performance,
        ),
        (
            "FSR 1 67 % + SMAA",
            A::spatial(Upscale::Fsr1).with_post(smaa),
            Quality::Quality,
        ),
    ]
}

fn main() {
    // The test bed's frame is large on the stack: more room than the main thread's 1 MiB.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(bench)
        .unwrap()
        .join()
        .unwrap();
}

fn bench() {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");
    let only: Vec<&str> = args
        .windows(2)
        .filter(|w| w[0] == "--only")
        .map(|w| w[1].as_str())
        .collect();
    let settings = if quick {
        Settings {
            size: [640, 360],
            timed: 10,
            sequence: 6,
        }
    } else {
        Settings {
            size: [1920, 1080],
            timed: 100,
            sequence: 10,
        }
    };
    let [w, h] = settings.size;
    let k = REFERENCE_SCALE as usize;
    let big = [w * REFERENCE_SCALE, h * REFERENCE_SCALE];

    // The references: a still frame, and the moving sequence.
    let still = run(&sponza(big), 0).unwrap();
    println!("Adapter: {}\n", still.adapter);
    let still = downsample(&still.rgba8, big[0] as usize, big[1] as usize, k);
    let (moving, moving_luma) = reference_sequence(sponza(settings.size), settings.sequence);
    let last = moving.last().unwrap();
    let (_, plain_luma) = reference_sequence(plain(settings.size), settings.sequence);

    println!(
        "| mode | passes ms | vs off ms | own passes ms | still FLIP | still PSNR dB | moving FLIP | moving PSNR dB | shimmer | shimmer, plain |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let mut off_ms = None;
    for (label, antialiasing, quality) in modes() {
        if !only.is_empty() && !only.contains(&label) {
            continue;
        }
        let shot = Shot {
            antialiasing,
            quality,
            ..sponza(settings.size)
        };
        let Some(timed) = run(&shot, settings.timed) else {
            continue;
        };
        let image = linear(&timed.rgba8);
        let still_flip = flip(&still, &image, w as usize, h as usize, DEFAULT_PPD).mean();
        let still_psnr = psnr(&still, &image);
        let Some(frames) = sequence_of(&shot, settings.sequence) else {
            continue;
        };
        let plain_shot = Shot {
            antialiasing,
            quality,
            ..plain(settings.size)
        };
        let Some(plain_frames) = sequence_of(&plain_shot, settings.sequence) else {
            continue;
        };
        let plain_lumas: Vec<Vec<f32>> = plain_frames.iter().map(|f| luma(f)).collect();
        let plain_shimmer = shimmer(&plain_luma, &plain_lumas);
        let moving_image = frames.last().unwrap();
        let moving_flip = flip(last, moving_image, w as usize, h as usize, DEFAULT_PPD).mean();
        let moving_psnr = psnr(last, moving_image);
        let frames_luma: Vec<Vec<f32>> = frames.iter().map(|f| luma(f)).collect();
        let shimmer = shimmer(&moving_luma, &frames_luma);
        let passes = total(&timed);
        let off = *off_ms.get_or_insert(passes);
        println!(
            "| {label} | {passes:.2} | {:+.2} | {:.2} | {still_flip:.4} | {still_psnr:.2} | {moving_flip:.4} | {moving_psnr:.2} | {shimmer:.3} | {plain_shimmer:.3} |",
            passes - off,
            aa_ms(&timed),
        );
        if let Ok(dir) = std::env::var("ENGINE_AA_BENCH_DUMP") {
            let path = PathBuf::from(dir).join(format!(
                "{}.png",
                label.replace([' ', '%', '(', ')', '×', '+', '.'], "_")
            ));
            let rgb: Vec<u8> = timed
                .rgba8
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            engine_golden::golden::write_png(&path, w, h, &rgb).unwrap();
            let encode8 = |c: f32| (encode(c) * 255.0).round() as u8;
            let moving_rgb: Vec<u8> = moving_image
                .iter()
                .flat_map(|c| c.map(encode8))
                .collect();
            let path = path.with_extension("moving.png");
            engine_golden::golden::write_png(&path, w, h, &moving_rgb).unwrap();
        }
    }
}
