//! Brixelizer GI against probe GI (ADR 0090, ADR 0084) on Sponza through the golden test
//! bed, at 1920 × 1080 without antialiasing:
//!
//! - **Cost:** GPU time of each GI's passes once it has settled, the median of 100 frames.
//! - **Memory:** what each GI keeps between frames.
//! - **Load:** probe GI's frames and GPU time until its volume converged (before its first
//!   frame shown); Brixelizer GI's distance field builds, and how its light fills in from
//!   its first frame (frames to half and to 90 % of its value after `--frames`).
//! - **Picture:** the frame and the GI's irradiance of the courtyard (`sponza_gi`'s view)
//!   and of the medallions from above (`sponza_gi_medallions`'s), written to
//!   `target/gi_bench/<view>_<gi>_{frame,irradiance}.png`.
//! - **Response:** with only the sun lighting Sponza (a black sky), the frames after the
//!   sun goes out until the GI's mean irradiance falls to half and to a tenth (at 640 ×
//!   360: every frame is captured).
//!
//! Brixelizer GI is timed and pictured after a warm-up of 256 frames (its caches settle by
//! half every 48 frames or so).
//!
//! With `--sdf normals|bricks|cascades|distance|iterations`, only Brixelizer's distance
//! field seen from both views (`target/gi_bench/<view>_sdf_<view>.png`).
//!
//! `cargo run -p engine_golden --release --example gi_bench [-- --frames N] [--sdf <view>]`
//! (`--frames`: the build-up's and the response's length, 400 by default)
//! (`ENGINE_ADAPTER=radeon` for the iGPU; `VK_LOADER_LAYERS_DISABLE=~implicit~`)

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_brixelizer::SdfDebugView;
use engine_core::math::Vec3;
use engine_golden::golden::write_png;
use engine_golden::scene::{BRIXELIZER_WARMUP, RenderOptions, Rendered, Shot, Source, render};
use std::path::{Path, PathBuf};

const TIMED: usize = 100;
/// Brixelizer GI's warm-up before the timing and the pictures here: its caches settle by
/// half every 48 frames or so.
const BENCH_WARMUP: u32 = 256;

#[derive(Clone, Copy, PartialEq)]
enum Gi {
    Probes,
    Brixelizer,
}

impl Gi {
    fn name(self) -> &'static str {
        match self {
            Gi::Probes => "probe GI",
            Gi::Brixelizer => "Brixelizer GI",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Gi::Probes => "probes",
            Gi::Brixelizer => "brixelizer",
        }
    }

    /// The GI's passes (probe GI's ray scene with it: nothing else traces in these shots).
    fn passes(self) -> &'static [&'static str] {
        match self {
            Gi::Probes => &["gi ", "blas", "tlas"],
            Gi::Brixelizer => &["brixelizer"],
        }
    }

    fn apply(self, shot: Shot) -> Shot {
        match self {
            Gi::Probes => Shot {
                gi: Some(1.0),
                ..shot
            },
            Gi::Brixelizer => Shot {
                brixelizer_gi: true,
                ..shot
            },
        }
    }
}

/// `sponza_gi`'s view (the courtyard) or `sponza_gi_medallions`'s.
fn view(name: &'static str, medallions: bool) -> Shot {
    let shot = Shot {
        size: [1920, 1080],
        runtime: true,
        exposure: 3.0,
        gi_readback: true,
        ..Shot::new(name, Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
    };
    if medallions {
        Shot {
            eye: Some(Vec3::new(-6.0, 6.5, -2.0)),
            target: Some(Vec3::new(-3.0, 2.0, 4.5)),
            probe_grid: Some([6, 2, 3]),
            ..shot
        }
    } else {
        shot
    }
}

fn run(shot: &Shot, timed_frames: usize) -> Rendered {
    render(
        shot,
        RenderOptions {
            validation: false,
            timed_frames,
        },
    )
    .unwrap_or_else(|e| panic!("{}: {e:?}", shot.name))
}

fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gi_bench")
}

