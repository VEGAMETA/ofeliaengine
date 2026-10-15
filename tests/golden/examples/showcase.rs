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
//! [--probes x,y,z] [--intel] [--vsm] [--vsm-bias B] [--eye x,y,z --target x,y,z] [--tag TAG]`
//! (`ENGINE_ADAPTER=radeon` for the iGPU; DLSS needs Streamline's DLLs in
//! `ENGINE_STREAMLINE_DIR`; `VK_LOADER_LAYERS_DISABLE=~implicit~`).
//!
//! - `--integrated`: the frame renderer's defaults for integrated GPUs (TAAU from 67 %,
//!   ambient occlusion Low) instead of the scenes' maximum (TAA at the output size,
//!   ambient occlusion High).
//! - `--upscaler`, `--quality`: another temporal upscaler, or another render size.
//! - `--off`: without those features, to see what each costs and gives.
//! - `--intel`: Intel's Sponza (2022; `tools/fetch-test-assets.ps1 -Only IntelSponza`)
//!   instead of Crytek's, with the cook's detail levels and meshlet culling, as a game's
//!   pack and the discrete class have them.
//! - `--vsm`: the sun's virtual shadow pages for opaque surfaces instead of its cascades
//!   (ADR 0093 step 3); `--vsm-bias`: their level bias (-1: texels half as wide).
//! - `--eye`, `--target`: another camera (a close-up); `--tag`: added to the file names.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::Vec3;
use engine_frame::{Antialiasing, DeviceClass, Quality, RenderDefaults};
use engine_golden::golden::write_png;
use engine_golden::scene::{RenderOptions, Shot, Skip, Source, render, sponza_day, sponza_night};
use engine_render::vsm::VsmSettings;
use std::path::{Path, PathBuf};

/// Intel's Sponza: its main package (the curtains and ivy are packages of their own).
const INTEL_SPONZA: &str = "IntelSponza/main_sponza/NewSponza_Main_glTF_003.gltf";

fn vec3(value: &str) -> Vec3 {
    let v: Vec<f32> = value.split(',').map(|n| n.parse().unwrap()).collect();
    Vec3::new(v[0], v[1], v[2])
}

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
    let mut intel = false;
    let mut vsm = false;
    let mut vsm_bias = 0.0;
    let mut eye: Option<Vec3> = None;
    let mut target: Option<Vec3> = None;
    let mut extra_tag = String::new();
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
            "--intel" => intel = true,
            "--vsm" => vsm = true,
            "--vsm-bias" => vsm_bias = args.next().unwrap().parse().unwrap(),
            "--eye" => eye = Some(vec3(&args.next().unwrap())),
            "--target" => target = Some(vec3(&args.next().unwrap())),
            "--tag" => extra_tag = args.next().unwrap(),
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
        if intel {
            shot.source = Source::Gltf(PathBuf::from(INTEL_SPONZA));
            shot.lods = true;
            shot.meshlets = true;
        }
        shot.virtual_shadows = vsm.then_some(VsmSettings {
            level_bias: vsm_bias,
            ..VsmSettings::default()
        });
        if eye.is_some() {
            shot.eye = eye;
            shot.target = target;
        }
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
        tag.push_str(if vsm { "_vsm" } else { "_cascades" });
        if vsm_bias != 0.0 {
            // Kept through the file name's filter: -0.5 as m0p5.
            let bias = vsm_bias.to_string().replace('-', "m").replace('.', "p");
            tag.push_str(&format!("_bias{bias}"));
        }
        if !extra_tag.is_empty() {
            tag.push('_');
            tag.push_str(&extra_tag);
        }
        let tag: String = tag
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let path = dir.join(format!(
            "{}sponza_{}_{}x{}{tag}.png",
            if intel { "intel_" } else { "" },
            shot.name,
            size[0],
            size[1]
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
        // The sun's shadow: its cascades' culls and draws, or its pages' bookkeeping,
        // culls, draws and visibility.
        let sum = |prefix: &str| -> f64 {
            rendered
                .pass_us
                .iter()
                .filter(|(name, _)| name.starts_with(prefix))
                .map(|(_, us)| us / 1000.0)
                .sum()
        };
        println!(
            "  the sun's shadow (ms): cascades {:.2}, pages {:.2}",
            sum("shadow"),
            sum("vsm")
        );
    }
}
