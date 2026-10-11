//! Frame generation against the truth (ADR 0091's measurement protocol, item 4): the frame
//! FSR 3.1 generated between two real frames of Sponza, the camera moving, against the
//! frame rendered for real at that moment, and against the real frame a display shows
//! there without frame generation. Over the whole image and in each part of the UI (a HUD,
//! a panel in the world, a panel on the camera), drawn over every frame with its camera.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::Vec3;
use engine_frame::Antialiasing;
use engine_golden::flip::{DEFAULT_PPD, FlipResult, flip, srgb_to_linear};
use engine_golden::golden::{output_dir, write_png};
use engine_golden::scene::{RenderOptions, Rendered, Shot, Skip, Source, render, vendor_name};
use std::path::PathBuf;

fn run(shot: &Shot) -> Option<Rendered> {
    match render(
        shot,
        RenderOptions {
            validation: true,
            timed_frames: 0,
        },
    ) {
        Ok(r) => Some(r),
        Err(Skip::NoGpu(e)) => {
            println!("{}: skipped, no GPU ({e})", shot.name);
            None
        }
        Err(Skip::NoScene(path)) => {
            println!("{}: skipped, {} missing", shot.name, path.display());
            None
        }
        Err(Skip::NoDlss(e) | Skip::Unsupported(e)) => {
            println!("{}: skipped, {e}", shot.name);
            None
        }
    }
}

/// Sponza without antialiasing (no history to tell the shots apart), the UI on, the
/// camera moving `motion` a frame, captured `after` frames into the motion.
fn sponza(name: &'static str, motion: Vec3, after: u32, frame_generation: bool) -> Shot {
    Shot {
        antialiasing: Antialiasing::OFF,
        camera_motion: motion,
        capture_after: Some(after),
        frame_generation,
        ui: true,
        ..Shot::new(name, Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
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

/// The mean of FLIP's error over a rectangle `[x0, y0, x1, y1]`.
fn mean_in(result: &FlipResult, [x0, y0, x1, y1]: [u32; 4]) -> f32 {
    let (mut sum, mut n) = (0.0f64, 0u64);
    for y in y0..y1 {
        for x in x0..x1 {
            sum += f64::from(result.error[y as usize * result.width + x as usize]);
            n += 1;
        }
    }
    (sum / n.max(1) as f64) as f32
}

/// The real frames come every 4 cm of the camera's diagonal motion (5.7 cm); the frame
/// generated between real frames 7 and 8 sees the scene from half-way, where the truth
/// renders it as its frame 15 at half the motion. Without frame generation a display shows
/// a real frame there: real frame 8 (frame 7 is as far).
#[test]
fn generated_frames_are_nearer_the_truth_than_real_ones() {
    let motion = Vec3::new(0.04, 0.0, 0.04);
    let Some(generated) = run(&sponza("fg_generated", motion, 8, true)) else {
        return;
    };
    let Some(truth) = run(&sponza("fg_truth", motion * 0.5, 15, false)) else {
        return;
    };
    let Some(held) = run(&sponza("fg_held", motion, 8, false)) else {
        return;
    };
    let (w, h) = (truth.width as usize, truth.height as usize);
    let reference = linear(&truth.rgba8);
    let against = |r: &Rendered| flip(&reference, &linear(&r.rgba8), w, h, DEFAULT_PPD);
    let (generated_flip, held_flip) = (against(&generated), against(&held));
    // The three frames and both error maps, for looking at them.
    let dir = output_dir(&vendor_name(truth.vendor_id));
    let _ = std::fs::create_dir_all(&dir);
    let rgb = |rgba8: &[u8]| -> Vec<u8> {
        rgba8
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect()
    };
    for (name, image) in [
        ("fg_generated", rgb(&generated.rgba8)),
        ("fg_truth", rgb(&truth.rgba8)),
        ("fg_held", rgb(&held.rgba8)),
        ("fg_generated_flip", generated_flip.heatmap_rgb8()),
        ("fg_held_flip", held_flip.heatmap_rgb8()),
    ] {
        let _ = write_png(
            &dir.join(format!("{name}.png")),
            truth.width,
            truth.height,
            &image,
        );
    }
    println!(
        "Frame generation against the truth on {} ({w}×{h}), mean FLIP:\n",
        truth.adapter
    );
    println!("| where | generated | real frame held |\n|---|---:|---:|");
    println!(
        "| the image | {:.4} | {:.4} |",
        generated_flip.mean(),
        held_flip.mean()
    );
    // Each frame's UI regions as its own camera saw them: the truth's and the generated
    // frame's camera are the same.
    assert_eq!(generated.ui_regions, truth.ui_regions);
    let mut regions = Vec::new();
    for (name, rect) in &truth.ui_regions {
        let (g, r) = (mean_in(&generated_flip, *rect), mean_in(&held_flip, *rect));
        println!("| {name} {rect:?} | {g:.4} | {r:.4} |");
        regions.push((*name, g, r));
    }
    println!();
    assert!(
        generated_flip.mean() < held_flip.mean(),
        "the generated frame ({:.4}) should be nearer the truth than the real frame ({:.4})",
        generated_flip.mean(),
        held_flip.mean()
    );
    // The panel in the world is drawn with the generated frame's camera, where the truth
    // has it: no worse there than the real frame, whose camera put it elsewhere.
    let (_, g, r) = regions
        .iter()
        .find(|(name, _, _)| *name == "world panel")
        .copied()
        .unwrap();
    assert!(
        g <= r,
        "the world panel: generated {g:.4} against the real frame's {r:.4}"
    );
}
