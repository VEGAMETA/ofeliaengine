//! The frame renderer (ADR 0072) under the validation layer: history dropped on a camera
//! cut (no ghost of the previous view), the texture LOD bias the upscaler asks for is
//! applied, settings changes start the history anew, an unavailable upscaler falls back
//! to TAAU, and the previous color reaches the scene hook; and every feature and mode fits
//! a main thread's 1 MiB stack; a streamed level fades in on screen. `framegen`: frame
//! generation with the UI layer (ADR 0091).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "frame/framegen.rs"]
mod framegen;
#[path = "frame/hot_reload.rs"]
mod hot_reload;

use common::{Bed, camera};
use engine_core::math::Vec3;
use engine_frame::{
    Antialiasing, FrameError, InvalidAntialiasing, Msaa, PostAntialiasing, PrepareContext, Quality,
    Upscale, UpscalerKind, View, ViewSettings,
};
use engine_render::ao::AoSettings;
use engine_render::dof::DofSettings;
use engine_render::forward::LocalShadowSettings;
use engine_render::grading::ColorGrading;
use engine_render::material::{MaterialDesc, TextureRef};
use engine_render::reflections::SsrSettings;
use engine_render::subsurface::SubsurfaceProfile;
use engine_render::texture_pool::{MipFade, TextureFormat, TextureLevels};

fn differing(a: &[u8], b: &[u8]) -> usize {
    a.chunks(4).zip(b.chunks(4)).filter(|(x, y)| x != y).count()
}

/// After a cut, a view's first frame is exactly a fresh view's first frame: nothing of
/// the previous picture survives in the history. The cut here is a change of level under
/// a still camera (a jump of the camera alone mostly reprojects off screen): without the
/// cut, the old level's picture shows through.
#[test]
fn a_cut_drops_the_history() {
    let Some(mut bed) = Bed::new((96, 64), true) else {
        return;
    };
    let red = bed.solid([0.9, 0.1, 0.1], [4.0, 0.2, 0.2]);
    let grey = bed.solid([0.5, 0.5, 0.5], [0.0; 3]);
    let blue = bed.solid([0.1, 0.2, 0.9], [0.0; 3]);
    // Level A: a glowing red wall. Level B: a floor and blue blocks.
    let level_a = vec![bed.cube_at(red, Vec3::new(30.0, 30.0, 0.2), Vec3::new(0.0, 0.0, -3.0))];
    let level_b = vec![
        bed.cube_at(grey, Vec3::new(10.0, 0.2, 10.0), Vec3::new(0.0, -0.1, 0.0)),
        bed.cube_at(blue, Vec3::splat(1.0), Vec3::new(-1.0, 0.5, 0.0)),
        bed.cube_at(blue, Vec3::new(0.6, 1.5, 0.6), Vec3::new(1.2, 0.75, -0.5)),
    ];
    let b = camera(Vec3::new(0.5, 2.0, 5.0), Vec3::new(0.0, 0.5, 0.0), false);
    let settings = ViewSettings {
        antialiasing: Antialiasing::TAAU,
        bloom: false,
        ..ViewSettings::default()
    };
    // TAA has 8 jitter phases: after 16 frames of A, B's frame has a fresh view's jitter.
    let render_after_a = |bed: &mut Bed, cut: bool| {
        let mut view = View::new(&bed.device, 16);
        for _ in 0..16 {
            bed.render(&mut view, &settings, &level_a, &b, false, |_| {});
        }
        let (_, pixels) = bed.render(
            &mut view,
            &settings,
            &level_b,
            &engine_frame::ViewCamera { cut, ..b },
            true,
            |t| assert_eq!(t.motion.unwrap().reset, cut),
        );
        assert_eq!(view.resets().cuts, u64::from(cut));
        (view, pixels.unwrap())
    };
    let (mut cut_view, after_cut) = render_after_a(&mut bed, true);
    let (mut uncut_view, without_cut) = render_after_a(&mut bed, false);
    let mut fresh = View::new(&bed.device, 16);
    let (_, first) = bed.render(&mut fresh, &settings, &level_b, &b, true, |t| {
        assert!(t.motion.unwrap().reset, "a new view starts its history");
    });
    let first = first.unwrap();
    let after_cut_differs = differing(&after_cut, &first);
    let ghost = differing(&without_cut, &first);
    println!("after a cut: {after_cut_differs} pixels differ from a fresh view; without: {ghost}");
    assert_eq!(
        after_cut_differs, 0,
        "the previous view ghosts through a cut"
    );
    assert!(ghost > 0, "the test cannot see a ghost");
    bed.finish(&mut [&mut cut_view, &mut uncut_view, &mut fresh]);
}

