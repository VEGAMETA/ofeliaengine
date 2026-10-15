//! CLAUDE.md §5: zero heap allocations per frame in steady state with reflection probes,
//! screen-space reflections and the GI volume (ADR 0099 step 4): the probes' mirror of
//! their entities, their captures (the spinning cubes inside the probe's box ask for one
//! every frame), the volume's rays and updates once converged. One test per file, so
//! nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "common/counting.rs"]
mod counting;

use engine_core::math::{Affine3A, Vec3};
use engine_ecs::GlobalTransform;
use engine_render::reflections::SsrSettings;
use engine_runtime::{GiVolume, ReflectionProbe};

#[global_allocator]
static GLOBAL: counting::CountingAllocator = counting::CountingAllocator;

#[test]
fn frames_with_probes_and_gi_do_not_allocate() {
    let Some((mut player, mut game)) = common::player_with(|settings| {
        settings.screen_space_reflections = Some(SsrSettings::default());
    }) else {
        return;
    };
    let center = GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 1.0, 0.0)));
    let world = player.world_mut();
    world.spawn((center, ReflectionProbe::new(Vec3::new(5.0, 2.0, 5.0))));
    world.spawn((center, GiVolume::new(Vec3::new(5.0, 2.0, 5.0), 1.0)));
    counting::assert_steady_state(&mut player, &mut game);
    let stats = player.reflection_probes().unwrap().stats();
    println!(
        "probes: {stats:?}; GI volume: {:?}",
        player
            .gi_volume()
            .map(|gi| (gi.grid().counts, gi.is_converged()))
    );
    assert_eq!(stats.captured, 1);
}