/// The GI's irradiance as an image: x / (1 + x), sRGB-encoded.
fn irradiance_png(r: &Rendered, path: &Path) {
    let Some((w, h, pixels)) = &r.gi_image else {
        return;
    };
    let encode = |v: f32| {
        let v = (v / (1.0 + v)).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    let rgb: Vec<u8> = pixels
        .iter()
        .flat_map(|p| [encode(p[0]), encode(p[1]), encode(p[2])])
        .collect();
    write_png(path, *w, *h, &rgb).unwrap();
}

fn frame_png(r: &Rendered, path: &Path) {
    let rgb: Vec<u8> = r
        .rgba8
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    write_png(path, r.width, r.height, &rgb).unwrap();
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn main() {
    let mut curve_frames = 400u32;
    let mut sdf = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => curve_frames = args.next().unwrap().parse().unwrap(),
            "--sdf" => {
                let name = args.next().unwrap();
                sdf = Some((
                    match name.as_str() {
                        "normals" => SdfDebugView::Normals,
                        "bricks" => SdfDebugView::Bricks,
                        "cascades" => SdfDebugView::Cascades,
                        "iterations" => SdfDebugView::Iterations,
                        _ => SdfDebugView::Distance,
                    },
                    name,
                ));
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let dir = out_dir();
    std::fs::create_dir_all(&dir).unwrap();
    if let Some((debug, name)) = sdf {
        for (label, medallions) in [("courtyard", false), ("medallions", true)] {
            let shot = Shot {
                brixelizer_view: Some(debug),
                gi_readback: false,
                ..Gi::Brixelizer.apply(view("gi_bench", medallions))
            };
            let r = run(&shot, 0);
            frame_png(&r, &dir.join(format!("{label}_sdf_{name}.png")));
        }
        return;
    }
    println!(
        "Brixelizer GI warmed up {BENCH_WARMUP} frames before the timing and the pictures \
         (the golden shots: {BRIXELIZER_WARMUP}).\n"
    );
    println!("| view | GI | GI passes ms | GPU frame ms | GI memory MiB | mean irradiance |");
    println!("|---|---|---:|---:|---:|---:|");
    let mut breakdown = Vec::new();
    let mut memory = Vec::new();
    let mut startup = Vec::new();
    let mut adapter = String::new();
    for (label, medallions) in [("courtyard", false), ("medallions", true)] {
        for gi in [Gi::Probes, Gi::Brixelizer] {
            let shot = Shot {
                brixelizer_warmup: Some(BENCH_WARMUP),
                ..gi.apply(view("gi_bench", medallions))
            };
            let r = run(&shot, TIMED);
            adapter.clone_from(&r.adapter);
            let gi_passes: Vec<(&str, f64)> = r
                .pass_us
                .iter()
                .filter(|(name, _)| gi.passes().iter().any(|p| name.starts_with(p)))
                .map(|(name, us)| (*name, *us))
                .collect();
            let gi_ms = gi_passes.iter().map(|(_, us)| us).sum::<f64>() / 1000.0;
            let mean = r.gi_means.first().copied().unwrap_or_default();
            println!(
                "| {label} | {} | {gi_ms:.3} | {:.3} | {:.1} | {:.3} |",
                gi.name(),
                r.frame_us / 1000.0,
                r.gi_bytes as f64 / f64::from(1 << 20),
                luminance(mean),
            );
            breakdown.push(format!(
                "{label}, {}: {}",
                gi.name(),
                gi_passes
                    .iter()
                    .map(|(name, us)| format!("{name} {:.3}", us / 1000.0))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if !medallions {
                memory.push(format!(
                    "{}: {}",
                    gi.name(),
                    r.gi_parts
                        .iter()
                        .map(|(name, bytes)| format!(
                            "{name} {:.1} MiB",
                            *bytes as f64 / f64::from(1 << 20)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                let gi_startup: Vec<(&str, f64)> = r
                    .startup_us
                    .iter()
                    .filter(|(name, _)| gi.passes().iter().any(|p| name.starts_with(p)))
                    .map(|(name, us)| (*name, *us))
                    .collect();
                startup.push(format!(
                    "{}: {} frames to ready ({}); GPU time before the first timed frame: {}",
                    gi.name(),
                    r.gi_settle.0,
                    match gi {
                        Gi::Probes => "the volume converged",
                        Gi::Brixelizer => "the warm-up",
                    },
                    gi_startup
                        .iter()
                        .map(|(name, us)| format!("{name} {:.1} ms", us / 1000.0))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            frame_png(&r, &dir.join(format!("{label}_{}_frame.png", gi.tag())));
            irradiance_png(
                &r,
                &dir.join(format!("{label}_{}_irradiance.png", gi.tag())),
            );
        }
    }
    println!("\nGI passes, ms:");
    for line in breakdown {
        println!("- {line}");
    }
    println!("\nGI memory (courtyard):");
    for line in memory {
        println!("- {line}");
    }
    println!("\nStart-up (courtyard):");
    for line in startup {
        println!("- {line}");
    }

    // Brixelizer GI from its first frame: the distance field is built in the first eight,
    // the GI's caches fill from then on (probe GI converges before its first frame shown).
    let base = view("gi_bench_buildup", false);
    let r = run(
        &Shot {
            size: [640, 360],
            brixelizer_warmup: Some(0),
            captures: curve_frames,
            ..Gi::Brixelizer.apply(base)
        },
        0,
    );
    let lum: Vec<f32> = r.gi_means.iter().map(|&m| luminance(m)).collect();
    let end = lum.last().copied().unwrap_or(0.0);
    let first_above = |fraction: f32| {
        lum.iter()
            .position(|&l| l >= fraction * end)
            .map_or_else(|| format!("> {curve_frames}"), |k| (k + 1).to_string())
    };
    println!(
        "\nBrixelizer GI from its first frame (courtyard, 640 × 360): frames to 50 % of its \
         value after {curve_frames} frames: {}, to 90 %: {}",
        first_above(0.5),
        first_above(0.9)
    );
    let curve: Vec<String> = lum
        .iter()
        .step_by(16)
        .map(|l| format!("{:.3}", l / end.max(1e-9)))
        .collect();
    println!("  every 16th frame: {}", curve.join(" "));

    println!("\nResponse: the sun alone (a black sky), out after the first capture; 640 × 360.\n");
    println!(
        "| GI | mean irradiance, sun on | frames to 50 % | frames to 10 % | after {curve_frames} frames |"
    );
    println!("|---|---:|---:|---:|---:|");
    for gi in [Gi::Probes, Gi::Brixelizer] {
        let base = view("gi_bench_response", false);
        let shot = gi.apply(Shot {
            size: [640, 360],
            sky_intensity: 0.0,
            sun: engine_render::forward::DirectionalLight {
                ambient: Vec3::ZERO,
                ..base.sun
            },
            captures: 1 + curve_frames,
            sun_off_after: Some(1),
            brixelizer_warmup: Some(BENCH_WARMUP),
            ..base
        });
        let r = run(&shot, 0);
        let lum: Vec<f32> = r.gi_means.iter().map(|&m| luminance(m)).collect();
        let on = lum[0];
        let first_below = |fraction: f32| {
            lum.iter()
                .skip(1)
                .position(|&l| l <= fraction * on)
                .map_or_else(|| format!("> {curve_frames}"), |k| (k + 1).to_string())
        };
        println!(
            "| {} | {on:.4} | {} | {} | {:.1} % |",
            gi.name(),
            first_below(0.5),
            first_below(0.1),
            100.0 * lum.last().copied().unwrap_or(0.0) / on.max(1e-9),
        );
        let curve: Vec<String> = lum
            .iter()
            .step_by(16)
            .map(|l| format!("{:.3}", l / on.max(1e-9)))
            .collect();
        println!("  every 16th frame: {}", curve.join(" "));
    }
    println!("\nAdapter: {adapter}");
}
