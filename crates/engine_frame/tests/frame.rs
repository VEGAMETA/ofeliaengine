//! The frame renderer (ADR 0072) under the validation layer: history dropped on a camera
//! cut (no ghost of the previous view), the texture LOD bias the upscaler asks for is
//! applied, settings changes start the history anew, an unavailable upscaler falls back
//! to TAAU, and the previous color reaches the scene hook.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use common::{Bed, camera};
use engine_core::math::Vec3;
use engine_frame::{Antialiasing, Quality, View, ViewSettings};
use engine_render::material::{MaterialDesc, TextureRef};
use engine_render::texture_pool::{TextureFormat, TextureLevels};

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
