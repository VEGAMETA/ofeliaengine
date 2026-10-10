//! Every antialiasing and scaling mode (ADR 0075) through the whole frame under the
//! validation layer with synchronization validation, with every feature that reads or
//! writes the scene's targets on: sky and atmosphere, height fog, ambient occlusion,
//! contact shadows, local lights with shadows, subsurface scattering, reflection probes
//! and screen-space reflections, transparency, occlusion culling. And MSAA with every
//! screen-space effect on keeps its edges: no halo where the effects meet a silhouette.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::Vec3;
use engine_frame::{
    Antialiasing, Msaa, PostAntialiasing, Quality, SmaaQuality, Upscale, UpscalerKind,
};
use engine_golden::flip::srgb_to_linear;
use engine_golden::scene::{
    RenderOptions, Rendered, Shot, Skip, Source, procedural_lights, render,
};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::reflections::SsrSettings;
use std::path::PathBuf;

fn run(shot: &Shot, validation: bool) -> Option<Rendered> {
    match render(
        shot,
        RenderOptions {
            validation,
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
            println!("{} ({}): skipped, {e}", shot.name, shot.antialiasing);
            None
        }
    }
}

/// The procedural scene with every feature on.
fn everything(name: &'static str, size: [u32; 2]) -> Shot {
    Shot {
        size,
        eye: Some(Vec3::new(0.4, 1.9, 4.8)),
        target: Some(Vec3::new(0.0, 1.0, -1.0)),
        lights: procedural_lights(),
        local_shadows: true,
        ao: Some(AoSettings::default()),
        contact_shadows: true,
        subsurface: true,
        probe_grid: Some([2, 1, 2]),
        ssr: Some(SsrSettings::default()),
        glossy_floor: true,
        atmosphere: Some(Atmosphere::earth()),
        fog: Some(HeightFog {
            density: 0.08,
            base_height: 0.0,
            falloff: 0.6,
            ..HeightFog::default()
        }),
        occlusion_culling: true,
        temporal_frames: 8,
        ..Shot::new(name, Source::Procedural)
    }
}

fn luma_spread(r: &Rendered) -> u32 {
    let lumas = r
        .rgba8
        .chunks_exact(4)
        .map(|p| u32::from(p[0]) + 2 * u32::from(p[1]) + u32::from(p[2]));
    let (lo, hi) = lumas.fold((u32::MAX, 0), |(lo, hi), l| (lo.min(l), hi.max(l)));
    hi.saturating_sub(lo)
}

