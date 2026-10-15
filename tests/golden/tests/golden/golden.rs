//! Golden-image render tests (ADR 0062): fixed scenes through the whole frame, under the
//! validation layer with synchronization validation, compared with the golden image of
//! this GPU's vendor through FLIP.
//!
//! - `ENGINE_GOLDEN_UPDATE=1` writes new golden images (look at them before committing).
//! - `ENGINE_REQUIRE_GOLDEN=1` fails on a missing golden image or scene.
//! - Renders and error maps go to `target/golden/<vendor>/`.

#![allow(clippy::print_stdout)]

use engine_core::math::Vec3;
use engine_frame::{Antialiasing, Msaa, PostAntialiasing, Quality, Upscale};
use engine_golden::golden::{Outcome, Tolerance, compare};
use engine_golden::scene::{
    RenderOptions, Shot, Skip, Source, procedural_lights, render, sponza_day, sponza_lights,
    sponza_night,
};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::dof::DofSettings;
use engine_render::forward::{DirectionalLight, LocalLight, Spot};
use engine_render::grading::ColorGrading;
use engine_render::reflections::SsrSettings;
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
        Err(Skip::Unsupported(e)) => {
            println!("{}: skipped, {e}", shot.name);
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
        Err(Skip::NoDlss(e)) => {
            assert!(
                !std::env::var("ENGINE_REQUIRE_DLSS").is_ok_and(|v| v == "1"),
                "{}: ENGINE_REQUIRE_DLSS=1 but DLSS is unavailable: {e}",
                shot.name
            );
            println!("{}: skipped, no DLSS ({e})", shot.name);
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

/// GPU particles (ADR 0081) over the procedural scene: sparks added and smoke blended,
/// sorted back to front, the smoke fading where it meets the floor; 90 steps of 1/60 s
/// before the capture.
#[test]
fn procedural_scene_with_particles() {
    check(&Shot {
        particles: true,
        // 1.5 s of particles before the capture.
        capture_after: Some(90),
        ..procedural("procedural_particles")
    });
}

/// An HDR panorama as the sky (ADR 0083): Poly Haven's Kloofendal 48d Partly Cloudy (Pure
/// Sky, CC0) at 2K behind the scene and in its image-based light, its sun found in it and
/// lighting the scene as the directional light, with its shadows.
#[test]
fn procedural_scene_under_an_hdri() {
    check(&Shot {
        panorama: Some("HDRI/kloofendal_48d_partly_cloudy_puresky_2k.hdr"),
        ..procedural("procedural_hdri")
    });
}

/// The particles under TAA (ADR 0081): with no transparent surface in the scene, they make
/// the view's reactive mask themselves, so the resolve does not smear them.
#[test]
fn procedural_scene_with_particles_under_taa() {
    check(&Shot {
        particles: true,
        // TAA: the temporal upscaler at the native size.
        antialiasing: Antialiasing::TAAU,
        quality: Quality::Native,
        temporal_frames: 90,
        ..procedural("procedural_particles_taa")
    });
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

/// The procedural scene at dusk with shadows of a point light in front of the spheres and
/// a spot light above them (ADR 0071): the spheres' shadows on the wall (several cube
/// faces), on the floor and on each other.
#[test]
fn procedural_scene_with_local_shadows() {
    check(&Shot {
        local_shadows: true,
        sky: false,
        sun: DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.2),
            color: Vec3::splat(0.1),
            ambient: Vec3::splat(0.02),
        },
        lights: vec![
            LocalLight {
                position: Vec3::new(-0.6, 1.3, 0.2),
                range: 7.0,
                color: Vec3::new(6.0, 4.0, 2.0),
                spot: None,
                shadow: true,
            },
            LocalLight {
                position: Vec3::new(2.4, 4.2, 0.6),
                range: 8.0,
                color: Vec3::new(20.0, 26.0, 40.0),
                spot: Some(Spot {
                    direction: Vec3::new(0.0, -1.0, -0.35).normalize(),
                    inner: 0.35,
                    outer: 0.6,
                }),
                shadow: true,
            },
        ],
        ..procedural("procedural_local_shadows")
    });
}

/// Sponza at night with point lights along the hall and spot lights from above casting
/// shadows (ADR 0071): columns, arches, plants and curtains on the floor and walls.
#[test]
fn sponza_with_local_shadows() {
    check(&Shot {
        lights: sponza_lights(),
        local_shadows: true,
        sky: false,
        sun: DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.2),
            color: Vec3::splat(0.05),
            ambient: Vec3::splat(0.01),
        },
        ..Shot::new(
            "sponza_local_shadows",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// The procedural scene with the sun low from the side and the sun's contact shadows (ADR
/// 0071): the spheres' and cubes' contact with the floor, sharper than the cascades.
#[test]
fn procedural_scene_with_contact_shadows() {
    check(&Shot {
        contact_shadows: true,
        sun: DirectionalLight {
            direction: Vec3::new(-0.8, -0.4, -0.3).normalize(),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.08),
        },
        ..procedural("procedural_contact_shadows")
    });
}

/// The procedural scene with decals (ADR 0082): a scorch mark across the corner of the
/// floor and the wall (each face takes it by its own angle), a glossy wet patch with a
/// painted arrow drawn over its edge, a ring wrapping a sphere and fading at its sides,
/// a riveted metal plate on the wall (normal map and surface replaced).
#[test]
fn procedural_scene_with_decals() {
    check(&Shot {
        decals: true,
        ..procedural("procedural_decals")
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
        (Err(Skip::NoDlss(e)), _) | (_, Err(Skip::NoDlss(e))) => {
            panic!("the shot does not use DLSS: {e}")
        }
        (Err(Skip::Unsupported(e)), _) | (_, Err(Skip::Unsupported(e))) => {
            panic!("the shot has no antialiasing: {e}")
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

/// Sponza graded (ADR 0094): a warm white balance, lifted cool shadows, warm highlights,
/// more contrast and vibrance, less saturation: the grading and the curve in one LUT.
#[test]
fn sponza_graded() {
    check(&Shot {
        grading: ColorGrading {
            white_balance: [0.35, -0.05],
            shadows: [0.95, 1.0, 1.15],
            highlights: [1.1, 1.05, 0.95],
            slope: [1.02, 1.0, 0.98],
            contrast: 1.2,
            vibrance: 1.3,
            saturation: 0.9,
            ..ColorGrading::default()
        },
        ..Shot::new(
            "sponza_graded",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// Sponza with depth of field (ADR 0095): focused on the nearest arches; the curtains in
/// front blur over them, the far end of the atrium blurs behind.
#[test]
fn sponza_dof() {
    check(&Shot {
        depth_of_field: Some(DofSettings {
            focus_distance: 3.0,
            f_number: 1.4,
            sensor_height: 0.1,
            ..DofSettings::default()
        }),
        ..Shot::new(
            "sponza_dof",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// Sponza's curtains as cloth (ADR 0092): material files with the curtains' own textures
/// and a sheen, applied by a material map when cooking, through a version 5 pack and
/// drawn from its SPIR-V.
#[test]
fn sponza_with_fabric_curtains() {
    check(&Shot {
        fabric_curtains: true,
        ..Shot::new(
            "sponza_fabric",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// Sponza's atrium with its textures streamed by texture feedback in a pool of 4 GiB
/// (ADR 0076): every level the view samples is resident, so the picture is `sponza`'s.
#[test]
fn sponza_streamed_with_an_ample_budget_is_sponza() {
    check(&Shot {
        texture_budget: Some(4 << 30),
        ..Shot::new("sponza", Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
    });
}

/// Sponza's atrium with its textures streamed in a pool of 3 MiB (ADR 0076): the tails
/// (2 MiB) and as much more as fits, every texture its finest levels dropped evenly.
#[test]
fn sponza_streamed_in_a_tight_budget() {
    check(&Shot {
        texture_budget: Some(3 << 20),
        ..Shot::new(
            "sponza_streamed_tight",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// The flight helmet: close-up materials (leather, glass, metal, rubber), normal maps.
#[test]
fn flight_helmet() {
    check(&Shot::new(
        "flight_helmet",
        Source::Gltf(PathBuf::from("FlightHelmet/FlightHelmet.gltf")),
    ));
}

/// glTF's clear coat on the clear coat model (ADR 0092, a built-in type): Khronos'
/// ClearCoatTest, rows of the coat's strength, roughness, their textures and the coat's
/// own normal map over a lit base.
#[test]
fn gltf_clear_coat() {
    check(&Shot {
        size: [400, 800],
        eye: Some(Vec3::new(0.0, 0.3, 13.3)),
        target: Some(Vec3::new(0.0, 0.3, 0.0)),
        ..Shot::new(
            "gltf_clearcoat",
            Source::Gltf(PathBuf::from("ClearCoatTest/ClearCoatTest.gltf")),
        )
    });
}

/// ClearCoatTest with ambient occlusion and its bent normals (ADR 0058): a material type
/// drawn by the twins of the AO pipelines (its PSO record holds both its plain and its
/// bent-normal entry points: the first frames draw the plain one).
#[test]
fn gltf_clear_coat_with_ambient_occlusion() {
    check(&Shot {
        size: [400, 800],
        eye: Some(Vec3::new(0.0, 0.3, 13.3)),
        target: Some(Vec3::new(0.0, 0.3, 0.0)),
        ao: Some(AoSettings::default()),
        ..Shot::new(
            "gltf_clearcoat_ao",
            Source::Gltf(PathBuf::from("ClearCoatTest/ClearCoatTest.gltf")),
        )
    });
}

/// The coat's own normal map over a textured, normal-mapped base: Khronos' ClearcoatWicker
/// (wrinkled plastic over wicker).
#[test]
fn gltf_clear_coat_wicker() {
    check(&Shot::new(
        "gltf_clearcoat_wicker",
        Source::Gltf(PathBuf::from("ClearcoatWicker/ClearcoatWicker.gltf")),
    ));
}

/// glTF's sheen as a layer over the lit base (ADR 0092, a built-in type): Khronos'
/// SheenTestGrid, the sheen's color and roughness over a checkered backdrop.
#[test]
fn gltf_sheen() {
    check(&Shot {
        eye: Some(Vec3::new(-0.17, 0.0, 0.7)),
        target: Some(Vec3::new(-0.17, 0.0, 0.0)),
        ..Shot::new(
            "gltf_sheen",
            Source::Gltf(PathBuf::from("SheenTestGrid/SheenTestGrid.gltf")),
        )
    });
}

/// glTF's unlit (ADR 0092, a built-in type): Khronos' UnlitTest, an orange and a blue
/// object each one flat color, no light on them (their shadows on the ground are allowed).
#[test]
fn gltf_unlit() {
    check(&Shot {
        eye: Some(Vec3::new(0.0, 1.2, 6.0)),
        target: Some(Vec3::new(0.0, 0.0, 0.0)),
        ..Shot::new(
            "gltf_unlit",
            Source::Gltf(PathBuf::from("UnlitTest/UnlitTest.gltf")),
        )
    });
}

/// The fox (ADR 0068), skinned on the GPU at 0.4 s into its Walk cycle (clip 1): legs
/// apart, tail curved. Seen from its side, its texture, sun shadows on itself.
#[test]
fn animated_fox() {
    check(&Shot {
        eye: Some(Vec3::new(105.0, 60.0, 30.0)),
        target: Some(Vec3::new(0.0, 40.0, -5.0)),
        animation: Some((1, 0.4)),
        ..Shot::new("animated_fox", Source::Gltf(PathBuf::from("Fox/Fox.gltf")))
    });
}

/// The fox walking under TAA (ADR 0068, ADR 0072): its clip plays at 60 frames per second
/// while 32 frames accumulate; the skinned motion pass gives the legs and tail their own
/// motion vectors, so they stay sharp instead of smearing.
#[test]
fn animated_fox_with_taa() {
    check(&Shot {
        eye: Some(Vec3::new(105.0, 60.0, 30.0)),
        target: Some(Vec3::new(0.0, 40.0, -5.0)),
        animation: Some((1, 0.4)),
        animation_rate: 1.0 / 60.0,
        antialiasing: Antialiasing::TAAU,
        ..Shot::new(
            "animated_fox_taa",
            Source::Gltf(PathBuf::from("Fox/Fox.gltf")),
        )
    });
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

/// Sponza through a temporal antialiaser (ADR 0072): `temporal_frames` frames accumulated
/// from a fresh history with the camera still, the last one captured.
fn sponza_antialiased(name: &'static str, antialiasing: Antialiasing, quality: Quality) -> Shot {
    Shot {
        antialiasing,
        quality,
        ..Shot::new(name, Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
    }
}

/// TAA at the output size: the plants' alpha-tested edges, the columns' silhouettes and
/// the shadow edges resolved over 32 jittered frames.
#[test]
fn sponza_with_taa() {
    check(&sponza_antialiased(
        "sponza_taa",
        Antialiasing::TAAU,
        Quality::Native,
    ));
}

/// TAAU from 67 % of the output size per axis (Quality: 1.5×), with the texture LOD bias
/// the upscaler asks for.
#[test]
fn sponza_with_taau_at_67_percent() {
    check(&sponza_antialiased(
        "sponza_taau_67",
        Antialiasing::TAAU,
        Quality::Quality,
    ));
}

/// FSR 3.1 Quality (1.5×).
#[test]
fn sponza_with_fsr_quality() {
    check(&sponza_antialiased(
        "sponza_fsr_quality",
        Antialiasing::FSR,
        Quality::Quality,
    ));
}

/// DLSS Quality, where the Streamline DLLs are (`ENGINE_STREAMLINE_DIR`) and the GPU
/// runs it; skipped elsewhere.
#[test]
fn sponza_with_dlss_quality() {
    check(&sponza_antialiased(
        "sponza_dlss_quality",
        Antialiasing::DLSS,
        Quality::Quality,
    ));
}

/// FXAA after tone mapping (ADR 0075): the plants' and columns' edges softened in one
/// pass.
#[test]
fn sponza_with_fxaa() {
    check(&sponza_antialiased(
        "sponza_fxaa",
        Antialiasing::FXAA,
        Quality::Native,
    ));
}

/// SMAA 1x (High) after tone mapping (ADR 0075).
#[test]
fn sponza_with_smaa() {
    check(&sponza_antialiased(
        "sponza_smaa",
        Antialiasing::SMAA_1X,
        Quality::Native,
    ));
}

/// SMAA T2x (ADR 0075): two jitter positions resolved after SMAA 1x.
#[test]
fn sponza_with_smaa_t2x() {
    check(&sponza_antialiased(
        "sponza_smaa_t2x",
        Antialiasing::SMAA_T2X,
        Quality::Native,
    ));
}

/// Nearest scaling from 50 % (ADR 0075): every scene pixel a 2 × 2 block, no blur.
#[test]
fn sponza_nearest_at_50_percent() {
    check(&sponza_antialiased(
        "sponza_nearest_50",
        Antialiasing::spatial(Upscale::Nearest),
        Quality::Performance,
    ));
}

/// MSAA 4× (ADR 0075): the columns' and arches' silhouettes from four samples, the
/// plants' alpha-tested leaves through alpha to coverage, the depth prepass.
#[test]
fn sponza_with_msaa_4x() {
    check(&sponza_antialiased(
        "sponza_msaa4",
        Antialiasing::msaa(Msaa::X4),
        Quality::Native,
    ));
}

/// MSAA 4× and SMAA (ADR 0075): the samples resolve the geometry's edges, SMAA what is
/// left (shading, alpha).
#[test]
fn sponza_with_msaa_4x_and_smaa() {
    check(&sponza_antialiased(
        "sponza_msaa4_smaa",
        Antialiasing::msaa(Msaa::X4).with_post(PostAntialiasing::SMAA),
        Quality::Native,
    ));
}

/// FSR 1 from 67 % (ADR 0075): EASU's edge-adaptive upscaling, RCAS's sharpening.
#[test]
fn sponza_with_fsr1() {
    check(&sponza_antialiased(
        "sponza_fsr1_67",
        Antialiasing::spatial(Upscale::Fsr1),
        Quality::Quality,
    ));
}

/// Catmull-Rom from 67 % (ADR 0075): bicubic in the tone mapper, sharper than bilinear,
/// no ringing around the highlights.
#[test]
fn sponza_catmull_rom_at_67_percent() {
    check(&sponza_antialiased(
        "sponza_catmull_rom_67",
        Antialiasing::spatial(Upscale::CatmullRom),
        Quality::Quality,
    ));
}

/// Lanczos-3 from 67 % (ADR 0075): the rows, then the columns in the tone mapper.
#[test]
fn sponza_lanczos3_at_67_percent() {
    check(&sponza_antialiased(
        "sponza_lanczos3_67",
        Antialiasing::spatial(Upscale::Lanczos3),
        Quality::Quality,
    ));
}

/// Integer scaling (ADR 0075): a 280 × 150 render shown 2× (560 × 300) in the middle of
/// 640 × 360, black around.
#[test]
fn sponza_integer_scaled() {
    check(&Shot {
        render_size: Some((280, 150)),
        ..sponza_antialiased(
            "sponza_integer",
            Antialiasing::spatial(Upscale::Integer),
            Quality::Native,
        )
    });
}

/// TAAU at 67 % with the camera strafing slowly (1 cm per frame, 31 cm over the
/// accumulation): ghosting and smearing behind the columns and the plants show here.
#[test]
fn sponza_with_taau_and_a_moving_camera() {
    check(&Shot {
        camera_motion: Vec3::new(0.0, 0.0, 0.01),
        ..sponza_antialiased("sponza_taau_moving", Antialiasing::TAAU, Quality::Quality)
    });
}

/// Frame generation (ADR 0091): the frame FSR 3.1 generated before a real one of Sponza
/// (TAAU at 67 %), the camera moving 2.8 cm a frame diagonally, with a HUD, a panel in the
/// world and one on the camera drawn over it with its half-way camera.
#[test]
fn sponza_with_frame_generation() {
    check(&Shot {
        camera_motion: Vec3::new(0.02, 0.0, 0.02),
        frame_generation: true,
        ..sponza_antialiased("sponza_fg", Antialiasing::TAAU, Quality::Quality)
    });
}

/// A material type (ADR 0092): the procedural scene's floor as a Slang type of its own (a
/// checker from its parameters), drawn by the passes compiled for it on first use.
#[test]
fn procedural_scene_with_a_material_type() {
    check(&Shot {
        stripes_floor: true,
        ..procedural("procedural_material_type")
    });
}

/// The shading models (ADR 0092): the bottom row of spheres, left to right unlit, clear
/// coat (a sharp coat over matte red), cloth (blue, a pale sheen), hair (brown, strands
/// combed down) and toon (two bands, a highlight, a rim), under the sun, the sky and the
/// local lights; the top row glTF's metal.
#[test]
fn procedural_scene_with_the_shading_models() {
    check(&Shot {
        shading_models: true,
        ..procedural("procedural_shading_models")
    });
}

/// The shading models over the polished floor with reflection probes and screen-space
/// reflections (ADR 0073): the clear coat's lobe is the one the reflections replace,
/// cloth's the sheen's.
#[test]
fn the_shading_models_with_reflections() {
    check(&Shot {
        shading_models: true,
        glossy_floor: true,
        probe_grid: Some([1, 1, 1]),
        ssr: Some(SsrSettings::default()),
        ..procedural("procedural_shading_models_reflections")
    });
}

/// A material type's first sight: drawn with its glTF fallback, the plain floor, before its
/// pipelines exist, the frame is the procedural scene's. Up to 0.01 % of the pixels may
/// differ: a type's batches are drawn after the glTF ones of their state, so where depths
/// tie (the floor meeting the wall) the other surface can win (2 pixels on the Radeon).
#[test]
fn a_material_type_first_draws_its_fallback() {
    let options = RenderOptions {
        validation: true,
        timed_frames: 0,
    };
    let plain = procedural("procedural");
    let first = Shot {
        stripes_floor: true,
        material_first_sight: true,
        ..procedural("procedural_material_first_sight")
    };
    let (plain, first) = match (render(&plain, options), render(&first, options)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(Skip::NoGpu(e)), _) | (_, Err(Skip::NoGpu(e))) => {
            println!("skipped, no GPU ({e})");
            return;
        }
        (Err(e), _) | (_, Err(e)) => panic!("{e:?}"),
    };
    let differing = plain
        .rgba8
        .chunks(4)
        .zip(first.rgba8.chunks(4))
        .filter(|(a, b)| a != b)
        .count();
    let allowed = plain.rgba8.len() / 4 / 10_000;
    assert!(
        differing <= allowed,
        "the first sight differs from the fallback in {differing} pixels (allowed {allowed})"
    );
}

/// A temporal shot renders the same bytes every time (the history and the jitter start
/// fresh once the scene is resident), so its golden image can be exact.
#[test]
fn temporal_shots_are_deterministic() {
    let options = RenderOptions {
        validation: false,
        timed_frames: 0,
    };
    let shot = Shot {
        antialiasing: Antialiasing::TAAU,
        quality: Quality::Quality,
        camera_motion: Vec3::new(0.01, 0.0, 0.0),
        temporal_frames: 12,
        ..procedural("procedural_taau")
    };
    let (one, two) = match (render(&shot, options), render(&shot, options)) {
        (Ok(one), Ok(two)) => (one, two),
        (Err(Skip::NoGpu(e)), _) | (_, Err(Skip::NoGpu(e))) => {
            println!("skipped, no GPU ({e})");
            return;
        }
        (Err(e), _) | (_, Err(e)) => panic!("{e:?}"),
    };
    assert_eq!(one.antialiasing, Antialiasing::TAAU);
    let differing = one
        .rgba8
        .chunks(4)
        .zip(two.rgba8.chunks(4))
        .filter(|(a, b)| a != b)
        .count();
    println!("{}: {differing} pixels differ", one.adapter);
    assert_eq!(differing, 0, "two renders of a temporal shot differ");
}

/// The game runtime's path (ECS entities extracted into a GPU scene, a `Camera` entity
/// resolved every frame by a `CameraView`; ADR 0064, ADR 0072) renders the same bytes as
/// the test bed's draw list with the same camera, without and with TAA.
#[test]
fn the_runtime_renders_what_the_test_bed_renders() {
    let options = RenderOptions {
        validation: true,
        timed_frames: 0,
    };
    for antialiasing in [Antialiasing::OFF, Antialiasing::TAAU] {
        let reference = Shot {
            rigid_camera: true,
            antialiasing,
            temporal_frames: 8,
            camera_motion: Vec3::new(0.02, 0.0, 0.0),
            ..procedural("procedural_runtime")
        };
        let runtime = Shot {
            runtime: true,
            ..reference.clone()
        };
        let (one, two) = match (render(&reference, options), render(&runtime, options)) {
            (Ok(one), Ok(two)) => (one, two),
            (Err(Skip::NoGpu(e)), _) | (_, Err(Skip::NoGpu(e))) => {
                println!("skipped, no GPU ({e})");
                return;
            }
            (Err(e), _) | (_, Err(e)) => panic!("{e:?}"),
        };
        let differing = one
            .rgba8
            .chunks(4)
            .zip(two.rgba8.chunks(4))
            .filter(|(a, b)| a != b)
            .count();
        println!(
            "{}: antialiasing {}: {differing} pixels differ",
            one.adapter, antialiasing
        );
        assert_eq!(differing, 0, "the runtime path drew another picture");
    }
}

/// The procedural scene on a polished metal floor with reflection probes and screen-space
/// reflections (ADR 0073): the spheres, cubes and wall mirrored in the floor where the
/// screen sees them, the probe's capture elsewhere.
#[test]
fn procedural_scene_with_reflections() {
    check(&Shot {
        glossy_floor: true,
        probe_grid: Some([1, 1, 1]),
        ssr: Some(SsrSettings::default()),
        ..procedural("procedural_reflections")
    });
}

/// Sponza by day with everything daylight uses (`scene::sponza_day`): the atmosphere and
/// its sun, light height fog, probe GI, ambient occlusion, contact shadows, reflection
/// probes and screen-space reflections, TAA, bloom.
#[test]
fn sponza_by_day_with_everything() {
    check(&sponza_day("sponza_day", [640, 360]));
}

/// Sponza by night with everything on (`scene::sponza_night`): a bonfire in the
/// courtyard (sparks, smoke, its light), lights along the hall, all with shadows; fog,
/// probe GI, ambient occlusion, reflection probes and screen-space reflections, TAA,
/// bloom.
#[test]
fn sponza_by_night_with_everything() {
    check(&sponza_night("sponza_night", [640, 360]));
}

/// Sponza with global illumination from a volume of probes (ADR 0088), through the
/// runtime: the arcades in shade and the galleries behind them take the light the sunlit
/// floor and walls reflect, without leaking through the walls. Exposed brighter: the
/// courtyard's floor sees a tenth of the sky's light that the sky's harmonics give it.
#[test]
fn sponza_with_gi() {
    check(&Shot {
        runtime: true,
        gi: Some(1.0),
        exposure: 3.0,
        ..Shot::new(
            "sponza_gi",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// Sponza with AMD's Brixelizer GI instead (ADR 0090), probe GI's reference, as
/// `sponza_with_gi` shows it: the distance field built over the first eight frames, the GI
/// running from then on (its rays would otherwise bring the sky in through the unbuilt
/// cascades) and warmed up for 128 frames. Its light is bluer and weaker than probe GI's
/// (its rays read a brick's radiance through a cosine lobe), and it keeps the arcades in
/// shade. Guards the vendored SDK's fixes for AMD's compiler too: without them the Radeon
/// builds the floor with holes.
#[test]
fn sponza_with_brixelizer_gi() {
    check(&Shot {
        runtime: true,
        brixelizer_gi: true,
        exposure: 3.0,
        ..Shot::new(
            "sponza_brixelizer_gi",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// The acceptance view of ADR 0084: the curtains' metal medallions seen from above in
/// shade read gold, reflecting the floor lit by the probe GI that the reflection probes
/// captured.
#[test]
fn sponza_medallions_with_gi() {
    check(&Shot {
        eye: Some(Vec3::new(-6.0, 6.5, -2.0)),
        target: Some(Vec3::new(-3.0, 2.0, 4.5)),
        runtime: true,
        gi: Some(1.0),
        exposure: 3.0,
        probe_grid: Some([6, 2, 3]),
        ..Shot::new(
            "sponza_gi_medallions",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}

/// Sponza with reflection probes (ADR 0073): the atrium cut into boxes, each captured from
/// its centre; inside, the stone and the floor reflect the arches instead of the sky.
#[test]
fn sponza_with_reflection_probes() {
    check(&Shot {
        probe_grid: Some([3, 1, 2]),
        ..Shot::new(
            "sponza_probes",
            Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
        )
    });
}