/// A texture whose levels are solid colors shows which level is sampled. Rendering at
/// half the output size, textures are sampled one level coarser; TAAU's LOD bias
/// (log2(render / output) = −1) brings back the output size's level.
#[test]
fn the_upscalers_lod_bias_is_applied() {
    let Some(mut bed) = Bed::new((128, 128), true) else {
        return;
    };
    // 256 × 256: level 0 red, 1 green, 2 blue, then grey.
    let colors: [[u8; 4]; 9] = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [128, 128, 128, 255],
        [128, 128, 128, 255],
        [128, 128, 128, 255],
        [128, 128, 128, 255],
        [128, 128, 128, 255],
        [128, 128, 128, 255],
    ];
    let levels = (0..9u32)
        .map(|k| {
            let side = (256 >> k) as usize;
            colors[k as usize].repeat(side * side)
        })
        .collect();
    let texture = bed
        .textures
        .add(
            &bed.device,
            &mut bed.allocator,
            TextureLevels {
                width: 256,
                height: 256,
                format: TextureFormat::Rgba8Unorm,
                levels,
            },
        )
        .unwrap();
    let material = bed.materials.add(MaterialDesc {
        base_color_texture: Some(TextureRef::new(texture)),
        ..MaterialDesc::solid([1.0; 3], 0.0, 1.0)
    });
    // A 4 × 4 face across the whole view: 256 texels over 128 output pixels, 64 render
    // pixels at half the size: level 1 at the output size, level 2 at the render size.
    let draws = vec![bed.cube_at(material, Vec3::new(4.0, 4.0, 0.2), Vec3::ZERO)];
    let distance = 2.0 / (0.45f32).tan();
    let view_camera = camera(Vec3::new(0.0, 0.0, 0.1 + distance), Vec3::ZERO, false);
    let mut dominant = |antialiasing| {
        let settings = ViewSettings {
            antialiasing,
            quality: Quality::Performance,
            bloom: false,
            ..ViewSettings::default()
        };
        let mut view = View::new(&bed.device, 4);
        let mut last = None;
        // Until the texture is resident, then a few frames of accumulation.
        for _ in 0..12 {
            last = Some(bed.render(&mut view, &settings, &draws, &view_camera, true, |_| {}));
        }
        let (prepared, pixels) = last.unwrap();
        let pixels = pixels.unwrap();
        let mut sum = [0u64; 3];
        for p in pixels.chunks(4) {
            for c in 0..3 {
                sum[c] += u64::from(p[c]);
            }
        }
        // SAFETY: the bed's device is idle after `render`.
        unsafe { view.destroy(&mut bed.allocator, &bed.bindless) };
        (prepared, sum)
    };
    let (off, off_sum) = dominant(Antialiasing::OFF);
    let (taau, taau_sum) = dominant(Antialiasing::TAAU);
    println!(
        "off: bias {} rgb {off_sum:?}; TAAU: bias {} rgb {taau_sum:?}",
        off.lod_bias, taau.lod_bias
    );
    assert_eq!((off.render, taau.render), ((64, 64), (64, 64)));
    assert_eq!(off.lod_bias, 0.0);
    assert_eq!(taau.lod_bias, -1.0);
    assert_eq!(bed.materials.texture_lod_bias(), -1.0);
    assert!(
        off_sum[2] > 4 * off_sum[1],
        "without a bias, level 2 (blue): {off_sum:?}"
    );
    assert!(
        taau_sum[1] > 4 * taau_sum[2],
        "with TAAU's bias, level 1 (green): {taau_sum:?}"
    );
    bed.finish(&mut []);
}