#[test]
fn every_mode_renders_with_every_feature() {
    let smaa = PostAntialiasing::SMAA;
    let modes: Vec<(Antialiasing, Quality)> = vec![
        (Antialiasing::OFF, Quality::Native),
        (
            Antialiasing::spatial(Upscale::Nearest),
            Quality::Performance,
        ),
        (Antialiasing::OFF, Quality::Performance),
        (
            Antialiasing::spatial(Upscale::Integer),
            Quality::Performance,
        ),
        (Antialiasing::spatial(Upscale::Fsr1), Quality::Quality),
        (Antialiasing::spatial(Upscale::Fsr1), Quality::Native),
        (Antialiasing::spatial(Upscale::CatmullRom), Quality::Quality),
        (
            Antialiasing::spatial(Upscale::Lanczos3),
            Quality::Performance,
        ),
        (Antialiasing::TAAU, Quality::Native),
        (Antialiasing::TAAU, Quality::Quality),
        (Antialiasing::FSR, Quality::Quality),
        (Antialiasing::DLSS, Quality::Quality),
        (Antialiasing::FXAA, Quality::Native),
        (Antialiasing::MLAA, Quality::Native),
        (
            Antialiasing::post(PostAntialiasing::Smaa(SmaaQuality::Low)),
            Quality::Native,
        ),
        (
            Antialiasing::post(PostAntialiasing::Smaa(SmaaQuality::Medium)),
            Quality::Native,
        ),
        (Antialiasing::SMAA_1X, Quality::Native),
        (
            Antialiasing::post(PostAntialiasing::Smaa(SmaaQuality::Ultra)),
            Quality::Native,
        ),
        (Antialiasing::SMAA_T2X, Quality::Native),
        (Antialiasing::SMAA_S2X, Quality::Native),
        (Antialiasing::SMAA_4X, Quality::Native),
        (Antialiasing::msaa(Msaa::X2), Quality::Native),
        (Antialiasing::msaa(Msaa::X4), Quality::Native),
        (Antialiasing::msaa(Msaa::X8), Quality::Native),
        (
            Antialiasing::msaa(Msaa::X4).with_post(smaa),
            Quality::Native,
        ),
        (
            Antialiasing::msaa(Msaa::X2).with_post(PostAntialiasing::Fxaa),
            Quality::Native,
        ),
        (Antialiasing::TAAU.with_samples(Msaa::X4), Quality::Quality),
        (
            Antialiasing::spatial(Upscale::Fsr1).with_samples(Msaa::X4),
            Quality::Quality,
        ),
        (
            Antialiasing::spatial(Upscale::Integer).with_post(smaa),
            Quality::Performance,
        ),
        (
            Antialiasing::spatial(Upscale::Lanczos3)
                .with_samples(Msaa::X2)
                .with_post(smaa),
            Quality::Quality,
        ),
        (
            Antialiasing::spatial(Upscale::CatmullRom).with_post(PostAntialiasing::Fxaa),
            Quality::Quality,
        ),
        (
            Antialiasing::temporal(UpscalerKind::Taau).with_post(PostAntialiasing::Fxaa),
            Quality::Quality,
        ),
    ];
    for (antialiasing, quality) in modes {
        let shot = Shot {
            antialiasing,
            quality,
            ..everything("every feature", [256, 144])
        };
        let Some(r) = run(&shot, true) else {
            continue;
        };
        let spread = luma_spread(&r);
        println!("{antialiasing} at {quality:?}: rendered, luma spread {spread}");
        assert!(spread > 100, "{antialiasing}: a blank image");
    }
}

/// Texture streaming with feedback (ADR 0076) under MSAA: the feedback pass redraws the
/// view's culled lists into its own single-sampled target, whatever the view's samples.
#[test]
fn texture_feedback_with_msaa() {
    let shot = Shot {
        texture_budget: Some(64 << 20),
        antialiasing: Antialiasing::msaa(Msaa::X4).with_post(PostAntialiasing::SMAA),
        ..Shot::new(
            "sponza streamed msaa",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    };
    if let Some(r) = run(&shot, true) {
        assert!(luma_spread(&r) > 100);
    }
}

/// Skinned meshes' motion pass into multisampled targets (with TAA, which reads it).
#[test]
fn skinned_motion_with_msaa() {
    let shot = Shot {
        animation: Some((0, 0.4)),
        animation_rate: 1.0 / 60.0,
        antialiasing: Antialiasing::TAAU.with_samples(Msaa::X4),
        temporal_frames: 8,
        ..Shot::new("fox msaa", Source::Gltf(PathBuf::from("Fox/Fox.gltf")))
    };
    if let Some(r) = run(&shot, true) {
        assert!(luma_spread(&r) > 100);
    }
}

fn linear(rgba8: &[u8]) -> Vec<[f32; 3]> {
    rgba8
        .chunks_exact(4)
        .map(|p| [0, 1, 2].map(|i| srgb_to_linear(f32::from(p[i]) / 255.0)))
        .collect()
}

/// A `k × k` box filter in linear light.
fn downsample(r: &Rendered, k: usize) -> Vec<[f32; 3]> {
    let (w, h) = (r.width as usize, r.height as usize);
    let full = linear(&r.rgba8);
    let mut out = Vec::with_capacity(w / k * h / k);
    for y in 0..h / k {
        for x in 0..w / k {
            let mut c = [0.0f32; 3];
            for dy in 0..k {
                for dx in 0..k {
                    let p = full[(k * y + dy) * w + k * x + dx];
                    for i in 0..3 {
                        c[i] += p[i] / (k * k) as f32;
                    }
                }
            }
            out.push(c);
        }
    }
    out
}

/// Pixels on a silhouette: the reference's luma changes sharply to a neighbour.
fn edges(reference: &[[f32; 3]], w: usize, h: usize) -> Vec<bool> {
    let luma = |c: &[f32; 3]| (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]).sqrt();
    let mut mask = vec![false; w * h];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let l = luma(&reference[y * w + x]);
            let jump = [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .map(|(dx, dy)| {
                    let n = &reference[(y as i32 + dy) as usize * w + (x as i32 + dx) as usize];
                    (luma(n) - l).abs()
                })
                .fold(0.0f32, f32::max);
            mask[y * w + x] = jump > 0.08;
        }
    }
    mask
}

