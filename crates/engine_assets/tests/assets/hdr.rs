//! Radiance HDR panoramas (ADR 0083): the decoder against our encoder and hand-written
//! bytes, malformed files, RGBE conversions, the panorama's directions, and the sun search
//! on synthetic skies and on the Poly Haven test panorama (`tools/fetch-test-assets.ps1`;
//! skipped without it, unless `ENGINE_REQUIRE_GOLDEN=1`).

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::float_cmp)]

use engine_assets::hdr::{
    HdrError, HdrImage, SunSearch, decode_hdr, encode_hdr, find_sun, linear_to_rgbe,
    panorama_direction, panorama_uv, rgbe_to_linear,
};
use engine_core::math::Vec3;
use std::path::{Path, PathBuf};

fn assets() -> PathBuf {
    std::env::var_os("ENGINE_TEST_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets"),
        PathBuf::from,
    )
}

/// An image whose channels mix runs and literals.
fn pattern(width: u32, height: u32) -> HdrImage {
    let pixels = (0..width * height)
        .map(|i| {
            let (x, y) = (i % width, i / width);
            [
                (x / 5 * 17 + y) as u8,
                (x * 31 + y * 7) as u8,
                if x % 9 < 4 { 200 } else { x as u8 },
                128 + (y % 3) as u8,
            ]
        })
        .collect();
    HdrImage {
        width,
        height,
        pixels,
    }
}

#[test]
fn encoded_images_decode_to_the_same_pixels() {
    // Run-length encoded (widths 8 to 32,767) and flat.
    for (width, height) in [(40, 3), (8, 2), (300, 5), (5, 4), (1, 1)] {
        let image = pattern(width, height);
        let bytes = encode_hdr(&image);
        assert_eq!(decode_hdr(&bytes).unwrap(), image, "{width} × {height}");
    }
}

#[test]
fn hand_written_scanlines_decode() {
    let mut bytes =
        b"#?RGBE\n# a comment\nEXPOSURE=1.0\nFORMAT=32-bit_rle_rgbe\n\n-Y 2 +X 8\n".to_vec();
    // Row 0, run-length encoded: red a run of 8, green 8 literals, blue a run of 5 then 3
    // literals, the exponent a run of 8.
    bytes.extend([2, 2, 0, 8]);
    bytes.extend([128 + 8, 100]);
    bytes.extend([8, 1, 2, 3, 4, 5, 6, 7, 8]);
    bytes.extend([128 + 5, 7, 3, 9, 9, 9]);
    bytes.extend([128 + 8, 130]);
    // Row 1, flat.
    for x in 0..8u8 {
        bytes.extend([x, 2 * x, 3 * x, 129]);
    }
    let image = decode_hdr(&bytes).unwrap();
    assert_eq!((image.width, image.height), (8, 2));
    for x in 0..8 {
        let blue = if x < 5 { 7 } else { 9 };
        assert_eq!(image.pixels[x], [100, x as u8 + 1, blue, 130]);
        let x = x as u8;
        assert_eq!(image.pixels[8 + usize::from(x)], [x, 2 * x, 3 * x, 129]);
    }
    // (100.5) / 256 × 2^(130 − 128).
    assert_eq!(image.radiance(0, 0).x, 100.5 / 256.0 * 4.0);
}