/// Changing the upscaler or the output size starts the history anew; a missing upscaler
/// falls back to TAAU; plain settings changes cost nothing.
#[test]
fn settings_changes_and_fallbacks() {
    let Some(mut bed) = Bed::new((64, 64), true) else {
        return;
    };
    let grey = bed.solid([0.5, 0.5, 0.5], [0.0; 3]);
    let draws = vec![bed.cube_at(grey, Vec3::splat(1.0), Vec3::ZERO)];
    let cam = camera(Vec3::new(1.0, 1.5, 3.0), Vec3::ZERO, false);
    let mut view = View::new(&bed.device, 4);
    let mut settings = ViewSettings {
        antialiasing: Antialiasing::TAAU,
        quality: Quality::Quality,
        ..ViewSettings::default()
    };
    let (p, _) = bed.render(&mut view, &settings, &draws, &cam, false, |t| {
        assert!(t.motion.is_some());
    });
    assert_eq!((p.output, p.render), ((64, 64), (42, 42)));
    assert_eq!(p.jitter_phases, 19, "TAAU at 1.5×: ⌈8 · 1.5²⌉");
    settings.bloom = false;
    bed.render(&mut view, &settings, &draws, &cam, false, |_| {});
    assert_eq!(view.resets().changes, 0, "bloom is not a history change");

    settings.antialiasing = Antialiasing::FSR;
    let (p, _) = bed.render(&mut view, &settings, &draws, &cam, false, |_| {});
    assert_eq!(p.antialiasing, Antialiasing::FSR);
    assert_eq!(view.resets().changes, 1);

    // DLSS needs Streamline, which this bed does not load.
    settings.antialiasing = Antialiasing::DLSS;
    let (p, _) = bed.render(&mut view, &settings, &draws, &cam, false, |_| {});
    assert_eq!(p.antialiasing, Antialiasing::TAAU);
    assert_eq!(view.resets().changes, 2);

    // Dynamic resolution: the render size follows the scale, the bias the quality.
    settings.render_scale = Some(0.5);
    let (p, _) = bed.render(&mut view, &settings, &draws, &cam, false, |_| {});
    assert_eq!(p.render, (32, 32));
    assert_eq!(
        p.lod_bias,
        engine_upscale::texture_lod_bias((42, 42), (64, 64))
    );
    assert_eq!(
        view.resets().changes,
        2,
        "a smaller render size keeps the history"
    );

    settings.antialiasing = Antialiasing::OFF;
    let (p, _) = bed.render(&mut view, &settings, &draws, &cam, false, |t| {
        assert!(
            t.motion.is_none(),
            "no motion vectors without temporal antialiasing"
        );
    });
    assert_eq!((p.jitter_phases, p.lod_bias), (0, 0.0));
    assert!(view.temporal().is_none());
    bed.finish(&mut [&mut view]);
}

/// The previous antialiased color reaches the scene hook from the second frame on, and
/// not right after a cut.
#[test]
fn the_previous_color_reaches_the_hook() {
    let Some(mut bed) = Bed::new((64, 48), true) else {
        return;
    };
    let grey = bed.solid([0.5, 0.5, 0.5], [0.0; 3]);
    let draws = vec![bed.cube_at(grey, Vec3::splat(1.0), Vec3::ZERO)];
    let cam = camera(Vec3::new(1.0, 1.5, 3.0), Vec3::ZERO, false);
    for antialiasing in [Antialiasing::OFF, Antialiasing::TAAU] {
        let settings = ViewSettings {
            antialiasing,
            quality: Quality::Quality,
            previous_color: true,
            ..ViewSettings::default()
        };
        let mut view = View::new(&bed.device, 4);
        let mut seen = Vec::new();
        for frame in 0..4 {
            let cut = frame == 2;
            bed.render(
                &mut view,
                &settings,
                &draws,
                &engine_frame::ViewCamera { cut, ..cam },
                false,
                |t| {
                    seen.push(t.previous_color.is_some());
                    assert!(t.motion.is_some(), "previous color implies motion vectors");
                },
            );
        }
        assert_eq!(seen, [false, true, false, true], "{}", antialiasing);
        // SAFETY: the bed's device is idle after `render`.
        unsafe { view.destroy(&mut bed.allocator, &bed.bindless) };
    }
    bed.finish(&mut []);
}

