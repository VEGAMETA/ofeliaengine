//! Cameras (ADR 0072): the main camera is found every frame, its fresh `Transform` is
//! what renders, cuts and camera switches drop the history, settings come through.

#![allow(clippy::unwrap_used)]

use engine_core::math::{Quat, Vec3, look_at};
use engine_ecs::{GlobalTransform, Transform, World};
use engine_frame::{Antialiasing, Exposure, Quality, ViewSettings};
use engine_runtime::{Camera, CameraView, look_rotation};

fn close(a: &engine_core::math::Mat4, b: &engine_core::math::Mat4) -> bool {
    a.to_cols_array()
        .iter()
        .zip(b.to_cols_array())
        .all(|(x, y)| (x - y).abs() < 1e-4)
}

#[test]
fn camera_settings_round_trip() {
    for antialiasing in [
        Antialiasing::Off,
        Antialiasing::TAAU,
        Antialiasing::FSR,
        Antialiasing::DLSS,
    ] {
        for quality in Quality::ALL {
            let camera = Camera::perspective(1.0, 0.1, 100.0).with_antialiasing(antialiasing, quality);
            assert_eq!(camera.antialiasing(), antialiasing);
            assert_eq!(camera.quality(), quality);
        }
    }
}

#[test]
fn the_main_camera_is_the_active_one_with_the_highest_priority() {
    let mut world = World::new();
    let low = world.spawn((Transform::from_xyz(1.0, 0.0, 0.0), Camera::perspective(1.0, 0.1, 100.0)));
    let high = world.spawn((
        Transform::from_xyz(2.0, 0.0, 0.0),
        Camera {
            priority: 5,
            ..Camera::perspective(1.0, 0.1, 100.0)
        },
    ));
    let mut cameras = CameraView::new(&mut world, ViewSettings::default());
    let main = cameras.resolve(&world).unwrap();
    assert_eq!(main.entity, high);
    assert!(main.camera.cut, "the first frame starts a history");
    assert!(!cameras.resolve(&world).unwrap().camera.cut);

    // Switching cameras is a cut.
    world.get_mut::<Camera>(high).unwrap().active = 0;
    let main = cameras.resolve(&world).unwrap();
    assert_eq!(main.entity, low);
    assert!(main.camera.cut);
    assert_eq!(main.camera.position, Vec3::new(1.0, 0.0, 0.0));

    // So is a bumped cut generation.
    assert!(!cameras.resolve(&world).unwrap().camera.cut);
    world.get_mut::<Camera>(low).unwrap().cut();
    assert!(cameras.resolve(&world).unwrap().camera.cut);

    world.get_mut::<Camera>(low).unwrap().active = 0;
    assert!(cameras.resolve(&world).is_none());
}

/// ADR 0019's addendum: the camera written in the frame's presentation step renders in
/// the same frame, without a tick's transform propagation.
#[test]
fn the_fresh_transform_renders_without_propagation() {
    let mut world = World::new();
    let eye = Vec3::new(3.0, 2.0, 5.0);
    let target = Vec3::new(0.0, 1.0, 0.0);
    let camera = world.spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, target, Vec3::Y)),
        GlobalTransform::IDENTITY,
        Camera::perspective(1.0, 0.1, 100.0),
    ));
    let mut cameras = CameraView::new(&mut world, ViewSettings::default());
    let main = cameras.resolve(&world).unwrap();
    assert!(close(&main.camera.view_from_world, &look_at(eye, target, Vec3::Y)));

    let moved = Vec3::new(-4.0, 3.0, 1.0);
    *world.get_mut::<Transform>(camera).unwrap() =
        Transform::from_translation(moved).with_rotation(look_rotation(moved, target, Vec3::Y));
    let main = cameras.resolve(&world).unwrap();
    assert!(!main.camera.cut, "motion is not a cut");
    assert!(close(&main.camera.view_from_world, &look_at(moved, target, Vec3::Y)));
}

#[test]
fn a_parented_camera_follows_its_parent() {
    let mut world = World::new();
    let parent = world.spawn((
        Transform::from_xyz(10.0, 0.0, 0.0),
        GlobalTransform(
            Transform::from_xyz(10.0, 0.0, 0.0)
                .with_rotation(Quat::from_rotation_y(core::f32::consts::FRAC_PI_2))
                .to_affine(),
        ),
    ));
    let camera = world.spawn((Transform::from_xyz(0.0, 1.0, 2.0), Camera::perspective(1.0, 0.1, 100.0)));
    world.set_parent(camera, Some(parent)).unwrap();
    let mut cameras = CameraView::new(&mut world, ViewSettings::default());
    let main = cameras.resolve(&world).unwrap();
    // (0, 1, 2) turned 90° about Y is (2, 1, 0), then moved by (10, 0, 0).
    assert!((main.camera.position - Vec3::new(12.0, 1.0, 0.0)).length() < 1e-4);
}

#[test]
fn the_camera_carries_its_settings() {
    let mut world = World::new();
    world.spawn((
        Transform::IDENTITY,
        Camera {
            exposure: 1.5,
            auto_exposure: 1,
            cinematic: 1,
            ..Camera::perspective(0.8, 0.5, 300.0).with_antialiasing(Antialiasing::FSR, Quality::Balanced)
        },
    ));
    let base = ViewSettings {
        bloom: false,
        ..ViewSettings::default()
    };
    let mut cameras = CameraView::new(&mut world, base);
    let main = cameras.resolve(&world).unwrap();
    assert_eq!(main.settings.antialiasing, Antialiasing::FSR);
    assert_eq!(main.settings.quality, Quality::Balanced);
    assert!(main.settings.cinematic);
    assert!(!main.settings.bloom, "the base settings are kept");
    assert!(matches!(main.settings.exposure, Exposure::Auto(s) if s.compensation == 1.5));
    assert_eq!((main.camera.fov_y, main.camera.near, main.camera.far), (0.8, 0.5, 300.0));
}
