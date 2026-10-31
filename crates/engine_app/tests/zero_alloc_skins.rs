//! CLAUDE.md §5: zero heap allocations per frame in steady state with a skinned model (ADR
//! 0124's E12-1a) posed anew every frame, as the editor poses one playing in place: the
//! clip sampled, the skeleton's model transforms and the palette made, the instance's
//! palette set; the editor's live model preview of it playing too (E12-1b), drawn and read
//! back every frame; and a second character played by its `Animator` (E12-2a), its clip on
//! the ticks and its pose each frame, fading all along from the clip at another moment
//! (E12-2b: two samples blended), the fade started again now and then, its root motion
//! moving it (E12-2c: taken out of both poses, read over each tick), its clip's events
//! fired as it plays (E12-3b: eight a loop, from its model's `.meta`); and a door playing
//! clips made in the editor (E12-4a: its turn and its lamp's range keyed, an event, a fade
//! between two clips now and then); and a character playing a clip made in the editor that
//! keys its joints (E12-4e: its arms and neck turned over its rest pose, a joint it lacks
//! left out), fading into its model's clip and back now and then; and a character playing an
//! animation graph (E12-5c: two states cross-faded by a number set now and then, a trigger
//! taken from any state, a layer masked to an arm and an additive one; E12-5b-2: the
//! trigger's transition inertialized, and a fade it cuts short going on as one, their poses
//! captured after the ticks; E12-5c-2: its root motion moving it, its clip's steps fired).
//! Khronos' CesiumMan
//! (`tools/fetch-test-assets.ps1`); skipped without it.
//! One test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "common/counting.rs"]
mod counting;