/// Invalid combinations (ADR 0075) are refused by `check` and `prepare` with the same
/// error, and the view keeps working with valid ones; switching between MSAA, filters
/// and spatial or temporal modes frame to frame renders every time, each mode once plain
/// and once graded with depth of field (ADRs 0094, 0095: both switched on and off from
/// frame to frame).
#[test]
fn antialiasing_is_validated_and_modes_switch() {
    let Some(mut bed) = Bed::new((96, 64), true) else {
        return;
    };
    let grey = bed.solid([0.5, 0.5, 0.5], [0.0; 3]);
    let draws = vec![bed.cube_at(grey, Vec3::splat(1.0), Vec3::ZERO)];
    let support = bed.frame.antialiasing_support();
    assert!(support.supports_samples(Msaa::X1));
    assert!(support.supports_upscaler(UpscalerKind::Taau));
    println!("MSAA support: {:?}", support.msaa);
    let invalid = [
        (
            Antialiasing::SMAA_T2X,
            Quality::Quality,
            InvalidAntialiasing::SmaaT2xScaled,
        ),
        (
            Antialiasing {
                post: PostAntialiasing::Off,
                ..Antialiasing::SMAA_T2X
            },
            Quality::Native,
            InvalidAntialiasing::SmaaT2xWithoutSmaa,
        ),
        (
            Antialiasing::FSR.with_samples(Msaa::X4),
            Quality::Quality,
            InvalidAntialiasing::MsaaIntoUpscaler(UpscalerKind::Fsr),
        ),
    ];
    let mut view = View::new(&bed.device, 16);
    for (antialiasing, quality, error) in invalid {
        let settings = ViewSettings {
            antialiasing,
            quality,
            ..ViewSettings::default()
        };
        assert_eq!(bed.frame.check(&settings, bed.size), Err(error));
        let result = bed.frame.prepare(
            &mut view,
            &settings,
            bed.size,
            PrepareContext {
                device: &bed.device,
                allocator: &mut bed.allocator,
                bindless: &bed.bindless,
                materials: &mut bed.materials,
                frame_number: bed.frame_number + 1,
                skinning: None,
            },
        );
        assert!(
            matches!(result, Err(FrameError::Antialiasing(e)) if e == error),
            "{antialiasing}: {result:?}"
        );
    }
    let modes = [
        (Antialiasing::msaa(Msaa::X4), Quality::Native),
        (Antialiasing::SMAA_4X, Quality::Native),
        (
            Antialiasing::spatial(Upscale::Integer),
            Quality::Performance,
        ),
        (Antialiasing::TAAU, Quality::Quality),
        (Antialiasing::FXAA, Quality::Native),
        (Antialiasing::OFF, Quality::Native),
        (
            Antialiasing::msaa(Msaa::X2).with_post(PostAntialiasing::Mlaa),
            Quality::Native,
        ),
        (
            Antialiasing::spatial(Upscale::Lanczos3),
            Quality::Performance,
        ),
        (
            Antialiasing::spatial(Upscale::CatmullRom).with_post(PostAntialiasing::Fxaa),
            Quality::Quality,
        ),
    ];
    let eye = Vec3::new(1.5, 1.5, 3.0);
    for (i, (antialiasing, quality)) in modes.into_iter().chain(modes).enumerate() {
        let graded = (i % modes.len() + i / modes.len()) % 2 == 1;
        let grading = if graded {
            ColorGrading {
                white_balance: [0.3, 0.0],
                contrast: 1.2,
                ..ColorGrading::default()
            }
        } else {
            ColorGrading::default()
        };
        let settings = ViewSettings {
            antialiasing,
            quality,
            bloom: false,
            grading,
            // Focused on the cube (the camera is 3.67 from its center): its face stays sharp.
            depth_of_field: graded.then(|| DofSettings {
                focus_distance: 3.0,
                f_number: 1.4,
                ..DofSettings::default()
            }),
            ..ViewSettings::default()
        };
        if bed.frame.check(&settings, bed.size).is_err() {
            continue;
        }
        let (prepared, pixels) = bed.render(
            &mut view,
            &settings,
            &draws,
            &camera(eye, Vec3::ZERO, false),
            true,
            |_| {},
        );
        assert_eq!(prepared.antialiasing, antialiasing);
        let pixels = pixels.unwrap();
        let lit = pixels.chunks(4).filter(|p| p[0] > 60).count();
        assert!(
            lit > 100,
            "{antialiasing} (graded: {graded}): the cube is missing ({lit} pixels)"
        );
    }
    bed.finish(&mut [&mut view]);
}