/// Display-encoded (sRGB) value, 0–255.
fn encoded(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    255.0
        * if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
}

/// Edge pixels where `test` is closer to `reference` than `base` by more than 10 of 255
/// (display-encoded, mean of the channels), and those where it is farther.
fn closer_farther(
    reference: &[[f32; 3]],
    base: &[[f32; 3]],
    test: &[[f32; 3]],
    mask: &[bool],
) -> (usize, usize) {
    let error = |a: &[f32; 3], r: &[f32; 3]| {
        (0..3)
            .map(|i| (encoded(a[i]) - encoded(r[i])).abs())
            .sum::<f32>()
            / 3.0
    };
    let (mut closer, mut farther) = (0, 0);
    for (((r, b), t), &m) in reference.iter().zip(base).zip(test).zip(mask) {
        if m {
            let d = error(t, r) - error(b, r);
            if d < -10.0 {
                closer += 1;
            } else if d > 10.0 {
                farther += 1;
            }
        }
    }
    (closer, farther)
}

fn edge_error(reference: &[[f32; 3]], test: &[[f32; 3]], mask: &[bool]) -> f64 {
    let (mut sum, mut n) = (0.0f64, 0usize);
    for ((r, t), &m) in reference.iter().zip(test).zip(mask) {
        if m {
            for i in 0..3 {
                sum += f64::from((r[i] - t[i]).abs());
            }
            n += 1;
        }
    }
    sum / (3 * n.max(1)) as f64
}

/// MSAA 4×'s gain at the edges (1 − its error / one sample's), against a 4 × 4
/// supersampled reference: `base`'s own, or `geometry`'s (the silhouettes of the scene
/// without effects, so that reflections' and subsurface light's own detail, which no
/// MSAA antialiases, does not count as an edge). Returns the gain and the edges used.
struct EdgeGain {
    /// 1 − MSAA's error / one sample's.
    gain: f64,
    /// Edge pixels MSAA brings closer to the reference, and those it takes away.
    closer: usize,
    farther: usize,
    mask: Vec<bool>,
}

fn edge_gain(label: &str, base: &Shot, geometry: Option<&[bool]>) -> Option<EdgeGain> {
    let k = 4usize;
    let size = base.size;
    let reference = run(
        &Shot {
            size: [size[0] * k as u32, size[1] * k as u32],
            ..base.clone()
        },
        false,
    )?;
    let reference = downsample(&reference, k);
    let mask = match geometry {
        Some(mask) => mask.to_vec(),
        None => edges(&reference, size[0] as usize, size[1] as usize),
    };
    let one = run(base, true)?;
    let msaa = run(
        &Shot {
            antialiasing: Antialiasing::msaa(Msaa::X4),
            ..base.clone()
        },
        true,
    )?;
    let (e1, e4) = (
        edge_error(&reference, &linear(&one.rgba8), &mask),
        edge_error(&reference, &linear(&msaa.rgba8), &mask),
    );
    let gain = 1.0 - e4 / e1;
    let (closer, farther) =
        closer_farther(&reference, &linear(&one.rgba8), &linear(&msaa.rgba8), &mask);
    if let Ok(dir) = std::env::var("ENGINE_AA_DUMP") {
        let save = |name: &str, rgba: &[u8]| {
            let rgb: Vec<u8> = rgba
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            let path = PathBuf::from(&dir).join(format!("{label}_{name}.png"));
            engine_golden::golden::write_png(&path, size[0], size[1], &rgb).unwrap();
        };
        save("one", &one.rgba8);
        save("msaa", &msaa.rgba8);
        let encode = |c: f32| {
            let c = c.clamp(0.0, 1.0);
            let e = if c <= 0.003_130_8 {
                c * 12.92
            } else {
                1.055 * c.powf(1.0 / 2.4) - 0.055
            };
            (e * 255.0).round() as u8
        };
        let reference_rgba: Vec<u8> = reference
            .iter()
            .flat_map(|c| [encode(c[0]), encode(c[1]), encode(c[2]), 255])
            .collect();
        save("reference", &reference_rgba);
    }
    println!(
        "{label}: {} edge pixels, error 1 sample {e1:.4}, MSAA 4× {e4:.4}: {:.0} % less; {closer} pixels closer, {farther} farther",
        mask.iter().filter(|&&m| m).count(),
        gain * 100.0
    );
    Some(EdgeGain {
        gain,
        closer,
        farther,
        mask,
    })
}

