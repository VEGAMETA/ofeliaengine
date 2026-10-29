//! CLAUDE.md §5: zero heap allocations per frame in steady state with a skinned model (ADR
//! 0124's E12-1a) posed anew every frame, as the editor poses one playing in place: the
//! clip sampled, the skeleton's model transforms and the palette made, the instance's
//! palette set. Khronos' CesiumMan (`tools/fetch-test-assets.ps1`); skipped without it.
//! One test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "common/counting.rs"]
mod counting;

use engine_app::{AssetMeta, Model};
use engine_ecs::{Entity, QueryState, Transform};
use engine_scene::{AssetPaths, AssetRef};
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: counting::CountingAllocator = counting::CountingAllocator;

#[test]
fn frames_with_a_posed_skinned_model_do_not_allocate() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("ofelia-skins-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("CesiumMan.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "Cesium_Man"),
        AssetRef::part(id, "Cesium_Man-effect"),
    );
    let Some((mut player, mut game)) = common::player_with(|settings| {
        settings.project = Some(dir.clone());
    }) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let mut paths = AssetPaths::new();
    paths.insert(mesh, "CesiumMan.gltf#Cesium_Man");
    paths.insert(material, "CesiumMan.gltf#Cesium_Man-effect");
    player.assets_mut().add_paths(&paths);
    let man = player.world_mut().spawn((
        Transform::from_xyz(0.0, -1.0, -3.0),
        Model::new(mesh, material),
    ));
    let started = Instant::now();
    while player.model_rig(man).is_none() {
        assert!(started.elapsed() < Duration::from_secs(30), "no skin");
        player.frame(&mut game).unwrap();
    }
    let length = player.model_rig(man).unwrap().clips()[0].duration();
    let mut time = 0.0f32;
    counting::assert_steady_state_each(
        &mut player,
        &mut game,
        |_| true,
        |player| {
            time = (time + 1.0 / 60.0) % length;
            player.pose_model(man, Some((0, time)));
        },
    );
    // Still skinned: the same entity drawn by its instance.
    let world = player.world_mut();
    let mut skinned = QueryState::<(Entity, &engine_runtime::SkinnedMesh)>::new(world);
    assert_eq!(skinned.iter_readonly(world).count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