/// A game builds the frame renderer on its main thread, which Windows gives 1 MiB of
/// stack. With every feature on, and through every antialiasing mode this GPU runs,
/// building and rendering must fit in it: renderers whose pipelines each held a copy of
/// the device's function table needed stack frames of up to 372 KB and overflowed it on
/// the Radeon (core review 2, §3.1).
#[test]
fn every_feature_fits_a_main_threads_stack() {
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(every_feature_and_mode)
        .unwrap()
        .join()
        .unwrap();
}

fn every_feature_and_mode() {
    let Some(mut bed) = Bed::new((96, 64), false) else {
        return;
    };
    let renderer = bed.frame.forward_mut();
    renderer.set_depth_prepass(true);
    renderer
        .set_ambient_occlusion(&bed.device, &bed.bindless, Some(AoSettings::default()))
        .unwrap();
    renderer
        .set_subsurface_profiles(
            &bed.device,
            &bed.bindless,
            &[SubsurfaceProfile::from_radius([0.3, 0.12, 0.06])],
        )
        .unwrap();
    renderer
        .set_local_shadows(
            &bed.device,
            &mut bed.allocator,
            &bed.bindless,
            Some(LocalShadowSettings::default()),
        )
        .unwrap();
    renderer
        .set_contact_shadows(&bed.device, &bed.bindless, Some(Default::default()))
        .unwrap();
    renderer
        .set_reflections(&bed.device, &bed.bindless, true)
        .unwrap();
    let support = bed.frame.antialiasing_support();
    let mut modes = vec![
        (Antialiasing::OFF, Quality::Native),
        (Antialiasing::FXAA, Quality::Native),
        (Antialiasing::MLAA, Quality::Native),
        (Antialiasing::SMAA_1X, Quality::Native),
        (Antialiasing::SMAA_T2X, Quality::Native),
        (Antialiasing::spatial(Upscale::Fsr1), Quality::Quality),
        (
            Antialiasing::spatial(Upscale::Nearest),
            Quality::Performance,
        ),
        (Antialiasing::spatial(Upscale::CatmullRom), Quality::Quality),
        (
            Antialiasing::spatial(Upscale::Lanczos3).with_post(PostAntialiasing::SMAA),
            Quality::Quality,
        ),
        (Antialiasing::TAAU, Quality::Native),
        (Antialiasing::TAAU, Quality::Quality),
    ];
    if support.supports_upscaler(UpscalerKind::Fsr) {
        modes.push((Antialiasing::FSR, Quality::Quality));
    }
    for samples in [Msaa::X2, Msaa::X4, Msaa::X8] {
        if support.supports_samples(samples) {
            modes.push((Antialiasing::msaa(samples), Quality::Native));
        }
    }
    if support.supports_samples(Msaa::X2) {
        modes.push((Antialiasing::SMAA_S2X, Quality::Native));
        modes.push((Antialiasing::SMAA_4X, Quality::Native));
    }
    let grey = bed.solid([0.5, 0.5, 0.5], [0.0; 3]);
    let draws = vec![bed.cube_at(grey, Vec3::splat(1.0), Vec3::ZERO)];
    let mut view = View::new(&bed.device, 16);
    let eye = Vec3::new(1.5, 1.5, 3.0);
    for (antialiasing, quality) in modes {
        let settings = ViewSettings {
            antialiasing,
            quality,
            screen_space_reflections: Some(SsrSettings::default()),
            ..ViewSettings::default()
        };
        for _ in 0..2 {
            bed.render(
                &mut view,
                &settings,
                &draws,
                &camera(eye, Vec3::ZERO, false),
                false,
                |_| {},
            );
        }
    }
    bed.finish(&mut [&mut view]);
}

