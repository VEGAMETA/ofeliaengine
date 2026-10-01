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
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::forward::DirectionalLight;
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

/// The procedural scene with subsurface scattering (ADR 0059): the floor and the
/// dielectric spheres scatter like wax; the shadows on the floor get soft, reddish edges.
#[test]
fn procedural_scene_with_subsurface_scattering() {
    check(&Shot {
        subsurface: true,
        ..procedural("procedural_sss")
    });
}

/// The ambient occlusion shot with async compute on and off (ADR 0065): the passes run
/// on another queue, in another order, with ownership transfers, and the bytes are the
/// same. Without an async compute queue both renders take the single-queue path.
#[test]
fn async_compute_renders_the_same_bytes_as_one_queue() {
    let options = RenderOptions {
        validation: true,
        timed_frames: 0,
    };
    let shot = |on| Shot {
        ao: Some(AoSettings::default()),
        async_compute: Some(on),
        ..procedural("procedural_ao")
    };
    let (one, two) = match (render(&shot(false), options), render(&shot(true), options)) {
        (Ok(one), Ok(two)) => (one, two),
        (Err(Skip::NoGpu(e)), _) | (_, Err(Skip::NoGpu(e))) => {
            println!("skipped, no GPU ({e})");
            return;
        }
        (Err(Skip::NoScene(path)), _) | (_, Err(Skip::NoScene(path))) => {
            panic!("procedural scene needs no files: {}", path.display())
        }
    };
    println!(
        "{}: async compute {} vs {}",
        two.adapter, one.async_compute, two.async_compute
    );
    assert!(!one.async_compute);
    assert!(one.rgba8 == two.rgba8, "async compute changed the picture");
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

/// The procedural scene under the physically based sky (ADR 0069), looking out over the
/// wall towards the horizon: the sun at `elevation` degrees, its light's color from the
/// atmosphere, the sky and the image-based light from its tables.
fn atmosphere_shot(name: &'static str, elevation: f32) -> Shot {
    let e = elevation.to_radians();
    Shot {
        eye: Some(Vec3::new(-6.5, 1.3, 3.0)),
        target: Some(Vec3::new(6.0, 1.8, -0.5)),
        fov_y: 70.0,
        sun: DirectionalLight {
            // Ahead and to the right of the camera, lighting the wall's face.
            direction: -Vec3::new(0.7 * e.cos(), e.sin(), 0.7 * e.cos()).normalize(),
            color: Vec3::splat(4.0),
            ambient: Vec3::ZERO,
        },
        atmosphere: Some(Atmosphere::earth()),
        ..procedural(name)
    }
}

/// Noon: a deep blue sky, white sunlight.
#[test]
fn procedural_scene_under_the_atmosphere_at_noon() {
    check(&atmosphere_shot("procedural_atmosphere_noon", 60.0));
}

/// Sunset: the sun 3° up, orange light, a reddened horizon; aerial perspective scaled up
/// (×2,000) so the 14-unit scene shows it.
#[test]
fn procedural_scene_under_the_atmosphere_at_sunset() {
    let mut shot = atmosphere_shot("procedural_atmosphere_sunset", 3.0);
    if let Some(a) = shot.atmosphere.as_mut() {
        a.aerial_perspective_scale = 2000.0;
    }
    shot.exposure = 3.0;
    check(&shot);
}

/// Height fog over the scene under the atmosphere in the morning: thick at the floor,
/// thinning upwards, glowing towards the sun.
#[test]
fn procedural_scene_with_height_fog() {
    check(&Shot {
        fog: Some(HeightFog {
            density: 0.08,
            base_height: 0.0,
            falloff: 0.6,
            ..HeightFog::default()
        }),
        ..atmosphere_shot("procedural_fog", 20.0)
    });
}
