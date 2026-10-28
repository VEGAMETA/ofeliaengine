//! Scripts in the editor (ADR 0114 E7-4a): New Script makes a file named after the selected
//! entity, which the player's script host loads at the next frame and whose component goes
//! to the entity as one step; Attach Script gives it a component a script of the project
//! declares, whose hooks run in Play.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Assets, Game, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind, WindowId};
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

/// A click on the middle of where the last frame drew `label`, then two frames.
fn click_text<G: Game>(
    player: &mut Player,
    game: &mut EditorGame<G>,
    overlay: &mut EditorOverlay,
    label: &str,
) {
    let [x, y] = game
        .text_at(label)
        .unwrap_or_else(|| panic!("`{label}` is not drawn"));
    send(
        game,
        WindowEventKind::CursorMoved {
            x: x as i32,
            y: y as i32,
        },
    );
    for pressed in [true, false] {
        send(
            game,
            WindowEventKind::MouseButton {
                button: MouseButton::Left,
                pressed,
            },
        );
        frame(player, game, overlay);
    }
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
        debuggable_scripts: true,
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

/// A key pressed and released.
fn tap<G: Game>(game: &mut EditorGame<G>, virtual_key: u16, scancode: u16) {
    for pressed in [true, false] {
        send(
            game,
            WindowEventKind::Key {
                virtual_key,
                scancode,
                extended: false,
                pressed,
                repeat: false,
            },
        );
    }
}

#[test]
fn a_logpoint_logs_in_play_and_f9_stops_the_script_at_its_line() {
    let dir = temp_dir("breakpoints");
    write_project(&dir, "Front door", true);
    // The last session left the script's tab shown, the caret on line 3 (`local t`), and
    // a logpoint on line 4 (ADR 0119).
    std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\nscript = 0\n\n[script]\npath = \
         \"scripts/spin.luau\"\ncaret = (2, 4)\nscroll = (0, 0)\n\n[breakpoint]\npath = \
         \"scripts/spin.luau\"\nline = 4\nlog = \"rate {self.rate}\"\n",
    )
    .unwrap();
    let Some((mut player, settings)) = player(&dir) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    game.begin_session(&mut player).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let door = find_named(player.world_mut(), "Front door").unwrap();
    assert!(player.add_script(door, "EditorSpin"));
    // Editing: no hook runs, nothing logged.
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(game.breakpoint_note(4), None);
    // Play: the logpoint's latest message after its line, with its hits.
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let note = game.breakpoint_note(4).unwrap_or_default();
    assert!(note.starts_with("rate 2  ×"), "{note}");
    let hits: u64 = note.rsplit('×').next().unwrap().parse().unwrap();
    assert!(hits >= 5, "{note}");
    if let Ok(path) = std::env::var("ENGINE_EDITOR_BREAKPOINTS_PNG") {
        let extent = player.extent();
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // F9 on the caret's line: a breakpoint; the next tick stops there, the script's tab in
    // front, the arrow and the band on the line, the frame's values after it.
    tap(&mut game, 0x78, 0x43);
    for _ in 0..4 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let said = game.message().unwrap_or_default();
    assert!(
        said.starts_with("Stopped at scripts/spin.luau:3 in on_tick"),
        "{said}"
    );
    assert_eq!(game.stopped_line(), Some(3));
    let values = game.breakpoint_note(3).unwrap_or_default();
    assert!(
        values.starts_with("self = ") && values.contains("dt = "),
        "{values}"
    );
    // The Debugger panel opened by itself: the stop, its stack, its frame's values, the
    // breakpoints.
    for drawn in [
        "Stopped · on_tick · scripts/spin.luau:3",
        "CALL STACK",
        "on_tick",
        "spin.luau:3",
        "LOCALS",
        "dt",
        "spin.luau:4",
        "t.rotation = quat.axis_angle(vec\u{2026}",
    ] {
        assert!(game.text_at(drawn).is_some(), "`{drawn}` in the Debugger");
    }
    // Its Over button steps to the next line (the logpoint's), then F10 to the hook's end;
    // F5 goes on, and the next tick stops at the breakpoint again.
    for line in [4, 5] {
        if line == 4 {
            click_text(&mut player, &mut game, &mut overlay, "Over F10");
        } else {
            tap(&mut game, 0x79, 0x44);
        }
        for _ in 0..3 {
            frame(&mut player, &mut game, &mut overlay);
        }
        let said = game.message().unwrap_or_default();
        assert!(
            said.starts_with(&format!("Stopped at scripts/spin.luau:{line} in on_tick")),
            "{said}"
        );
        assert_eq!(game.stopped_line(), Some(line));
    }
    let values = game.breakpoint_note(5).unwrap_or_default();
    assert!(values.contains("t = "), "a local made since: {values}");
    // The lines above it show what they assigned: the parameters on the function's line,
    // `t` after line 3 (beside its breakpoint's hits).
    for drawn in ["self = Self", "dt = 0.015625", "t = View"] {
        assert!(game.text_at(drawn).is_some(), "`{drawn}` on the lines above the stop");
    }
    tap(&mut game, 0x74, 0x3F);
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(game.stopped_line(), Some(3), "the next tick");
    if let Ok(path) = std::env::var("ENGINE_EDITOR_STOP_PNG") {
        let extent = player.extent();
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // Stopped: the world kept still but for the editor's edits.
    let rotation =
        |player: &mut Player| player.world_mut().get::<Transform>(door).unwrap().rotation;
    let still = rotation(&mut player);
    for _ in 0..5 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(rotation(&mut player), still, "the world kept still");
    assert!(player.script_stop().is_some() && game.playing());
    player
        .world_mut()
        .get_mut::<Transform>(door)
        .unwrap()
        .translation
        .y = 2.0;
    // Ctrl+F9 turns the breakpoint off, F5 goes on: the tick ends, the door turns again.
    let ctrl = |game: &mut EditorGame<Still>, pressed: bool| {
        send(
            game,
            WindowEventKind::Key {
                virtual_key: 0x11,
                scancode: 0x1D,
                extended: false,
                pressed,
                repeat: false,
            },
        );
    };
    ctrl(&mut game, true);
    tap(&mut game, 0x78, 0x43);
    ctrl(&mut game, false);
    frame(&mut player, &mut game, &mut overlay);
    tap(&mut game, 0x74, 0x3F);
    for _ in 0..6 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(player.script_stop().is_none(), "gone on");
    assert_eq!(game.stopped_line(), None);
    assert_ne!(rotation(&mut player), still, "it turns again");
    let y = player
        .world_mut()
        .get::<Transform>(door)
        .unwrap()
        .translation
        .y;
    assert_eq!(y, 2.0, "the edit at the stop stays");
    // Alt+F9: its options under the line, the keys to its condition; typed, Enter keeps it.
    let alt = |game: &mut EditorGame<Still>, pressed: bool| {
        send(
            game,
            WindowEventKind::Key {
                virtual_key: 0x12,
                scancode: 0x38,
                extended: false,
                pressed,
                repeat: false,
            },
        );
    };
    alt(&mut game, true);
    tap(&mut game, 0x78, 0x43);
    alt(&mut game, false);
    for _ in 0..2 {
        frame(&mut player, &mut game, &mut overlay);
    }
    for c in "self.rate > 1".chars() {
        send(&mut game, WindowEventKind::Char(c));
    }
    frame(&mut player, &mut game, &mut overlay);
    if let Ok(path) = std::env::var("ENGINE_EDITOR_BREAK_OPTIONS_PNG") {
        let extent = player.extent();
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    enter(&mut game);
    frame(&mut player, &mut game, &mut overlay);
    let said = game.message().unwrap_or_default();
    assert!(
        said.starts_with("Breakpoint at scripts/spin.luau:3"),
        "{said}"
    );
    // On again: its condition holds, the script stops; F8 stops Play there, the stopped
    // call ending without an error, the world put back.
    ctrl(&mut game, true);
    tap(&mut game, 0x78, 0x43);
    ctrl(&mut game, false);
    for _ in 0..4 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(player.script_stop().is_some(), "stopped again");
    tap(&mut game, 0x77, 0x42);
    for _ in 0..2 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(!game.playing() && player.script_stop().is_none());
    let said = game.message().unwrap_or_default();
    assert!(!said.contains("error"), "{said}");
    let y = player
        .world_mut()
        .get::<Transform>(door)
        .unwrap()
        .translation
        .y;
    assert_eq!(y, 0.0, "the world as Play found it");
    // Kept in the session: a breakpoint that stops says no more than its condition.
    game.end_session(player.world_mut());
    let session = std::fs::read_to_string(dir.join(".ofelia/session")).unwrap();
    assert!(
        session.contains("line = 3\ncondition = \"self.rate > 1\"\n"),
        "{session}"
    );
    assert!(
        session.contains("line = 4\nlog = \"rate {self.rate}\""),
        "{session}"
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A script whose hook fails at its line 4.
const FAULTY: &str = r#"local Faulty = component("Faulty", { rate = 2 })
function Faulty.on_tick(self, dt: number)
    local target = nil
    local x = target.x
end
return Faulty
"#;

#[test]
fn an_error_stops_play_at_its_line_once_and_f5_lets_it_take_its_course() {
    let dir = temp_dir("errors");
    write_project(&dir, "Front door", true);
    std::fs::write(dir.join("scripts/faulty.luau"), FAULTY).unwrap();
    let Some((mut player, settings)) = player(&dir) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    game.begin_session(&mut player).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    // Two instances of the failing script: a bug in one line stops once.
    let door = find_named(player.world_mut(), "Front door").unwrap();
    let sun = find_named(player.world_mut(), "Sun").unwrap();
    assert!(player.add_script(door, "Faulty"));
    assert!(player.add_script(sun, "Faulty"));
    frame(&mut player, &mut game, &mut overlay);
    // Play: the first instance's error stops at its line (errors not caught stop by
    // default), the script in front, the line in the error's colors with the message.
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..6 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let said = game.message().unwrap_or_default();
    assert!(
        said.starts_with("Error at scripts/faulty.luau:4 in on_tick: attempt to index nil"),
        "{said}"
    );
    assert_eq!(game.stopped_line(), Some(4));
    let note = game.breakpoint_note(4).unwrap_or_default();
    assert!(note.starts_with("attempt to index nil"), "{note}");
    let stop = player.script_stop().cloned().unwrap();
    let error = stop.error.clone().unwrap();
    assert!(!error.caught);
    for drawn in [
        format!(
            "Error · on_tick · scripts/faulty.luau:4 · {}",
            error.message
        )
        .as_str(),
        "Errors the script does not catch",
        "Errors caught by pcall",
    ] {
        assert!(game.text_at(drawn).is_some(), "`{drawn}` in the Debugger");
    }
    if let Ok(path) = std::env::var("ENGINE_EDITOR_ERROR_PNG") {
        let extent = player.extent();
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // F5: the error takes its course; the other instance's error at the same line does
    // not stop again.
    tap(&mut game, 0x74, 0x3F);
    for _ in 0..6 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(player.script_stop().is_none(), "gone on");
    assert_eq!(game.stopped_line(), None);
    assert!(game.playing());
    // A break at the next line: the spinning script's first line stops, and F5 goes on.
    assert!(player.add_script(door, "EditorSpin"));
    assert!(game.run_command(player.world_mut(), "debug.break_next"));
    for _ in 0..4 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let said = game.message().unwrap_or_default();
    assert!(
        said.starts_with("Stopped at scripts/spin.luau:3 in on_tick"),
        "{said}"
    );
    tap(&mut game, 0x74, 0x3F);
    for _ in 0..4 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(player.script_stop().is_none(), "gone on");
    // Stopped; the Debugger's row turns errors not caught off: the next Play's error goes
    // to the Output, the game going on.
    tap(&mut game, 0x77, 0x42);
    for _ in 0..2 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(!game.playing());
    click_text(
        &mut player,
        &mut game,
        &mut overlay,
        "Errors the script does not catch",
    );
    let said = game.message().unwrap_or_default();
    assert!(
        said.starts_with("Errors scripts do not catch go to the Output"),
        "{said}"
    );
    assert!(game.run_command(player.world_mut(), "play.play"));
    for _ in 0..6 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(player.script_stop().is_none() && game.playing());
    tap(&mut game, 0x77, 0x42);
    frame(&mut player, &mut game, &mut overlay);
    // Kept in the session.
    game.end_session(player.world_mut());
    let session = std::fs::read_to_string(dir.join(".ofelia/session")).unwrap();
    assert!(
        session.contains("error_stops = (false, false)"),
        "{session}"
    );
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