use engine_app::{AssetMeta, Model};
use engine_core::math::Vec3;
use engine_ecs::{Entity, QueryState, Transform};
use engine_scene::{
    AnimFile, AnimGraphFile, AssetId, AssetPaths, AssetRef, ClipEvent, LayerBlend, LayerFile, Name,
    NodeFile, NodeKind, ParamFile, ParamKind, StateFile, TransitionFile,
};
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
    // Its one clip (unnamed: `Clip 1`) steps eight times a loop.
    let mut meta = AssetMeta::read(&dir.join("CesiumMan.gltf"));
    meta.model.clips.push(engine_scene::ClipImport {
        name: String::from("Clip 1"),
        events: (0..8)
            .map(|i| engine_scene::ClipEvent {
                time: i as f32 * 0.25,
                name: String::from("step"),
                data: String::from(if i % 2 == 0 { "left" } else { "right" }),
                sound: String::new(),
            })
            .collect(),
        ..engine_scene::ClipImport::default()
    });
    std::fs::write(
        AssetMeta::path(&dir.join("CesiumMan.gltf")),
        meta.to_text(id),
    )
    .unwrap();
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
    // A door with two clips made in the editor (E12-4a), held as the editor holds them.
    let (swing_file, flicker_file) = (AssetId::new(), AssetId::new());
    let mut swing = AnimFile::default();
    swing.set_key("Transform.rotation", 0.0, &[0.0, 0.0, 0.0]);
    swing.set_key("Transform.rotation", 0.5, &[0.0, 60.0, 0.0]);
    swing.set_key("Transform.rotation", 1.0, &[0.0, 0.0, 0.0]);
    swing.events.push(ClipEvent {
        time: 0.5,
        name: String::from("creak"),
        ..ClipEvent::default()
    });
    let mut flicker = AnimFile {
        length: 0.5,
        ..AnimFile::default()
    };
    flicker.set_key("Lamp:PointLight.range", 0.0, &[2.0]);
    flicker.set_key("Lamp:PointLight.range", 0.25, &[6.0]);
    flicker.set_key("Transform.rotation", 0.0, &[0.0, 10.0, 0.0]);
    player.assets_mut().set_anim_clip(swing_file, Some(&swing));
    player
        .assets_mut()
        .set_anim_clip(flicker_file, Some(&flicker));
    let door = player.world_mut().spawn((
        Transform::from_xyz(-1.0, -1.0, -3.0),
        engine_runtime::Animator::new("swing")
            .with_clip("swing", AssetRef::file(swing_file))
            .with_clip("flicker", AssetRef::file(flicker_file)),
    ));
    let lamp = player.world_mut().spawn((
        Name::new("Lamp"),
        Transform::IDENTITY,
        engine_runtime::PointLight::new(Vec3::ONE, 2.0),
    ));
    player.world_mut().set_parent(lamp, Some(door)).unwrap();
    let wave_file = AssetId::new();
    let mut wave = AnimFile::default();
    for (joint, turn) in [
        ("Skeleton_arm_joint_R", 70.0),
        ("Skeleton_arm_joint_L__4_", -40.0),
        ("Skeleton_neck_joint_1", 20.0),
        ("Missing", 10.0),
    ] {
        let target = engine_scene::joint_target(joint, "rotation");
        wave.set_key(&target, 0.0, &[0.0, 0.0, 0.0]);
        wave.set_key(&target, 0.5, &[turn, turn, 0.0]);
        wave.set_key(&target, 1.0, &[0.0, 0.0, 0.0]);
    }
    player.assets_mut().set_anim_clip(wave_file, Some(&wave));
    let waver = player.world_mut().spawn((
        Transform::from_xyz(2.0, -1.0, -3.0),
        Model::new(mesh, material),
        engine_runtime::Animator::new("wave").with_clip("wave", AssetRef::file(wave_file)),
    ));
    // A character playing an animation graph (E12-5c), as the editor holds it.
    let graph_file = AssetId::new();
    player
        .assets_mut()
        .set_anim_graph(graph_file, Some(&dance_graph()));
    let dancer = player.world_mut().spawn((
        Transform::from_xyz(-2.0, -1.0, -3.0),
        Model::new(mesh, material),
        engine_runtime::Animator {
            graph: AssetRef::file(graph_file),
            root_motion: engine_runtime::RootMotion::Move,
            ..engine_runtime::Animator::default()
        },
    ));
    // Its parameters by name: jump, speed.
    let (jump, speed) = (0u16, 1u16);
    let dance = |player: &mut engine_app::Player, fast: Option<bool>| {
        if let Some(animator) = player
            .world_mut()
            .get_mut::<engine_runtime::Animator>(dancer)
        {
            match fast {
                Some(fast) => animator.set_slot(speed, if fast { 1.0 } else { 0.0 }),
                None => animator.fire_slot(jump),
            };
        }
    };
    let wave_to = |player: &mut engine_app::Player, clip: &str| {
        if let Some(animator) = player
            .world_mut()
            .get_mut::<engine_runtime::Animator>(waver)
        {
            animator.play(clip, 0.3);
        }
    };
    let switch = |player: &mut engine_app::Player, clip: &str| {
        if let Some(animator) = player.world_mut().get_mut::<engine_runtime::Animator>(door) {
            animator.play(clip, 0.25);
        }
    };
    let started = Instant::now();
    while player.model_rig(man).is_none()
        || player.model_rig(walker).is_none()
        || player.model_rig(waver).is_none()
        || player.model_rig(dancer).is_none()
    {
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
    // Both of the door's clips played once before counting: each bound once, then kept.
    for clip in ["flicker", "swing"] {
        switch(&mut player, clip);
        for _ in 0..40 {
            player.frame(&mut game).unwrap();
        }
    }
    // The waver's clip turns his right arm: its child away from where it rests.
    let arm_child = {
        let skeleton = player.model_rig(waver).unwrap().skeleton();
        let arm = skeleton.find("Skeleton_arm_joint_R").unwrap();
        (0..skeleton.joint_count())
            .find(|&c| skeleton.parent(c) == Some(arm))
            .unwrap()
    };
    let mut rest = Vec::new();
    player.model_rig(waver).unwrap().rest_places(&mut rest);
    let mut joints = Vec::new();
    assert!(player.model_joints(waver, &mut joints));
    let moved = (Vec3::from(joints[arm_child].translation) - rest[arm_child]).length();
    println!("the waver's arm's child moved {moved:.4} from its rest");
    assert!(moved > 1e-3, "posed by his own clip's joints");
    // Both of his clips faded into once before counting.
    for clip in ["", "wave"] {
        wave_to(&mut player, clip);
        for _ in 0..40 {
            player.frame(&mut game).unwrap();
        }
    }
    // The dancer's states each shown once before counting (their names' room grown).
    for fast in [Some(true), None, Some(false)] {
        dance(&mut player, fast);
        for _ in 0..40 {
            player.frame(&mut game).unwrap();
        }
    }
    let mut fast = false;
    let mut states = [0u32; 2];
    let mut steps = 0usize;
    let danced_from = player
        .world_mut()
        .get::<Transform>(dancer)
        .unwrap()
        .translation;
    let mut waving = true;
    let mut frames = 0u32;
    let mut creaks = 0usize;
    let mut door_clip = "swing";
    // The editor's live model preview of the same model plays too (ADR 0124's E12-1b): its
    // pose and turn change every frame, so it is drawn and read back every frame.
    let mut time = 0.0f32;
    let mut pixels = Vec::new();
    let mut taken = 0u32;
    let mut heard = 0usize;
    // ... playing the dancer's graph in place of its clip (E12-5d-2d), its speed held by the
    // editor and changed now and then.
    player.preview_graph(Some(graph_file));
    player.hold_preview_param(0, Some(1.0));
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
                beside: Some((0, length - time)),
            }));
            taken += u32::from(player.take_model_preview(&mut pixels));
            if let Some(events) = player
                .world_mut()
                .resource::<engine_runtime::AnimationEvents>()
            {
                heard += events.fired().len();
                creaks += events.fired().iter().filter(|e| e.entity == door).count();
                steps += events.fired().iter().filter(|e| e.entity == dancer).count();
            }
            frames += 1;
            if frames.is_multiple_of(200) {
                fade(player);
            }
            if frames.is_multiple_of(120) {
                fast = !fast;
                dance(player, Some(fast));
                player.hold_preview_param(0, Some(if fast { 3.0 } else { 1.0 }));
            }
            if frames.is_multiple_of(90) {
                dance(player, None);
            }
            if let Some(animator) = player.world_mut().get::<engine_runtime::Animator>(dancer) {
                states[usize::from(animator.state == "Fast")] += 1;
            }
            if frames.is_multiple_of(100) {
                waving = !waving;
                wave_to(player, if waving { "wave" } else { "" });
            }
            if frames.is_multiple_of(150) {
                door_clip = if door_clip == "swing" {
                    "flicker"
                } else {
                    "swing"
                };
                switch(player, door_clip);
            }
        },
    );
    println!("{taken} model previews taken, {heard} clip events heard, {creaks} creaks");
    println!(
        "the dancer: {} frames walking, {} fast",
        states[0], states[1]
    );
    let danced = (player
        .world_mut()
        .get::<Transform>(dancer)
        .unwrap()
        .translation
        - danced_from)
        .length();
    println!("the dancer: {steps} steps fired, moved {danced:.3} by its root motion");
    assert!(steps > 0, "its graph's clip fired its steps");
    // CesiumMan walks nearly in place: a little, as the walker's clip moves it.
    assert!(danced > 1e-3, "its graph's root motion moved it: {danced}");
    assert!(
        states[0] > 0 && states[1] > 0,
        "both states shown: {states:?}"
    );
    assert!(heard > 0, "the walker's clip fired its events");
    assert!(creaks > 0, "the door's clip fired its event");
    let range = player
        .world_mut()
        .get::<engine_runtime::PointLight>(lamp)
        .unwrap()
        .range;
    println!("the door's lamp's range {range}");
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
    assert_eq!(skinned.iter_readonly(world).count(), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The dancer's graph: his clip walking and twice as fast, cross-faded by `speed`; `jump`
/// back to walking from any state; a layer of the clip on his right arm at half weight, and
/// an additive one of it at a third.
fn dance_graph() -> AnimGraphFile {
    let mut graph = AnimGraphFile::default();
    graph.params.insert(
        "speed".into(),
        ParamFile {
            kind: ParamKind::Float,
            value: 0.0,
        },
    );
    graph.params.insert(
        "jump".into(),
        ParamFile {
            kind: ParamKind::Trigger,
            value: 0.0,
        },
    );
    let layer = |order: u32, root: &str| LayerFile {
        name: root.into(),
        order,
        root: root.into(),
        ..LayerFile::default()
    };
    graph.layers.insert("base".into(), layer(0, "m"));
    graph.layers.insert(
        "arm".into(),
        LayerFile {
            weight: 0.5,
            mask: [("Skeleton_arm_joint_R".to_owned(), 1.0)].into(),
            ..layer(1, "slow")
        },
    );
    graph.layers.insert(
        "lean".into(),
        LayerFile {
            weight: 0.3,
            blend: LayerBlend::Additive,
            ..layer(2, "slow")
        },
    );
    graph.nodes.insert(
        "m".into(),
        NodeFile {
            kind: NodeKind::Machine,
            entry: "walk".into(),
            ..NodeFile::default()
        },
    );
    for (node, speed) in [("slow", 1.0), ("fast", 2.0)] {
        graph.nodes.insert(
            node.into(),
            NodeFile {
                clip: "Clip 1".into(),
                speed,
                ..NodeFile::default()
            },
        );
    }
    for (state, name, pose) in [("walk", "Walk", "slow"), ("run", "Fast", "fast")] {
        graph.states.insert(
            state.into(),
            StateFile {
                name: name.into(),
                machine: "m".into(),
                pose: pose.into(),
                ..StateFile::default()
            },
        );
    }
    // The jump inertialized (E12-5b-2), and a fade it cuts short goes on as one: the
    // player captures their poses.
    for (id, from, to, when) in [
        ("t1", "walk", "run", "speed > 0.5"),
        ("t2", "run", "walk", "speed <= 0.5"),
        ("t3", "", "walk", "jump"),
    ] {
        graph.transitions.insert(
            id.into(),
            TransitionFile {
                from: from.into(),
                to: to.into(),
                when: when.into(),
                inertialize: id == "t3",
                ..TransitionFile::default()
            },
        );
    }
    graph
}
