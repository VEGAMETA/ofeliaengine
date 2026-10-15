//! Reflection probes and the GI volume as components in the player (ADR 0099 step 4):
//! each is made with its first entity, follows its entity and goes with it, with
//! screen-space reflections in the view, under validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use engine_core::math::{Affine3A, Vec3};
use engine_ecs::GlobalTransform;
use engine_render::reflections::SsrSettings;
use engine_runtime::{GiVolume, ReflectionProbe};

#[test]
fn probes_and_the_gi_volume_follow_their_entities() {
    let Some((mut player, mut game)) = common::player_with(|settings| {
        settings.screen_space_reflections = Some(SsrSettings::default());
    }) else {
        return;
    };
    let mut frames = |player: &mut engine_app::Player, n: usize| {
        for _ in 0..n {
            player.frame(&mut game).unwrap();
        }
    };
    // Nothing is made before the entities come.
    frames(&mut player, 3);
    assert!(player.reflection_probes().is_none());
    assert!(player.gi_volume().is_none() && player.ray_scene().is_none());

    // A room probe around the ring of cubes, and a volume over it.
    let at = |x: f32| GlobalTransform(Affine3A::from_translation(Vec3::new(x, 1.0, 0.0)));
    let world = player.world_mut();
    let probe = world.spawn((at(0.0), ReflectionProbe::new(Vec3::new(5.0, 2.0, 5.0))));
    let volume = world.spawn((at(0.0), GiVolume::new(Vec3::new(5.0, 2.0, 5.0), 1.0)));
    frames(&mut player, 40);
    let probes = player.reflection_probes().unwrap();
    let stats = probes.stats();
    println!("probes: {stats:?}");
    assert_eq!((stats.probes, stats.captured, stats.visible), (1, 1, 1));
    let id = player.probe_extract().unwrap().probe(probe).unwrap();
    assert_eq!(probes.get(id).unwrap().position, Vec3::new(0.0, 1.0, 0.0));
    let gi_on = player.gi_settings().is_some();
    if gi_on {
        let gi = player.gi_volume().unwrap();
        let rays = player.ray_scene().unwrap().stats();
        println!(
            "GI: {:?}, {} updates; rays: {rays:?}",
            gi.grid(),
            gi.updates_done()
        );
        assert_eq!(gi.grid().counts, [11, 5, 11]);
        assert!(gi.is_converged() && gi.has_light());
        // The floor and the eight cubes.
        assert!(rays.blas_count >= 1 && rays.instances == 9, "{rays:?}");
        assert!(player.renderer().forward().global_illumination());
    } else {
        println!("no ray queries: the volume is not made");
        assert!(player.gi_volume().is_none());
    }

    // The probe moves with its entity; a resized volume is made anew.
    let world = player.world_mut();
    *world.get_mut::<GlobalTransform>(probe).unwrap() = at(1.0);
    world.get_mut::<GiVolume>(volume).unwrap().half_extents = Vec3::new(6.0, 2.0, 6.0);
    frames(&mut player, 1);
    let probes = player.reflection_probes().unwrap();
    assert_eq!(probes.get(id).unwrap().position, Vec3::new(1.0, 1.0, 0.0));
    if gi_on {
        let gi = player.gi_volume().unwrap();
        assert_eq!(gi.grid().counts, [13, 5, 13]);
        assert!(!gi.is_converged());
    }

    // Gone with their entities: the probe leaves the set, the volume and its rays go.
    let world = player.world_mut();
    world.despawn(probe);
    world.despawn(volume);
    frames(&mut player, 3);
    assert!(player.probe_extract().unwrap().is_empty());
    assert!(player.reflection_probes().unwrap().get(id).is_none());
    assert!(player.gi_volume().is_none() && player.ray_scene().is_none());
    assert!(!player.renderer().forward().global_illumination());
    frames(&mut player, 10);

    let report = player.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
