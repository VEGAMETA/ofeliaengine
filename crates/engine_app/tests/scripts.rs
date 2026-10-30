//! Luau scripts in the player (ADR 0114, E7-1a): a project's `scripts/` loaded at start, a
//! script component given to an entity, its hooks run in the fixed ticks only while the
//! world simulates; a script driving a character's animation graph (ADR 0124's E12-5c).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;
use std::time::{Duration, Instant};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

#[test]
fn a_scripts_component_spins_its_entity_in_the_ticks() {
    let dir = std::env::temp_dir().join(format!("ofelia-scripts-player-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("scripts/spin.luau"),
        r#"local Spin = component("Spin", { rate = 1.5 })
function Spin.on_start(self)
    print("spinning", self.entity.name)
end
function Spin.on_tick(self, dt)
    local t = self.Transform
    t.rotation = quat.axis_angle(vector.create(0, 1, 0), self.rate * dt) * t.rotation
end
return Spin
"#,
    )
    .unwrap();
    let settings = PlayerSettings {
        title: String::from("scripts test"),
        window: Window::Offscreen {
            width: 64,
            height: 64,
        },
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        tick_hz: 64.0,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let names: Vec<String> = player
        .scripts()
        .unwrap()
        .component_names()
        .map(str::to_owned)
        .collect();
    assert_eq!(names, ["Spin"]);
    let cube = player.world_mut().spawn((
        Name::new("Cube"),
        Transform::IDENTITY,
        GlobalTransform::IDENTITY,
    ));
    assert!(player.add_script(cube, "Spin"));
    assert!(!player.add_script(cube, "Nothing"));
    let mut game = Still;
    // Held still: the scripts wait.
    player.set_simulating(false);
    player.set_frame_time(Some(0.25));
    player.frame(&mut game).unwrap();
    assert_eq!(
        player.world_mut().get::<Transform>(cube).unwrap().rotation,
        Quat::IDENTITY
    );
    // Simulating: 1.5 rad/s for every tick the frames ran (64 a second, a few a frame at
    // most).
    player.set_simulating(true);
    player.set_frame_time(Some(1.0 / 32.0));
    let mut ticks = 0;
    for _ in 0..8 {
        ticks += player.frame(&mut game).unwrap().map_or(0, |r| r.ticks);
    }
    assert!(ticks >= 8, "{ticks}");
    let rotation = player.world_mut().get::<Transform>(cube).unwrap().rotation;
    let expected = Quat::from_axis_angle(Vec3::Y, 1.5 * ticks as f32 / 64.0);
    assert!(rotation.angle_between(expected) < 1e-3, "{rotation:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-5c: a script drives a fox's animation graph in the player. The player reads
/// the graph before the scripts' tick, so the script's `set` and `trigger` reach it by name;
/// the graph's tick after the scripts takes them; the script reads the states it leaves.
#[test]
fn a_script_drives_a_foxs_animation_graph() {
    use engine_app::{AssetMeta, Assets};
    use engine_scene::{
        AnimGraphFile, LayerFile, NodeFile, NodeKind, ParamFile, ParamKind, SaveOptions,
        SceneRegistry, StateFile, TransitionFile, save_with,
    };
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/Fox");
    if !source.join("Fox.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("ofelia-scripts-graph-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for folder in ["models", "anims", "scripts"] {
        std::fs::create_dir_all(dir.join(folder)).unwrap();
    }
    for file in ["Fox.gltf", "Fox.bin", "Texture.png"] {
        std::fs::copy(source.join(file), dir.join("models").join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("models/Fox.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "fox1"),
        AssetRef::part(id, "fox_material"),
    );
    // Idle (Survey), Move (Walk) once `speed` is above 0.1, Alert (Survey twice as fast)
    // from any state by `look`, back to Idle near its end.
    let graph_id = AssetId::new();
    let mut graph = AnimGraphFile::default();
    for (name, kind) in [("speed", ParamKind::Float), ("look", ParamKind::Trigger)] {
        graph
            .params
            .insert(name.into(), ParamFile { kind, value: 0.0 });
    }
    graph.layers.insert(
        "base".into(),
        LayerFile {
            name: "Base".into(),
            root: "m".into(),
            ..LayerFile::default()
        },
    );
    graph.nodes.insert(
        "m".into(),
        NodeFile {
            kind: NodeKind::Machine,
            entry: "idle".into(),
            ..NodeFile::default()
        },
    );
    for (node, clip, speed) in [
        ("survey", "Survey", 1.0),
        ("walk", "Walk", 1.0),
        ("alert", "Survey", 2.0),
    ] {
        graph.nodes.insert(
            node.into(),
            NodeFile {
                clip: clip.into(),
                speed,
                ..NodeFile::default()
            },
        );
    }
    for (state, name, pose) in [
        ("idle", "Idle", "survey"),
        ("move", "Move", "walk"),
        ("alert", "Alert", "alert"),
    ] {
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
    for (t, from, to, when, exit) in [
        ("t1", "idle", "move", "speed > 0.1", 0.0),
        ("t2", "", "alert", "look", 0.0),
        ("t3", "alert", "idle", "", 0.9),
    ] {
        graph.transitions.insert(
            t.into(),
            TransitionFile {
                from: from.into(),
                to: to.into(),
                when: when.into(),
                blend: 0.1,
                exit,
                ..TransitionFile::default()
            },
        );
    }
    std::fs::write(dir.join("anims/fox.agraph"), graph.to_text(graph_id, None)).unwrap();
    // Each new state noted; Idle asks for speed (once), Move fires `look` and stops.
    std::fs::write(
        dir.join("scripts/rider.luau"),
        r#"local Rider = component("Rider", { seen = prop.string(""), last = prop.string("") })
function Rider.on_tick(self, dt)
    local a = self.Animator
    local state = a.state
    if state ~= self.last then
        self.last = state
        self.seen ..= state .. ";"
    end
    if state == "Idle" and not string.find(self.seen, "Move", 1, true) then
        a:set("speed", 1)
    elseif state == "Move" then
        a:trigger("look")
        a:set("speed", 0)
    end
end
return Rider
"#,
    )
    .unwrap();
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(mesh, "models/Fox.gltf#fox1");
    paths.insert(material, "models/Fox.gltf#fox_material");
    paths.insert(AssetRef::file(graph_id), "anims/fox.agraph");
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Fox"),
        Transform::from_scale(Vec3::splat(0.01)),
        Model::new(mesh, material),
        Animator {
            graph: AssetRef::file(graph_id),
            ..Animator::default()
        },
    ));
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("graph script test"),
        window: Window::Offscreen {
            width: 64,
            height: 64,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        tick_hz: 64.0,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let fox = find_named(player.world_mut(), "Fox").unwrap();
    assert!(player.add_script(fox, "Rider"));
    let seen = |player: &mut Player| -> String {
        let id = player.scripts().unwrap().component("Rider").unwrap();
        let world = player.world_mut();
        let engine_reflect::TypeKind::Struct(fields) =
            &world.components().type_info(id).unwrap().kind
        else {
            unreachable!()
        };
        let at = fields.iter().find(|f| f.name == "seen").unwrap().offset;
        let base = world.get_raw(fox, id).unwrap().as_ptr();
        // SAFETY: Rider's `seen`, a `String` at its offset in the live component.
        unsafe { engine_reflect::read_str(base.add(at)) }.to_owned()
    };
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    let started = Instant::now();
    while seen(&mut player) != "Idle;Move;Alert;Idle;" {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "seen {:?}, the animator {:?}",
            seen(&mut player),
            player.world_mut().get::<Animator>(fox).map(|a| &a.state)
        );
        player.frame(&mut game).unwrap();
    }
    let a = player.world_mut().get::<Animator>(fox).unwrap();
    assert_eq!(
        a.params[..2],
        [0.0, 0.0],
        "look used up, speed set back to 0"
    );
    assert_eq!(a.params_set & 0b10, 0b10, "speed set by the script");
    let _ = std::fs::remove_dir_all(&dir);
}
