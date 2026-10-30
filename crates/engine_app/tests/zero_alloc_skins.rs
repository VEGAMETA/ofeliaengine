//! CLAUDE.md §5: zero heap allocations per frame in steady state with a skinned model (ADR
//! 0124's E12-1a) posed anew every frame, as the editor poses one playing in place: the
//! clip sampled, the skeleton's model transforms and the palette made, the instance's
//! palette set; the editor's live model preview of it playing too (E12-1b), drawn and read
//! back every frame; and a second character played by its `Animator` (E12-2a), its clip on
//! the ticks and its pose each frame, fading all along from the clip at another moment
//! (E12-2b: two samples blended), the fade started again now and then, its root motion
//! moving it (E12-2c: taken out of both poses, read over each tick). Khronos' CesiumMan
//! (`tools/fetch-test-assets.ps1`); skipped without it.
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
    let walker = player.world_mut().spawn((
        Transform::from_xyz(1.0, -1.0, -3.0),
        Model::new(mesh, material),
        engine_runtime::Animator {
            root_motion: engine_runtime::RootMotion::Move,
            ..engine_runtime::Animator::new("")
        },
    ));
    // A socket in his hand, a cube in it (E12-1c): placed every tick.
    let hand = player.world_mut().spawn((
        Transform::IDENTITY,
        engine_runtime::JointAttachment::new("Skeleton_arm_joint_R__3_"),
    ));
    player.world_mut().set_parent(hand, Some(walker)).unwrap();
    let cube = player.world_mut().spawn((
        Transform::from_xyz(0.0, 0.1, 0.0),
        Model::new(engine_app::builtin::CUBE, engine_app::builtin::MATERIAL),
    ));
    player.world_mut().set_parent(cube, Some(hand)).unwrap();
    let started = Instant::now();
    while player.model_rig(man).is_none() || player.model_rig(walker).is_none() {
        assert!(started.elapsed() < Duration::from_secs(30), "no skin");
        player.frame(&mut game).unwrap();
    }
    let length = player.model_rig(man).unwrap().clips()[0].duration();
    let fade = |player: &mut engine_app::Player| {
        if let Some(animator) = player
            .world_mut()
            .get_mut::<engine_runtime::Animator>(walker)
        {
            animator.play("", 1000.0);
        }
    };
    fade(&mut player);
    let mut frames = 0u32;
    // The editor's live model preview of the same model plays too (ADR 0124's E12-1b): its
    // pose and turn change every frame, so it is drawn and read back every frame.
    let mut time = 0.0f32;
    let mut pixels = Vec::new();
    let mut taken = 0u32;
    counting::assert_steady_state_each(
        &mut player,
        &mut game,
        |_| true,
        |player| {
            time = (time + 1.0 / 60.0) % length;
            player.pose_model(man, Some((0, time)));
            player.set_model_preview(Some(engine_app::ModelPreview {
                mesh,
                material,
                turn: [time, 0.0],
                pose: Some((0, time)),
                floor: Some([time, 0.0, time]),
            }));
            taken += u32::from(player.take_model_preview(&mut pixels));
            frames += 1;
            if frames.is_multiple_of(200) {
                fade(player);
            }
        },
    );
    println!("{taken} model previews taken");
    assert!(taken > 100, "the live preview drawn as it played: {taken}");
    let walked = player
        .world_mut()
        .get::<engine_runtime::Animator>(walker)
        .unwrap()
        .time;
    println!("the animator's clip at {walked:.3} s");
    let fading = player
        .world_mut()
        .get::<engine_runtime::Animator>(walker)
        .unwrap()
        .fade;
    assert_eq!(fading, 1000.0, "faded all along");
    // Still skinned: the same entities drawn by their instances.
    let world = player.world_mut();
    let mut skinned = QueryState::<(Entity, &engine_runtime::SkinnedMesh)>::new(world);
    assert_eq!(skinned.iter_readonly(world).count(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}
