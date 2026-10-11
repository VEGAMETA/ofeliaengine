//! Sponza by day and by night with everything on (`scene::sponza_day`,
//! `scene::sponza_night`): each rendered through the golden test bed, its frame saved
//! and its GPU time measured.
//!
//! For each scene: the GPU's frame end to end (median of the timed frames) as
//! milliseconds and frames per second, the CPU's share of declaring and recording a
//! frame, and the costliest passes. The frames go to `target/showcase/<vendor>/`.
//!
//! `cargo run -p engine_golden --release --example showcase [-- --size 2560x1440]
//! [--integrated] [--upscaler taau|fsr|dlss] [--quality native|quality|balanced|performance|ultra]
//! [--off gi,ssr,probes,ao,fog,contact,particles,lights,taa,bloom] [--only day|night]
//! [--probes x,y,z]` (`ENGINE_ADAPTER=radeon` for the iGPU; DLSS needs Streamline's DLLs
//! in `ENGINE_STREAMLINE_DIR`; `VK_LOADER_LAYERS_DISABLE=~implicit~`).
//!
//! - `--integrated`: the frame renderer's defaults for integrated GPUs (TAAU from 67 %,
//!   ambient occlusion Low) instead of the scenes' maximum (TAA at the output size,
//!   ambient occlusion High).
//! - `--upscaler`, `--quality`: another temporal upscaler, or another render size.
//! - `--off`: without those features, to see what each costs and gives.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_frame::{Antialiasing, DeviceClass, Quality, RenderDefaults};
use engine_golden::golden::write_png;
use engine_golden::scene::{RenderOptions, Shot, Skip, render, sponza_day, sponza_night};
use std::path::Path;

/// Frames timed after the capture.
const TIMED: usize = 200;

fn main() {
    let mut size = [1920, 1080];
    let mut integrated = false;
    let mut off = String::new();
    let mut only: Option<String> = None;
    let mut probes: Option<[u32; 3]> = None;
    let mut upscaler: Option<Antialiasing> = None;
    let mut quality: Option<Quality> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--size" => {
                let value = args.next().unwrap();
                let (w, h) = value.split_once('x').unwrap();
                size = [w.parse().unwrap(), h.parse().unwrap()];
            }
            "--integrated" => integrated = true,
            "--upscaler" => {
                upscaler = Some(match args.next().unwrap().as_str() {
                    "taau" => Antialiasing::TAAU,
                    "fsr" => Antialiasing::FSR,
                    "dlss" => Antialiasing::DLSS,
                    other => panic!("unknown upscaler {other}"),
                });
            }
            "--quality" => {
                quality = Some(match args.next().unwrap().as_str() {
                    "native" => Quality::Native,
                    "quality" => Quality::Quality,
                    "balanced" => Quality::Balanced,
                    "performance" => Quality::Performance,
                    "ultra" => Quality::UltraPerformance,
                    other => panic!("unknown quality {other}"),
                });
            }
            "--off" => off = args.next().unwrap(),
            "--only" => only = args.next(),
            "--probes" => {
                let v: Vec<u32> = args
                    .next()
                    .unwrap()
                    .split(',')
                    .map(|n| n.parse().unwrap())
                    .collect();
                probes = Some([v[0], v[1], v[2]]);
            }
            _ => panic!("unknown argument {arg}"),
        }
    }
    let mut shots: [Shot; 2] = [sponza_day("day", size), sponza_night("night", size)];
    let defaults = RenderDefaults::for_class(DeviceClass::Integrated);
    for shot in &mut shots {
        if std::env::var("SHOWCASE_NO_GI").is_ok() {
            shot.gi = None;
        }
        for feature in off.split(',').filter(|f| !f.is_empty()) {
            match feature {
                "gi" => shot.gi = None,
                "ssr" => shot.ssr = None,
                "probes" => shot.probe_grid = None,
                "ao" => shot.ao = None,
                "fog" => shot.fog = None,
                "contact" => shot.contact_shadows = false,
                "particles" => shot.particles = false,
                "lights" => shot.lights.clear(),
                "taa" => shot.antialiasing = Antialiasing::OFF,
                "bloom" => shot.bloom = false,
                _ => panic!("unknown feature {feature}"),
            }
        }
        if let Some(grid) = probes {
            shot.probe_grid = Some(grid);
        }
        if integrated {
            shot.ao = defaults.ambient_occlusion;
            shot.antialiasing = defaults.antialiasing;
            shot.quality = defaults.quality;
        }
        if let Some(upscaler) = upscaler {
            shot.antialiasing = upscaler;
        }
        if let Some(quality) = quality {
            shot.quality = quality;
        }
    }
    for shot in shots
        .iter()
        .filter(|s| only.as_deref().is_none_or(|o| o == s.name))
    {
        let rendered = match render(
            shot,
            RenderOptions {
                validation: false,
                timed_frames: TIMED,
            },
        ) {
            Ok(r) => r,
            Err(Skip::Unsupported(e) | Skip::NoDlss(e)) => {
                println!("{}: skipped, {e}", shot.name);
                continue;
            }
            Err(e) => panic!("{}: {e:?}", shot.name),
        };
        let vendor = match rendered.vendor_id {
            0x10de => "nvidia",
            0x1002 => "amd",
            0x8086 => "intel",
            _ => "other",
        };
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/showcase")
            .join(vendor);
        std::fs::create_dir_all(&dir).unwrap();
        let rgb: Vec<u8> = rendered
            .rgba8
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let mut tag = String::new();
        if integrated {
            tag.push_str("_integrated");
        }
        if upscaler.is_some() || quality.is_some() {
            tag.push_str(&format!("_{:?}_{:?}", rendered.antialiasing, shot.quality));
        }
        let tag: String = tag
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let path = dir.join(format!(
            "sponza_{}_{}x{}{tag}.png",
            shot.name, size[0], size[1]
        ));
        write_png(&path, rendered.width, rendered.height, &rgb).unwrap();
        let ms = rendered.frame_us / 1000.0;
        println!(
            "{} on {} at {}x{}, {:?} {:?}: GPU frame {ms:.2} ms = {:.0} fps (async compute {}); {}",
            shot.name,
            rendered.adapter,
            size[0],
            size[1],
            rendered.antialiasing,
            shot.quality,
            1000.0 / ms,
            if rendered.async_compute { "on" } else { "off" },
            path.display()
        );
        let mut passes: Vec<(&&str, &f64)> = rendered.pass_us.iter().collect();
        passes.sort_by(|a, b| b.1.total_cmp(a.1));
        let line: Vec<String> = passes
            .iter()
            .take(10)
            .map(|(name, us)| format!("{name} {:.2}", **us / 1000.0))
            .collect();
        println!("  costliest passes (ms): {}", line.join(", "));
    }
}
