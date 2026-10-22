//! Scripts in the editor (ADR 0114 E7-4a): New Script makes a file named after the selected
//! entity, which the player's script host loads at the next frame and whose component goes
//! to the entity as one step; Attach Script gives it a component a script of the project
//! declares, whose hooks run in Play.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Assets, Game, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_platform::window::{WindowEvent, WindowEventKind, WindowId};
use engine_runtime::{Camera, Sun, look_rotation};
use engine_scene::{AssetId, Name, SaveOptions, SceneId, SceneRegistry, find_named, save_with};
use std::path::{Path, PathBuf};

/// The game: nothing of its own, the scripts do it all.
struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn start(_player: &mut Player) -> Result<Still, Box<dyn std::error::Error>> {
    Ok(Still)
}

/// A script already in the project: it turns its entity about y.
const SPIN: &str = r#"local EditorSpin = component("EditorSpin", { rate = 2 })
function EditorSpin.on_tick(self, dt: number)
    local t = self.Transform
    t.rotation = quat.axis_angle(vector.create(0, 1, 0), self.rate * dt) * t.rotation
end
return EditorSpin
"#;

/// One editor frame, as the editor's loop runs it.
fn frame<G: Game>(player: &mut Player, game: &mut EditorGame<G>, overlay: &mut EditorOverlay) {
    player.frame_overlaid(game, Some(overlay)).unwrap();
    player.set_view_rect(game.view_rect());
    game.sync_assets(player);
}

/// A window's event for the editor.
fn send<G: Game>(game: &mut EditorGame<G>, kind: WindowEventKind) {
    game.event(&WindowEvent {
        window: WindowId(0),
        time: std::time::Instant::now(),
        kind,
    });
}

/// Enter pressed and released.
fn enter<G: Game>(game: &mut EditorGame<G>) {
    for pressed in [true, false] {
        send(
            game,
            WindowEventKind::Key {
                virtual_key: 0x0D,
                scancode: 0x1C,
                extended: false,
                pressed,
                repeat: false,
            },
        );
    }
}

