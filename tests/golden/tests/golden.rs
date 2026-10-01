//! Golden-image render tests (ADR 0062): fixed scenes through the whole frame, under the
//! validation layer with synchronization validation, compared with the golden image of
//! this GPU's vendor through FLIP.
//!
//! - `ENGINE_GOLDEN_UPDATE=1` writes new golden images (look at them before committing).
//! - `ENGINE_REQUIRE_GOLDEN=1` fails on a missing golden image or scene.
//! - Renders and error maps go to `target/golden/<vendor>/`.

#![allow(clippy::print_stdout)]

use engine_core::math::Vec3;
use engine_golden::golden::{Outcome, Tolerance, compare};
use engine_golden::scene::{RenderOptions, Shot, Skip, Source, procedural_lights, render};
use engine_render::ao::AoSettings;
use std::path::PathBuf;

fn require_golden() -> bool {
    std::env::var("ENGINE_REQUIRE_GOLDEN").is_ok_and(|v| v == "1")
}

fn check(shot: &Shot) {
    let options = RenderOptions {
        validation: true,
        timed_frames: 0,
    };
    let rendered = match render(shot, options) {
        Ok(r) => r,
        Err(Skip::NoGpu(e)) => {
            println!("{}: skipped, no GPU ({e})", shot.name);
            return;
        }
        Err(Skip::NoScene(path)) => {
            assert!(
                !require_golden(),
                "{}: {} missing (tools/fetch-test-assets.ps1, or ENGINE_TEST_ASSETS)",
                shot.name,
                path.display()
            );
            println!("{}: skipped, {} missing", shot.name, path.display());
            return;
        }
    };
    match compare(&rendered, shot.name, Tolerance::default()) {
        Outcome::Matched(result) => println!(
            "{} on {}: mean FLIP {:.5}, 99.9th percentile {:.4}, max {:.4}",
            shot.name,
            rendered.adapter,
            result.mean(),
            result.percentile(0.999),
            result.max()
        ),
        Outcome::Different(result) => panic!(
            "{} on {}: differs from its golden image: mean FLIP {:.5}, 99.9th percentile {:.4}, \
             max {:.4} (render and error map in target/golden/)",
            shot.name,
            rendered.adapter,
            result.mean(),
            result.percentile(0.999),
            result.max()
        ),
        Outcome::Updated(path) => {
            println!("{}: golden image written: {}", shot.name, path.display())
        }
        Outcome::Missing(path) => {
            assert!(
                !require_golden(),
                "{}: no golden image {}",
                shot.name,
                path.display()
            );
            println!("{}: no golden image yet ({})", shot.name, path.display());
        }
    }
}

fn procedural(name: &'static str) -> Shot {
    Shot {
        eye: Some(Vec3::new(0.4, 1.9, 4.8)),
        target: Some(Vec3::new(0.0, 1.0, -1.0)),
        lights: procedural_lights(),
        ..Shot::new(name, Source::Procedural)
    }
}

/// Spheres of graded roughness and metalness, glass, a glowing cube, sun shadows, a
/// point and a spot light, sky light and bloom.
#[test]
fn procedural_scene() {
    check(&procedural("procedural"));
}

/// The same without sky light, with the depth prepass: the other shading permutations.
#[test]
fn procedural_scene_without_sky() {
    check(&Shot {
        sky: false,
        bloom: false,
        prepass: true,
        ..procedural("procedural_no_sky")
    });
}

/// The procedural scene with ambient occlusion (High): contact shadows under the spheres
/// and cubes and along the wall.
#[test]
fn procedural_scene_with_ambient_occlusion() {
    check(&Shot {
        ao: Some(AoSettings::default()),
        ..procedural("procedural_ao")
    });
}

/// Sponza with ambient occlusion: the curtains' folds, the arches, the plants.
#[test]
fn sponza_with_ambient_occlusion() {
    check(&Shot {
        ao: Some(AoSettings::default()),
        ..Shot::new(
            "sponza_ao",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// Sponza's atrium: textures (BC7, BC5 normals), alpha-tested plants, sun shadows through
/// the arches, sky light.
#[test]
fn sponza() {
    check(&Shot::new(
        "sponza",
        Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
    ));
}

/// The flight helmet: close-up materials (leather, glass, metal, rubber), normal maps.
#[test]
fn flight_helmet() {
    check(&Shot::new(
        "flight_helmet",
        Source::Gltf(PathBuf::from("FlightHelmet/FlightHelmet.gltf")),
    ));
}