/// Each effect alone (for looking into a halo).
#[test]
#[ignore = "diagnostic"]
fn msaa_edge_gain_per_effect() {
    let size = [320u32, 180];
    let plain = Shot {
        size,
        eye: Some(Vec3::new(0.4, 1.9, 4.8)),
        target: Some(Vec3::new(0.0, 1.0, -1.0)),
        lights: procedural_lights(),
        bloom: false,
        ..Shot::new("halo", Source::Procedural)
    };
    let Some(EdgeGain { mask, .. }) = edge_gain("none", &plain, None) else {
        return;
    };
    let mask = Some(&mask[..]);
    edge_gain(
        "ao",
        &Shot {
            ao: Some(AoSettings::default()),
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "contact",
        &Shot {
            contact_shadows: true,
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "local shadows",
        &Shot {
            local_shadows: true,
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "subsurface",
        &Shot {
            subsurface: true,
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "glossy floor",
        &Shot {
            glossy_floor: true,
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "probes",
        &Shot {
            glossy_floor: true,
            probe_grid: Some([2, 1, 2]),
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "ssr",
        &Shot {
            glossy_floor: true,
            ssr: Some(SsrSettings::default()),
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "atmosphere",
        &Shot {
            atmosphere: Some(Atmosphere::earth()),
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "fog",
        &Shot {
            fog: Some(HeightFog {
                density: 0.08,
                base_height: 0.0,
                falloff: 0.6,
                ..HeightFog::default()
            }),
            ..plain.clone()
        },
        mask,
    );
    edge_gain(
        "occlusion culling",
        &Shot {
            occlusion_culling: true,
            ..plain.clone()
        },
        mask,
    );
}

/// MSAA with every screen-space effect on (ADR 0075): ambient occlusion, contact
/// shadows, reflections and subsurface scattering read the nearest sample's depth, fog
/// and the atmosphere run per sample. If an effect spread one side of a silhouette over
/// the other (a halo), MSAA's samples would no longer bring the edge pixels towards the
/// supersampled reference. So: against a 4 × 4 supersampled reference, MSAA 4× must cut
/// the edge pixels' error of one sample by much the same share with every effect on as
/// with none.
#[test]
fn msaa_shows_no_edge_halos_with_screen_space_effects() {
    let size = [320u32, 180];
    let plain = Shot {
        size,
        eye: Some(Vec3::new(0.4, 1.9, 4.8)),
        target: Some(Vec3::new(0.0, 1.0, -1.0)),
        lights: procedural_lights(),
        bloom: false,
        ..Shot::new("halo", Source::Procedural)
    };
    let Some(plain) = edge_gain("no effects", &plain, None) else {
        return;
    };
    let effects = Shot {
        bloom: false,
        temporal_frames: 1,
        ..everything("halo", size)
    };
    let Some(effects) = edge_gain("every effect", &effects, Some(&plain.mask)) else {
        return;
    };
    assert!(
        plain.gain > 0.25,
        "MSAA 4× barely helps the edges: {:.3}",
        plain.gain
    );
    // Ambient occlusion and contact shadows are one value per pixel (the nearest
    // sample's), so the samples behind a silhouette get the front's (under the spheres,
    // where they touch the floor; ADR 0075 measures each effect), and the effects' own
    // detail changes what the reference shows at some edges: MSAA's gain is smaller there.
    // A halo would instead take a ring of pixels around every silhouette away from the
    // reference.
    assert!(
        effects.gain > 0.4 * plain.gain,
        "with every effect on MSAA 4× keeps {:.0} % of its edge gain",
        100.0 * effects.gain / plain.gain
    );
    assert!(
        effects.closer >= 2 * effects.farther,
        "with every effect on MSAA 4× takes {} edge pixels away from the reference and brings {} closer: halos",
        effects.farther,
        effects.closer
    );
}