#[test]
fn malformed_files_are_refused() {
    let header = |resolution: &str| format!("#?RADIANCE\n\n{resolution}\n").into_bytes();
    assert_eq!(decode_hdr(b"P6\n2 2\n"), Err(HdrError::Signature));
    assert_eq!(
        decode_hdr(b"#?RADIANCE\nFORMAT=32-bit_rle_xyze\n\n-Y 1 +X 1\n\0\0\0\0"),
        Err(HdrError::Format("32-bit_rle_xyze".into()))
    );
    for resolution in ["+Y 2 +X 2", "-Y 0 +X 4", "-Y 2", "-Y 70000 +X 70000"] {
        let mut bytes = header(resolution);
        bytes.extend([0; 64]);
        assert!(
            matches!(decode_hdr(&bytes), Err(HdrError::Resolution(_))),
            "{resolution}"
        );
    }
    assert_eq!(
        decode_hdr(b"#?RADIANCE\nFORMAT=32-bit"),
        Err(HdrError::Truncated)
    );
    // Pixels missing: flat, and in a run-length encoded line.
    let mut bytes = header("-Y 2 +X 2");
    bytes.extend([1; 12]);
    assert_eq!(decode_hdr(&bytes), Err(HdrError::Truncated));
    let mut bytes = header("-Y 1 +X 8");
    bytes.extend([2, 2, 0, 8, 128 + 8, 1, 128 + 8, 1]);
    assert_eq!(decode_hdr(&bytes), Err(HdrError::Truncated));
    // A run past the end of the line, and a zero count.
    let mut bytes = header("-Y 1 +X 8");
    bytes.extend([2, 2, 0, 8, 128 + 9, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(decode_hdr(&bytes), Err(HdrError::Scanline(0)));
    let mut bytes = header("-Y 1 +X 8");
    bytes.extend([2, 2, 0, 8, 0, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(decode_hdr(&bytes), Err(HdrError::Scanline(0)));
}

#[test]
fn rgbe_keeps_values_within_a_mantissa_step() {
    for v in [
        1e-30f32, 1e-6, 0.01, 0.5, 1.0, 1.5, 3.0, 255.0, 65_504.0, 1e6, 1e30,
    ] {
        for c in [[v, v, v], [v, v * 0.5, v * 0.01], [0.0, v, v * 0.25]] {
            let back = rgbe_to_linear(linear_to_rgbe(c));
            let max = c[0].max(c[1]).max(c[2]);
            for k in 0..3 {
                // Half a step of a mantissa of at least 128.
                assert!(
                    (back[k] - c[k]).abs() <= max / 256.0 * 1.0001,
                    "{c:?} → {back:?}"
                );
            }
        }
    }
    assert_eq!(linear_to_rgbe([0.0; 3]), [0; 4]);
    assert_eq!(linear_to_rgbe([-1.0, f32::NAN, 0.0]), [0; 4]);
    assert_eq!(linear_to_rgbe([f32::INFINITY, 1.0, 1.0]), [255; 4]);
    assert_eq!(rgbe_to_linear([200, 1, 1, 0]), [0.0; 3]);
}

#[test]
fn panorama_directions_follow_the_engine_axes() {
    let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6;
    assert!(close(panorama_uv(Vec3::NEG_Z), [0.5, 0.5]));
    assert!(close(panorama_uv(Vec3::X), [0.75, 0.5]));
    assert!(close(panorama_uv(Vec3::NEG_X), [0.25, 0.5]));
    assert_eq!(panorama_uv(Vec3::Y)[1], 0.0);
    assert_eq!(panorama_uv(Vec3::NEG_Y)[1], 1.0);
    for i in 0..200 {
        let t = i as f32 * 0.61;
        let d = Vec3::new(t.sin(), (t * 1.7).cos() * 0.9, (t * 0.3).cos()).normalize();
        let back = panorama_direction(panorama_uv(d));
        assert!(back.dot(d) > 1.0 - 1e-6, "{d} → {back}");
    }
}

/// A sky of `sky` everywhere and, if `sun` is given (direction, angular radius, radiance),
/// a disk of it, its edge pixels covered in part (8 × 8 samples each).
fn sky_with_sun(
    width: u32,
    height: u32,
    sky: impl Fn(Vec3) -> Vec3,
    sun: Option<(Vec3, f32, Vec3)>,
) -> HdrImage {
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let uv = |sx: f32, sy: f32| {
                [
                    (x as f32 + sx) / width as f32,
                    (y as f32 + sy) / height as f32,
                ]
            };
            let d = panorama_direction(uv(0.5, 0.5));
            let mut c = sky(d);
            if let Some((toward, radius, radiance)) = sun
                && d.dot(toward) > (radius + 2.0f32.to_radians()).cos()
            {
                let mut covered = 0;
                for k in 0..64 {
                    let s = panorama_direction(uv(
                        (k % 8) as f32 / 8.0 + 1.0 / 16.0,
                        (k / 8) as f32 / 8.0 + 1.0 / 16.0,
                    ));
                    if s.dot(toward) > radius.cos() {
                        covered += 1;
                    }
                }
                c = c.lerp(radiance, covered as f32 / 64.0);
            }
            pixels.push(linear_to_rgbe(c.to_array()));
        }
    }
    HdrImage {
        width,
        height,
        pixels,
    }
}

#[test]
fn the_sun_of_a_synthetic_sky_is_found() {
    let sky = Vec3::new(0.45, 0.5, 0.6);
    let radius = 0.5f32.to_radians();
    let solid_angle = core::f32::consts::TAU * (1.0 - radius.cos());
    // Illuminance (3, 2.8, 2.5) above the sky.
    let illuminance = Vec3::new(3.0, 2.8, 2.5);
    let radiance = sky + illuminance / solid_angle;
    for toward in [
        panorama_direction([0.6, 0.33]),
        // Near the seam and near the zenith.
        panorama_direction([0.999, 0.4]),
        panorama_direction([0.2, 0.01]),
    ] {
        let image = sky_with_sun(2048, 1024, |_| sky, Some((toward, radius, radiance)));
        let sun = find_sun(&image, &SunSearch::default()).unwrap();
        let error = sun
            .direction
            .dot(toward)
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees();
        assert!(error < 0.05, "direction off by {error}°");
        for k in 0..3 {
            let ratio = sun.illuminance[k] / illuminance[k];
            assert!(
                (0.98..1.02).contains(&ratio),
                "illuminance {} vs {illuminance}",
                sun.illuminance
            );
        }
        assert!(
            (sun.sky - sky).abs().max_element() < 0.01,
            "sky {}",
            sun.sky
        );
        assert!(
            (0.5..2.0).contains(&sun.cone.to_degrees()),
            "cone {}°",
            sun.cone.to_degrees()
        );
    }
}

#[test]
fn an_overcast_sky_has_no_sun() {
    let image = sky_with_sun(512, 256, |d| Vec3::splat(0.2 + 0.8 * d.y.max(0.0)), None);
    assert_eq!(find_sun(&image, &SunSearch::default()), None);
    // A bright patch, but not bright enough to be a sun.
    let patch = Some((
        Vec3::new(0.3, 0.5, -0.8).normalize(),
        2f32.to_radians(),
        Vec3::splat(50.0),
    ));
    let image = sky_with_sun(512, 256, |_| Vec3::splat(0.5), patch);
    assert_eq!(find_sun(&image, &SunSearch::default()), None);
}

#[test]
fn the_sun_of_the_test_panorama_is_found() {
    let path = assets().join("HDRI/kloofendal_48d_partly_cloudy_puresky_2k.hdr");
    let Ok(bytes) = std::fs::read(&path) else {
        assert!(
            !std::env::var("ENGINE_REQUIRE_GOLDEN").is_ok_and(|v| v == "1"),
            "{} missing (tools/fetch-test-assets.ps1)",
            path.display()
        );
        println!("skipped: {} missing", path.display());
        return;
    };
    let start = std::time::Instant::now();
    let image = decode_hdr(&bytes).unwrap();
    let decoded = start.elapsed();
    let start = std::time::Instant::now();
    let sun = find_sun(&image, &SunSearch::default()).unwrap();
    let found = start.elapsed();
    let elevation = sun.direction.y.asin().to_degrees();
    let luminance = sun.illuminance.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    println!(
        "{} × {}: decoded in {decoded:?}, sun found in {found:?}: elevation {elevation:.2}°, \
         illuminance {} (luminance {luminance:.3}), cone {:.2}°, sky {}",
        image.width,
        image.height,
        sun.illuminance,
        sun.cone.to_degrees(),
        sun.sky
    );
    // The panorama's name: a sun 48° up. Measured independently (docs/research/hdri-environments.md):
    // 4.29 within 0.5°, 4.38 within 1° (luminance at normal incidence).
    assert!((47.0..49.0).contains(&elevation), "elevation {elevation}°");
    assert!((4.0..4.6).contains(&luminance), "luminance {luminance}");
    assert!(sun.cone.to_degrees() < 2.0);
}

#[test]
fn the_test_panorama_cooks_into_a_bc6h_cube() {
    use engine_assets::panorama::{CookedPanorama, PanoramaCookSettings, cook_panorama};
    let path = assets().join("HDRI/kloofendal_48d_partly_cloudy_puresky_2k.hdr");
    let Ok(bytes) = std::fs::read(&path) else {
        println!("skipped: {} missing", path.display());
        return;
    };
    let image = decode_hdr(&bytes).unwrap();
    let jobs = engine_core::jobs::ThreadPool::builder().build().unwrap();
    let start = std::time::Instant::now();
    let cooked = cook_panorama(&image, &PanoramaCookSettings::default(), Some(&jobs));
    let took = start.elapsed();
    println!(
        "{} × {} cooked in {took:?} on {} workers: {}² faces, {} levels, {:.1} MiB of BC6H",
        image.width,
        image.height,
        jobs.num_workers(),
        cooked.size,
        cooked.levels.len(),
        cooked.bytes() as f64 / f64::from(1 << 20)
    );
    assert_eq!(cooked.size, 512);
    assert_eq!(cooked.levels.len(), 10);
    assert_eq!(
        cooked.levels[0].len(),
        6 * CookedPanorama::face_bytes(512, 0)
    );
    assert_eq!(cooked.sun, find_sun(&image, &SunSearch::default()));
}
