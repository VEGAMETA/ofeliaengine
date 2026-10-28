//! Play (ADR 0107 E3): the game runs on the scene's world, which the editor keeps still
//! while editing and puts back at Stop. `ENGINE_EDITOR_PLAY_PNG=<file>` writes a frame
//! while playing.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Assets, Game, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Entity, QueryState, Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind, WindowId};
use engine_runtime::{Camera, Sun, Tween, look_rotation};
use engine_scene::{AssetId, Name, SaveOptions, SceneId, SceneRegistry, find_named, save_with};
use std::path::{Path, PathBuf};

/// The game: the crate slides along x, a shot flies (an entity of the game's own), the
/// score counts the ticks.
struct Slider {
    crate_: Entity,
    shot: Entity,
}

/// A resource the game adds.
struct Score(u32);

fn start(player: &mut Player) -> Result<Slider, Box<dyn std::error::Error>> {
    let world = player.world_mut();
    let crate_ = find_named(world, "Crate").ok_or("no crate in the scene")?;
    let shot = world.spawn((Transform::from_xyz(0.0, 3.0, 0.0),));
    world.insert_resource(Score(0));
    Ok(Slider { crate_, shot })
}

impl Game for Slider {
    fn tick(&mut self, world: &mut World, dt: f32) {
        if let Some(t) = world.get_mut::<Transform>(self.crate_) {
            t.translation.x += dt;
        }
        if let Some(t) = world.get_mut::<Transform>(self.shot) {
            t.translation.z -= 10.0 * dt;
        }
        if let Some(s) = world.resource_mut::<Score>() {
            s.0 += 1;
        }
    }
}

/// A game whose tenth tick panics.
struct Faulty(u32);

fn start_faulty(_player: &mut Player) -> Result<Faulty, Box<dyn std::error::Error>> {
    Ok(Faulty(0))
}

impl Game for Faulty {
    fn tick(&mut self, _world: &mut World, _dt: f32) {
        self.0 += 1;
        assert!(self.0 < 10, "the tenth tick");
    }
}

/// One editor frame, as the editor's loop runs it.
fn frame<G: Game>(player: &mut Player, game: &mut EditorGame<G>, overlay: &mut EditorOverlay) {
    player.frame_overlaid(game, Some(overlay)).unwrap();
    player.set_view_rect(game.view_rect());
    game.sync_assets(player);
}

/// The scene: a floor, a crate, a door on a tween (up 2 m in a second), the sun, and the
/// game's camera. Its text.
fn write_scene(dir: &Path) -> String {
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.55, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Crate"),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Door"),
        Transform::from_xyz(-2.0, 0.0, 0.0),
        Model::new(builtin::CUBE, builtin::MATERIAL),
        Tween::new("Transform.translation.y", &[0.0], &[2.0], 1.0),
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
    text
}

