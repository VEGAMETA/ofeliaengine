//! CLAUDE.md §5: zero heap allocations per frame in steady state with models (ADR 0108):
//! entities drawn by asset, built-in meshes and material files, their models shown and
//! hidden in turn every tick, so the player keeps their renderers in step every frame. One
//! test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "common/counting.rs"]
mod counting;

use engine_app::{MaterialFile, Model, builtin};
use engine_core::math::Vec3;
use engine_ecs::Transform;
use engine_scene::{AssetId, AssetPaths, AssetRef};

#[global_allocator]
static GLOBAL: counting::CountingAllocator = counting::CountingAllocator;

#[test]
fn frames_with_models_do_not_allocate() {
    let dir = std::env::temp_dir().join(format!("ofelia-models-alloc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let id = AssetId::new();
    let blue = MaterialFile {
        base_color: [0.1, 0.2, 0.8, 1.0],
        ..MaterialFile::default()
    };
    std::fs::write(dir.join("blue.material"), blue.to_text(id)).unwrap();
    let Some((mut player, mut game)) = common::player_with(|settings| {
        settings.project = Some(dir.clone());
    }) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let mut paths = AssetPaths::new();
    paths.insert(AssetRef::file(id), "blue.material");
    player.assets_mut().add_paths(&paths);
    let world = player.world_mut();
    for (k, mesh) in [builtin::SPHERE, builtin::CYLINDER, builtin::PLANE]
        .into_iter()
        .enumerate()
    {
        world.spawn((
            Transform::from_xyz(-1.5 + 1.5 * k as f32, 0.0, -1.0).with_scale(Vec3::splat(0.4)),
            Model::new(mesh, AssetRef::file(id)),
            common::Flicker(0),
        ));
    }
    world.spawn((
        Transform::from_xyz(0.0, 0.8, -1.0).with_scale(Vec3::splat(0.4)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    counting::assert_steady_state(&mut player, &mut game);
    assert_eq!(player.assets().problem(AssetRef::file(id)), None);
    let _ = std::fs::remove_dir_all(&dir);
}