/// The scene: a door named `door`, the sun and a camera; with `spin`, the project's script.
fn write_project(dir: &Path, door: &str, spin: bool) {
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new(door),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let eye = Vec3::new(2.0, 2.0, 6.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let paths = Assets::new(dir.to_path_buf()).paths().clone();
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
    if spin {
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/spin.luau"), SPIN).unwrap();
    }
}

/// A player for the project in `dir`, or `None` without a GPU (unless one is required).
fn player(dir: &Path) -> Option<(Player, PlayerSettings)> {
    let settings = PlayerSettings {
        title: String::from("editor scripts test"),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.to_path_buf()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    match Player::new(&settings) {
        Ok(player) => Some((player, settings)),
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn new_script_is_loaded_by_the_host_and_attached_and_attached_scripts_run_in_play() {
    let dir = temp_dir("scripts");
    write_project(&dir, "Front door", true);
    let Some((mut player, settings)) = player(&dir) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..5 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let door = find_named(player.world_mut(), "Front door").unwrap();
    game.select(Some(door));

    // New Script: the file now, the host's load and the attaching at the next frame.
    assert!(game.run_command(player.world_mut(), "scene.new_script"));
    let file = dir.join("scripts/front_door.luau");
    assert!(file.exists());
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    let message = game.message().unwrap();
    assert!(message.contains("attached FrontDoor"), "{message}");
    let names: Vec<String> = player
        .scripts()
        .unwrap()
        .component_names()
        .map(str::to_owned)
        .collect();
    assert!(names.iter().any(|n| n == "EditorSpin"), "{names:?}");
    assert!(names.iter().any(|n| n == "FrontDoor"), "{names:?}");
    let new = player
        .world_mut()
        .components()
        .id_by_name("script::FrontDoor")
        .unwrap();
    assert!(player.world_mut().get_raw(door, new).is_some());
    // Opened in its tab; Ctrl+Z takes the component off and leaves the file.
    assert!(game.text_at("front_door.luau").is_some(), "its tab");
    assert!(game.run_command(player.world_mut(), "edit.undo"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(player.world_mut().get_raw(door, new).is_none());
    assert!(file.exists());

    // Attach Script: the project's script picked from the list.
    assert!(game.run_command(player.world_mut(), "scene.attach_script"));
    frame(&mut player, &mut game, &mut overlay);
    for c in "EditorSpin".chars() {
        send(&mut game, WindowEventKind::Char(c));
    }
    frame(&mut player, &mut game, &mut overlay);
    enter(&mut game);
    frame(&mut player, &mut game, &mut overlay);
    let spin = player
        .world_mut()
        .components()
        .id_by_name("script::EditorSpin")
        .unwrap();
    assert!(player.world_mut().get_raw(door, spin).is_some());
    // Still while editing; turning in Play; put back at Stop.
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let rotation =
        |player: &mut Player| player.world_mut().get::<Transform>(door).unwrap().rotation;
    assert_eq!(rotation(&mut player), Quat::IDENTITY);
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..30 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.playing());
    let turned = rotation(&mut player).angle_between(Quat::IDENTITY);
    assert!(turned > 0.2, "the door turned {turned} rad");
    assert!(game.run_command(player.world_mut(), "play.stop"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(!game.playing());
    assert_eq!(rotation(&mut player), Quat::IDENTITY, "Stop put it back");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_project_without_scripts_gets_its_script_host_from_its_first_new_script() {
    let dir = temp_dir("first-script");
    write_project(&dir, "Back door", false);
    let Some((mut player, settings)) = player(&dir) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(player.scripts().is_none(), "no scripts folder, no host");
    let door = find_named(player.world_mut(), "Back door").unwrap();
    game.select(Some(door));
    assert!(game.run_command(player.world_mut(), "scene.new_script"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(dir.join("scripts/back_door.luau").exists());
    let names: Vec<String> = player
        .scripts()
        .unwrap()
        .component_names()
        .map(str::to_owned)
        .collect();
    assert_eq!(names, ["BackDoor"]);
    let id = player
        .world_mut()
        .components()
        .id_by_name("script::BackDoor")
        .unwrap();
    assert!(player.world_mut().get_raw(door, id).is_some());
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_script_changed_on_disk_runs_again_its_values_kept_by_name() {
    let dir = temp_dir("reload");
    write_project(&dir, "Front door", true);
    let Some((mut player, settings)) = player(&dir) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let door = find_named(player.world_mut(), "Front door").unwrap();
    assert!(player.add_script(door, "EditorSpin"));
    let first = player
        .world_mut()
        .components()
        .id_by_name("script::EditorSpin")
        .unwrap();
    // The rate set on the door (3, not the default 2): kept through the reload.
    game.set(player.world_mut(), door, "EditorSpin.rate", "3")
        .unwrap();
    // Another prop and a faster hook, written by another editor: the watcher's turn.
    let changed = SPIN
        .replace("{ rate = 2 }", "{ rate = 2, boost = 10 }")
        .replace("self.rate * dt", "self.rate * self.boost * dt");
    std::fs::write(dir.join("scripts/spin.luau"), changed).unwrap();
    let started = std::time::Instant::now();
    let timer = engine_platform::time::HighResTimer::new().unwrap();
    loop {
        frame(&mut player, &mut game, &mut overlay);
        let said = game.message().unwrap_or_default();
        if said.starts_with("Reloaded scripts/spin.luau") {
            assert!(said.contains("EditorSpin: props changed"), "{said}");
            println!("{said}");
            break;
        }
        assert!(started.elapsed().as_secs() < 10, "no reload: {said}");
        let _ = timer.sleep(std::time::Duration::from_millis(20));
    }
    let world = player.world_mut();
    let moved = world
        .components()
        .id_by_name("script::EditorSpin#2")
        .unwrap();
    assert!(world.get_raw(door, first).is_none());
    assert!(world.get_raw(door, moved).is_some());
    // The values by name: the rate set, the boost's default; the inspector's path reads them.
    let rate = |player: &mut Player| {
        let world = player.world_mut();
        let info = world.components().type_info(moved).unwrap();
        let engine_reflect::TypeKind::Struct(fields) = &info.kind else {
            unreachable!()
        };
        let at = |name: &str| fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(door, moved).unwrap().as_ptr();
        // SAFETY: two f32 props of the door's live value.
        unsafe {
            (
                base.add(at("rate")).cast::<f32>().read_unaligned(),
                base.add(at("boost")).cast::<f32>().read_unaligned(),
            )
        }
    };
    assert_eq!(rate(&mut player), (3.0, 10.0));
    // Undo of the rate's step puts back the default, in the new layout.
    assert!(game.run_command(player.world_mut(), "edit.undo"));
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(rate(&mut player), (2.0, 10.0));
    assert!(player.world_mut().get_raw(door, first).is_none());
    // Play runs the new hook.
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let turned = player
        .world_mut()
        .get::<Transform>(door)
        .unwrap()
        .rotation
        .angle_between(Quat::IDENTITY);
    assert!(turned > 1.0, "the boosted hook turned it {turned} rad");
    assert!(game.run_command(player.world_mut(), "play.stop"));
    frame(&mut player, &mut game, &mut overlay);
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