/// A player for the scene in `dir`, or `None` without a GPU (unless one is required).
fn player(dir: &Path, title: &str) -> Option<(Player, PlayerSettings)> {
    let settings = PlayerSettings {
        title: String::from(title),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.to_path_buf()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        // The game's input, which an eject takes the keyboard from.
        input: Some(engine_input::InputMap::new()),
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

/// The cameras in the world: the scene's, and the editor's while editing.
fn cameras(world: &mut World) -> Vec<i32> {
    let mut q = QueryState::<&Camera>::new(world);
    let mut priorities: Vec<i32> = q.iter_readonly(world).map(|c| c.priority).collect();
    priorities.sort_unstable();
    priorities
}

/// The entities without a scene id: the editor's own, and the game's while playing.
fn unnamed(world: &mut World) -> usize {
    let mut q = QueryState::<engine_ecs::Entity>::new(world);
    let all: Vec<Entity> = q.iter_readonly(world).collect();
    all.iter()
        .filter(|&&e| world.get::<SceneId>(e).is_none())
        .count()
}

#[test]
fn play_runs_the_game_on_the_scene_and_stop_puts_it_back() {
    let dir = temp_dir("play");
    let text = write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor play test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..30 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let world = player.world_mut();
    let door = find_named(world, "Door").unwrap();
    let crate_ = find_named(world, "Crate").unwrap();
    // Editing: the world stands, so the door's tween waits for Play.
    assert_eq!(world.get::<Transform>(door).unwrap().translation.y, 0.0);
    assert_eq!(world.get::<Tween>(door).unwrap().elapsed, 0.0);
    assert_eq!(
        cameras(world),
        [0, i32::MAX],
        "the scene's and the editor's"
    );

    // Play: the game made on the scene's world, which runs.
    assert!(game.run_command(player.world_mut(), "play.play"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.playing());
    let entered = game.message().unwrap();
    assert!(entered.starts_with("Play started in"), "{entered}");
    println!("{entered}");
    for _ in 0..30 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let world = player.world_mut();
    let x = world.get::<Transform>(crate_).unwrap().translation.x;
    assert!(x > 0.3, "the crate slid: {x}");
    let y = world.get::<Transform>(door).unwrap().translation.y;
    assert!(y > 0.5, "the door's tween runs: {y}");
    assert!(world.resource::<Score>().is_some_and(|s| s.0 >= 20));
    assert_eq!(cameras(world), [0], "the game's camera alone");
    assert_eq!(unnamed(world), 1, "the shot");
    if let Ok(path) = std::env::var("ENGINE_EDITOR_PLAY_PNG") {
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let extent = player.extent();
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // Saving waits for Stop: the play world is not the scene.
    assert!(game.run_command(player.world_mut(), "file.save"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.message().unwrap().contains("F8 stops it first"));
    assert_eq!(
        std::fs::read_to_string(dir.join("main.scene")).unwrap(),
        text
    );

    // Stop: the scene as it was, the game's shot, score and camera override gone.
    assert!(game.run_command(player.world_mut(), "play.stop"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(!game.playing());
    let stopped = game.message().unwrap();
    assert!(stopped.starts_with("Stopped"), "{stopped}");
    frame(&mut player, &mut game, &mut overlay);
    let world = player.world_mut();
    let crate_ = find_named(world, "Crate").unwrap();
    let door = find_named(world, "Door").unwrap();
    assert_eq!(world.get::<Transform>(crate_).unwrap().translation.x, 0.0);
    assert_eq!(world.get::<Transform>(door).unwrap().translation.y, 0.0);
    assert_eq!(world.get::<Tween>(door).unwrap().elapsed, 0.0);
    assert!(world.resource::<Score>().is_none());
    assert_eq!(cameras(world), [0, i32::MAX], "the editor's camera is back");
    assert_eq!(unnamed(world), 1, "the editor's camera alone");
    // The world still stands, and nothing is left to save.
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let world = player.world_mut();
    assert_eq!(world.get::<Transform>(door).unwrap().translation.y, 0.0);
    assert!(!game.unsaved());
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_mixers_changes_in_play_stay_as_one_step_that_takes_them_back() {
    let dir = temp_dir("play-mixer");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor play mixer test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let has_bus = |player: &mut Player| {
        let mixer = player.world_mut().resource::<engine_app::Mixer>().unwrap();
        mixer.buses().any(|b| b == "bus")
    };
    // Play, and a bus added while it plays: the game has it at once.
    assert!(game.run_command(player.world_mut(), "play.play"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.playing());
    assert!(game.run_command(player.world_mut(), "audio.add_bus"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(has_bus(&mut player), "heard in Play");
    // Stop: it stays, the world put back around it.
    assert!(game.run_command(player.world_mut(), "play.stop"));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(!game.playing());
    assert!(has_bus(&mut player), "kept after Stop");
    // One step for Play's changes: undone, the mixer as Play began; redone, the bus again.
    assert!(game.run_command(player.world_mut(), "edit.undo"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(!has_bus(&mut player), "undone");
    assert!(game.run_command(player.world_mut(), "edit.redo"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(has_bus(&mut player), "redone");
    // The command takes them back too.
    assert!(game.run_command(player.world_mut(), "audio.take_back_play"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(!has_bus(&mut player), "taken back");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_in_the_games_tick_stops_play_and_says_where() {
    let dir = temp_dir("play-panic");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor play panic test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, start_faulty).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..5 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..20 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(!game.playing(), "the panic stopped Play");
    let message = game.message().unwrap();
    assert!(
        message.contains("panicked in `tick`") && message.contains("the tenth tick"),
        "{message}"
    );
    assert!(message.contains("play.rs"), "where: {message}");
    // The editor goes on.
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(cameras(player.world_mut()), [0, i32::MAX]);
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The scene's crate's x and the score.
fn state(world: &mut World) -> (f32, u32) {
    let crate_ = find_named(world, "Crate").unwrap();
    let x = world.get::<Transform>(crate_).unwrap().translation.x;
    (x, world.resource::<Score>().map_or(0, |s| s.0))
}

#[test]
fn pause_stands_the_game_still_and_a_step_runs_one_tick() {
    let dir = temp_dir("play-pause");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor pause test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    // F7: the world stands, the game's hooks wait.
    assert!(game.run_command(player.world_mut(), "play.pause"));
    frame(&mut player, &mut game, &mut overlay);
    let (x, score) = state(player.world_mut());
    assert!(x > 0.1 && score > 5, "{x} {score}");
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(state(player.world_mut()), (x, score), "paused");
    assert!(game.playing() && game.message().unwrap().starts_with("Paused"));
    // Shift+F7: one tick, then paused again.
    assert!(game.run_command(player.world_mut(), "play.step"));
    for _ in 0..5 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let (stepped, after) = state(player.world_mut());
    assert_eq!(after, score + 1, "one tick");
    let dt = (1.0 / settings.tick_hz) as f32;
    assert!((stepped - x - dt).abs() < 1e-4, "{stepped} after {x}");
    // F7 again: on.
    assert!(game.run_command(player.world_mut(), "play.pause"));
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let (on, later) = state(player.world_mut());
    assert!(on > stepped && later > after + 5, "{on} {later}");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restart_plays_again_from_the_scene() {
    let dir = temp_dir("play-restart");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor restart test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..30 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let (x, score) = state(player.world_mut());
    assert!(x > 0.3 && score > 20);
    // Ctrl+F5: stopped and played again, the game made anew on the scene.
    assert!(game.run_command(player.world_mut(), "play.restart"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.playing());
    frame(&mut player, &mut game, &mut overlay);
    let (x, score) = state(player.world_mut());
    assert!(x < 0.05 && score <= 2, "from the start: {x} {score}");
    assert_eq!(unnamed(player.world_mut()), 1, "one shot, the new game's");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn play_from_here_starts_the_scenes_camera_at_the_editors() {
    let dir = temp_dir("play-here");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor play from here test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    // Where the editor's camera is (the camera above every other).
    let world = player.world_mut();
    let mut q = QueryState::<(&Camera, &Transform)>::new(world);
    let editor_eye = q
        .iter_readonly(world)
        .find(|(c, _)| c.priority == i32::MAX)
        .map(|(_, t)| t.translation)
        .unwrap();
    let camera = find_named(world, "Camera").unwrap();
    let scene_eye = world.get::<Transform>(camera).unwrap().translation;
    assert!(
        editor_eye.distance(scene_eye) > 1.0,
        "{editor_eye} {scene_eye}"
    );
    // Shift+F5: the scene's camera starts there, and the game is told where.
    assert!(game.run_command(player.world_mut(), "play.from_here"));
    frame(&mut player, &mut game, &mut overlay);
    let world = player.world_mut();
    let at = world.get::<Transform>(camera).unwrap().translation;
    assert!(at.distance(editor_eye) < 1e-3, "{at} {editor_eye}");
    let from = *world.resource::<engine_app::PlayFrom>().unwrap();
    assert!(from.position.distance(editor_eye) < 1e-3);
    // Stop: both as they were.
    assert!(game.run_command(player.world_mut(), "play.stop"));
    frame(&mut player, &mut game, &mut overlay);
    let world = player.world_mut();
    assert_eq!(
        world.get::<Transform>(camera).unwrap().translation,
        scene_eye
    );
    assert!(world.resource::<engine_app::PlayFrom>().is_none());
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn eject_gives_the_editor_its_camera_on_the_running_game() {
    let dir = temp_dir("play-eject");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor eject test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(cameras(player.world_mut()), [0]);
    // Shift+F8: the editor's camera back, the game running on without the keyboard.
    assert!(game.run_command(player.world_mut(), "play.eject"));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let world = player.world_mut();
    assert_eq!(cameras(world), [0, i32::MAX]);
    let input = world.resource::<engine_input::Input>().unwrap();
    assert_eq!(input.devices(), engine_input::DeviceFilter::NONE);
    let (x, _) = state(world);
    for _ in 0..5 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(state(player.world_mut()).0 > x, "the game runs on");
    // Shift+F8 again: back in the game, its keyboard with it.
    assert!(game.run_command(player.world_mut(), "play.eject"));
    for _ in 0..2 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let world = player.world_mut();
    assert_eq!(cameras(world), [0]);
    let devices = world.resource::<engine_input::Input>().unwrap().devices();
    assert!(devices.keyboard && devices.mouse);
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Enter pressed and released, as the window reports it.
fn enter<G: Game>(game: &mut EditorGame<G>) {
    for pressed in [true, false] {
        game.event(&WindowEvent {
            window: WindowId(0),
            time: std::time::Instant::now(),
            kind: WindowEventKind::Key {
                virtual_key: 0x0D,
                scancode: 0x1C,
                extended: false,
                pressed,
                repeat: false,
            },
        });
    }
}

/// The scene's entities (with a scene id).
fn scene_entities(world: &mut World) -> usize {
    let mut q = QueryState::<&SceneId>::new(world);
    q.iter_readonly(world).count()
}

#[test]
fn changes_kept_while_playing_stay_after_stop() {
    let dir = temp_dir("play-keep");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor keep test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..20 {
        frame(&mut player, &mut game, &mut overlay);
    }
    // Ctrl+K on the crate: its changes stay at Stop.
    let crate_ = find_named(player.world_mut(), "Crate").unwrap();
    game.select(Some(crate_));
    assert!(game.run_command(player.world_mut(), "play.keep"));
    frame(&mut player, &mut game, &mut overlay);
    let said = game.message().unwrap();
    assert!(said.contains("kept at Stop"), "{said}");
    // Paused, so the crate stays where Stop will find it.
    assert!(game.run_command(player.world_mut(), "play.pause"));
    frame(&mut player, &mut game, &mut overlay);
    let (x, _) = state(player.world_mut());
    assert!(x > 0.2, "{x}");
    assert!(game.run_command(player.world_mut(), "play.stop"));
    frame(&mut player, &mut game, &mut overlay);
    let said = game.message().unwrap();
    assert!(said.contains("kept the changes of 1 entity"), "{said}");
    let world = player.world_mut();
    let (kept, _) = state(world);
    assert_eq!(kept, x, "the crate where it was");
    let door = find_named(world, "Door").unwrap();
    assert_eq!(
        world.get::<Transform>(door).unwrap().translation.y,
        0.0,
        "the door was not kept"
    );
    assert!(game.unsaved(), "a change to the scene");
    // One undo step takes it back.
    assert!(game.run_command(player.world_mut(), "edit.undo"));
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(state(player.world_mut()).0, 0.0);
    assert!(!game.unsaved());
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keeping_every_change_keeps_what_the_game_made() {
    let dir = temp_dir("play-keep-all");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor keep all test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    frame(&mut player, &mut game, &mut overlay);
    let before = scene_entities(player.world_mut());
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.run_command(player.world_mut(), "play.pause"));
    frame(&mut player, &mut game, &mut overlay);
    // "Keep Changes…": its first line keeps every change.
    assert!(game.run_command(player.world_mut(), "play.keep_list"));
    frame(&mut player, &mut game, &mut overlay);
    enter(&mut game);
    frame(&mut player, &mut game, &mut overlay);
    let (x, _) = state(player.world_mut());
    assert!(game.run_command(player.world_mut(), "play.stop"));
    frame(&mut player, &mut game, &mut overlay);
    let said = game.message().unwrap();
    assert!(said.contains("kept the changes of 3 entities"), "{said}");
    let world = player.world_mut();
    // The crate's slide, the door's rise, and the shot, now a scene entity.
    assert_eq!(state(world).0, x);
    let door = find_named(world, "Door").unwrap();
    assert!(world.get::<Transform>(door).unwrap().translation.y > 0.1);
    assert_eq!(scene_entities(world), before + 1, "the shot kept");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_game_plays_in_a_window_of_its_own_or_in_the_viewport() {
    let dir = temp_dir("play-window");
    write_scene(&dir);
    let Some((mut player, settings)) = player(&dir, "editor play window test") else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    frame(&mut player, &mut game, &mut overlay);
    // Play: a window of its own (offscreen here: an image), the game's camera in it.
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(player.has_view_window());
    let extent = player.view_window_extent().unwrap();
    assert!(extent.width >= 1280 && extent.height >= 720, "{extent:?}");
    // The game has the mouse there; Shift+F1 frees it and gives it back, pausing frees
    // it until going on.
    assert!(game.view_cursor_locked() && !game.cursor_locked());
    let devices = |player: &mut Player| {
        player
            .world_mut()
            .resource::<engine_input::Input>()
            .unwrap()
            .devices()
    };
    assert!(game.run_command(player.world_mut(), "play.mouse"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(!game.view_cursor_locked());
    // Freed, the game no longer hears the mouse, the keyboard still.
    let freed = devices(&mut player);
    assert!(!freed.mouse && freed.keyboard, "{freed:?}");
    assert!(game.run_command(player.world_mut(), "play.mouse"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.view_cursor_locked());
    assert!(devices(&mut player).mouse, "given back");
    assert!(game.run_command(player.world_mut(), "play.pause"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(!game.view_cursor_locked());
    assert!(game.run_command(player.world_mut(), "play.pause"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.view_cursor_locked());
    let rgba = player
        .capture_view_window(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    // The game's picture: the sky's blue above, the floor's grey below (no panels).
    let at = |x: u32, y: u32| {
        let i = ((y * extent.width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    let [r, g, b] = at(extent.width / 2, 4);
    assert!(b > r && b > 60.0, "sky {r} {g} {b}");
    if let Ok(path) = std::env::var("ENGINE_EDITOR_PLAY_WINDOW_PNG") {
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // Stop: the window goes, and the mouse is the editor's.
    assert!(game.run_command(player.world_mut(), "play.stop"));
    frame(&mut player, &mut game, &mut overlay);
    assert!(!player.has_view_window());
    assert!(!game.view_cursor_locked() && !game.cursor_locked());
    // In the viewport instead: no window, the game has the mouse over the editor's.
    assert!(game.run_command(player.world_mut(), "play.where"));
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.playing() && !player.has_view_window());
    assert!(game.cursor_locked() && !game.view_cursor_locked());
    // The window losing the keyboard frees it; a click in the game gives it back.
    let send = |game: &mut engine_editor::EditorGame<_>, kind| {
        game.event(&WindowEvent {
            window: WindowId(0),
            time: std::time::Instant::now(),
            kind,
        });
    };
    send(&mut game, WindowEventKind::Focused(false));
    assert!(!game.cursor_locked());
    send(&mut game, WindowEventKind::Focused(true));
    let [x, y, w, h] = game.view_rect().unwrap();
    send(
        &mut game,
        WindowEventKind::CursorMoved {
            x: (x + w / 2) as i32,
            y: (y + h / 2) as i32,
        },
    );
    send(
        &mut game,
        WindowEventKind::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        },
    );
    frame(&mut player, &mut game, &mut overlay);
    assert!(
        game.cursor_locked(),
        "a click in the game gives it the mouse"
    );
    send(
        &mut game,
        WindowEventKind::MouseButton {
            button: MouseButton::Left,
            pressed: false,
        },
    );
    // Ejected, the editor's; back in the game, the game's.
    assert!(game.run_command(player.world_mut(), "play.eject"));
    assert!(!game.cursor_locked());
    assert!(game.run_command(player.world_mut(), "play.eject"));
    assert!(game.cursor_locked());
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