/// A streamed level fades in (ADR 0078): the texture's view starts at the level shown and
/// steps finer by fractional levels, and the sampler blends the two levels between, so the
/// picture moves from the old level's colour to the new one's over the fade, not at once.
/// Level 0 is red and level 1 green; the face is sampled at about level 0.
#[test]
fn a_streamed_level_fades_in_on_screen() {
    let Some(mut bed) = Bed::new((256, 256), true) else {
        return;
    };
    if !bed.device.image_view_min_lod() {
        println!("no VK_EXT_image_view_min_lod: levels show at once");
        bed.finish(&mut []);
        return;
    }
    bed.textures.set_mip_fade(Some(MipFade::Frames(4)));
    let colors = |k: u32| match k {
        0 => [255u8, 0, 0, 255],
        1 => [0, 255, 0, 255],
        _ => [0, 0, 255, 255],
    };
    let texture = bed
        .textures
        .add_empty(&mut bed.allocator, 256, 256, TextureFormat::Rgba8Unorm, 9)
        .unwrap();
    let upload = |bed: &mut Bed, k: u32| {
        let side = (256 >> k) as usize;
        let data = colors(k).repeat(side * side);
        let mut span = bed
            .textures
            .staging(&mut bed.allocator, data.len() as u64)
            .unwrap()
            .unwrap();
        span.bytes_mut()[..data.len()].copy_from_slice(&data);
        bed.textures.upload_levels(texture, k, 1, span).unwrap();
    };
    for k in (1..9).rev() {
        upload(&mut bed, k);
    }
    let material = bed.materials.add(MaterialDesc {
        base_color_texture: Some(TextureRef::new(texture)),
        ..MaterialDesc::solid([1.0; 3], 0.0, 1.0)
    });
    // A 4 × 4 face across the 256 × 256 view: a texel a pixel, level 0 where resident.
    let draws = vec![bed.cube_at(material, Vec3::new(4.0, 4.0, 0.2), Vec3::ZERO)];
    let distance = 2.0 / (0.45f32).tan();
    let view_camera = camera(Vec3::new(0.0, 0.0, 0.1 + distance), Vec3::ZERO, false);
    let settings = ViewSettings {
        antialiasing: Antialiasing::OFF,
        quality: Quality::Native,
        bloom: false,
        ..ViewSettings::default()
    };
    let mut view = View::new(&bed.device, 4);
    // Red's share of red and green over the middle of the view.
    let red_share = |bed: &mut Bed, view: &mut View| {
        let (_, pixels) = bed.render(view, &settings, &draws, &view_camera, true, |_| {});
        let pixels = pixels.unwrap();
        let (mut red, mut green) = (0u64, 0u64);
        for y in 64..192 {
            for x in 64..192 {
                let p = &pixels[(y * 256 + x) * 4..][..4];
                red += u64::from(p[0]);
                green += u64::from(p[1]);
            }
        }
        red as f64 / (red + green).max(1) as f64
    };
    // Levels 1-8 resident and shown: green.
    for _ in 0..3 {
        red_share(&mut bed, &mut view);
    }
    let before = red_share(&mut bed, &mut view);
    upload(&mut bed, 0);
    let shares: Vec<f64> = (0..6).map(|_| red_share(&mut bed, &mut view)).collect();
    println!("red share before {before:.3}, then {shares:.3?}");
    assert!(before < 0.05, "level 1 shown: {before}");
    // The frame level 0 arrives in still shows level 1; then a step a frame, never
    // greener than the last, with frames between the two levels before the fade ends.
    // The RTX blends the view's fractional first level in quarters (0.23, 0.50, 0.77 of
    // red); the Radeon's driver in halves (0, 0.5, 1: 2026-10-10, ADR 0078).
    assert!(shares[0] < 0.05, "{shares:?}");
    for w in shares.windows(2) {
        assert!(w[1] >= w[0] - 0.02, "{shares:?}");
    }
    assert!(
        shares[1..4].iter().any(|&s| s > 0.05 && s < 0.95),
        "{shares:?}"
    );
    assert!(shares[4] > 0.95 && shares[5] > 0.95, "{shares:?}");
    bed.finish(&mut [&mut view]);
}
