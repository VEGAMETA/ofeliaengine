//! The editor around a game, offscreen (ADR 0107): a scene file opened, the shell painted
//! over the frame, the scene drawn into the viewport panel from the editor's camera; then
//! edited on the player (duplicate, move, undo) and saved back. `ENGINE_EDITOR_PNG=<file>`
//! writes the first frame, `ENGINE_EDITOR_PNG2` the edited one.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{
    Assets, Brush, BrushGroup, BrushOp, BrushShape, Game, MaterialFile, Model, Player,
    PlayerSettings, Window, builtin,
};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind, WindowId};
use engine_runtime::{Camera, PointLight, SpotLight, Sun};
use engine_scene::{AssetId, AssetRef, Name, SaveOptions, SceneId, SceneRegistry, save_with};

struct NoGame;

impl Game for NoGame {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

#[test]
fn the_editor_draws_its_scene_in_the_viewport() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-app-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let red_id = AssetId::new();
    let red = MaterialFile {
        base_color: [0.8, 0.1, 0.1, 1.0],
        ..MaterialFile::default()
    };
    std::fs::write(dir.join("red.material"), red.to_text(red_id)).unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.55, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Cube"),
        Transform::from_scale(Vec3::splat(2.0)),
        Model::new(builtin::CUBE, AssetRef::file(red_id)),
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
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(AssetRef::file(red_id), "red.material");
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();

    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor test"),
        window: Window::Offscreen { width, height },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..30 {
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
        player.set_view_rect(game.view_rect());
    }
    let rgba = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    if let Ok(path) = std::env::var("ENGINE_EDITOR_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let [x, y, w, h] = game.view_rect().unwrap();
    assert!(
        w > width / 3 && h > height / 3 && x + w <= width && y + h <= height,
        "the viewport panel {x} {y} {w} {h}"
    );
    let pixel = |rgba: &[u8], px: u32, py: u32| {
        let i = ((py * width + px) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    let red_in_viewport = |rgba: &[u8]| {
        let mut red = 0;
        for py in (y..y + h).step_by(4) {
            for px in (x..x + w).step_by(4) {
                let [r, g, b] = pixel(rgba, px, py);
                if r > 1.6 * g && r > 1.6 * b && r > 40.0 {
                    red += 1;
                }
            }
        }
        red
    };
    // The red cube is in the viewport, drawn by the editor's camera.
    let red = red_in_viewport(&rgba);
    assert!(red > 50, "red pixels in the viewport: {red}");
    // Outside the viewport the shell's panels: none of the scene's red.
    let mut red_outside = 0;
    for py in (0..height).step_by(8) {
        for px in (0..width).step_by(8) {
            let inside = px >= x && px < x + w && py >= y && py < y + h;
            let [r, g, b] = pixel(&rgba, px, py);
            if !inside && r > 1.6 * g && r > 1.6 * b && r > 40.0 {
                red_outside += 1;
            }
        }
    }
    assert!(
        red_outside < 10,
        "red pixels outside the viewport: {red_outside}"
    );

    // Editing on the player: select the cube, duplicate it (Ctrl+D's command) and move
    // the copy beside it; the inspector shows the copy (`ENGINE_EDITOR_PNG2`).
    let world = player.world_mut();
    let mut names = engine_ecs::QueryState::<(engine_ecs::Entity, &Name)>::new(world);
    let cube = names
        .iter_readonly(world)
        .find(|(_, n)| n.as_str() == "Cube")
        .map(|(e, _)| e)
        .unwrap();
    game.select(Some(cube));
    assert!(!game.unsaved());
    assert!(game.run_command(world, "scene.duplicate"));
    let copy = game.selected().unwrap();
    assert_ne!(copy, cube);
    game.set(world, copy, "Transform.translation", "(2.6, 0, 0)")
        .unwrap();
    assert!(game.unsaved());
    let rgba = frames(&mut player, &mut game, &mut overlay);
    if let Ok(path) = std::env::var("ENGINE_EDITOR_PNG2") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let two = red_in_viewport(&rgba);
    assert!(
        two > red * 3 / 2,
        "the copy beside the cube: {two} red pixels, {red} before"
    );
    // Undo both steps: the copy is gone, and the scene is as it was saved.
    let world = player.world_mut();
    assert!(game.run_command(world, "edit.undo"));
    assert!(game.run_command(world, "edit.undo"));
    assert!(!game.unsaved());
    let rgba = frames(&mut player, &mut game, &mut overlay);
    let one = red_in_viewport(&rgba);
    assert!(
        one * 10 < red * 12 && one * 12 > red * 10,
        "undone: {one} red pixels, {red} at first"
    );
    // Redo the duplicate and save: the file holds both cubes under the scene's id.
    let world = player.world_mut();
    assert!(game.run_command(world, "edit.redo"));
    assert!(game.run_command(world, "file.save"));
    assert!(!game.unsaved());
    let saved = std::fs::read_to_string(dir.join("main.scene")).unwrap();
    assert!(
        saved.starts_with(&text[..text.find('\n').unwrap()]),
        "the same header"
    );
    // The copy is numbered among its siblings.
    assert_eq!(saved.matches("Name = \"Cube\"").count(), 1, "{saved}");
    assert_eq!(saved.matches("Name = \"Cube 2\"").count(), 1, "{saved}");
    assert!(saved.contains("asset(\"red.material\""), "{saved}");

    // HDR (ADR 0038): on a display in HDR the panels are drawn in scRGB at Windows' SDR
    // white level (200 nits here: 2.5 in scRGB units), the same colors as in SDR.
    let (px, py) = (width - 12, height - 60);
    let sdr = frames(&mut player, &mut game, &mut overlay);
    let i = ((py * width + px) * 4) as usize;
    let srgb_byte = f32::from(sdr[i]);
    player.assume_display(engine_app::DisplayInfo {
        monitor: 0,
        refresh_hz: None,
        hdr_supported: true,
        hdr_enabled: true,
        sdr_white_nits: 200.0,
        max_nits: 1000.0,
        max_full_frame_nits: 600.0,
        min_nits: 0.05,
    });
    let hdr = frames(&mut player, &mut game, &mut overlay);
    assert_eq!(player.levels().color, engine_app::OutputColor::ScRgb);
    assert_eq!(hdr.len(), (width * height * 8) as usize);
    let i = ((py * width + px) * 8) as usize;
    let value = engine_core::half::to_f32(u16::from_le_bytes([hdr[i], hdr[i + 1]]));
    let c = srgb_byte / 255.0;
    let linear = if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    };
    assert!(
        (value - linear * 2.5).abs() <= 0.15 * linear * 2.5 + 0.002,
        "the dock in scRGB: {value}, from SDR's {srgb_byte} ({} expected)",
        linear * 2.5
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Picking on the player (ADR 0109): the pointer over the red cube drawn in the
/// viewport hovers it, a click there selects it through the pick of its pixel and
/// outlines it (`ENGINE_EDITOR_PICK_PNG=<file>` writes that frame), and a click on the
/// empty sky clears the selection. Clicking the cube again cycles to a cube hidden inside
/// it, then back. A box over the view takes the outer cube, with Alt both
/// (`ENGINE_EDITOR_BOX_PNG=<file>` writes the frame of the first box being dragged); F
/// then frames them.
#[test]
fn a_click_in_the_viewport_selects_what_is_drawn_there() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-pick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let red_id = AssetId::new();
    let red = MaterialFile {
        base_color: [0.8, 0.1, 0.1, 1.0],
        ..MaterialFile::default()
    };
    std::fs::write(dir.join("red.material"), red.to_text(red_id)).unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Cube"),
        Transform::from_scale(Vec3::splat(2.0)),
        Model::new(builtin::CUBE, AssetRef::file(red_id)),
    ));
    // Inside it, hidden by it from every side.
    world.spawn((
        SceneId::new(),
        Name::new("Inner"),
        Transform::from_scale(Vec3::splat(0.5)),
        Model::new(builtin::CUBE, AssetRef::file(red_id)),
    ));
    // The sun away from the cube: its icon takes a click over it.
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_xyz(4.0, 3.0, -4.0).with_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(AssetRef::file(red_id), "red.material");
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor pick test"),
        window: Window::Offscreen { width, height },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..20 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let cube = engine_scene::find_named(player.world_mut(), "Cube").unwrap();
    let inner = engine_scene::find_named(player.world_mut(), "Inner").unwrap();
    // The red cube's middle on the screen.
    let rgba = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    let [x, y, w, h] = game.view_rect().unwrap();
    let (mut sum, mut n) = ((0u64, 0u64), 0u64);
    for py in (y..y + h).step_by(2) {
        for px in (x..x + w).step_by(2) {
            let i = ((py * width + px) * 4) as usize;
            let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from);
            if r > 1.6 * g && r > 1.6 * b && r > 40.0 {
                sum = (sum.0 + u64::from(px), sum.1 + u64::from(py));
                n += 1;
            }
        }
    }
    assert!(n > 50, "the cube is drawn: {n} red pixels");
    // The gap right of the viewport, before the scene's dock: the window's color (sRGB 18,
    // 18, 22), not what the output's memory held.
    let i = (((y + h / 2) * width + x + w) * 4) as usize;
    let gap = [rgba[i], rgba[i + 1], rgba[i + 2]];
    assert!(
        gap.iter()
            .zip([18u8, 18, 22])
            .all(|(&a, b)| a.abs_diff(b) <= 2),
        "the gap beside the viewport: {gap:?}"
    );
    let on_cube = ((sum.0 / n) as i32, (sum.1 / n) as i32);
    send(
        &mut game,
        WindowEventKind::CursorMoved {
            x: on_cube.0,
            y: on_cube.1,
        },
    );
    for _ in 0..6 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(game.hovered(), Some(cube), "hovered at {on_cube:?}");
    assert_eq!(game.selected(), None);
    left_click(&mut player, &mut game, &mut overlay);
    assert_eq!(game.selection(), [cube], "clicked at {on_cube:?}");
    // Its outline in the active color (sRGB about 255, 206, 118) around it.
    let rgba = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    if let Ok(path) = std::env::var("ENGINE_EDITOR_PICK_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let mut outline = 0;
    for py in y..y + h {
        for px in x..x + w {
            let i = ((py * width + px) * 4) as usize;
            let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]];
            if r >= 235 && (175..=235).contains(&g) && (80..=160).contains(&b) {
                outline += 1;
            }
        }
    }
    assert!(outline > 100, "outlined pixels: {outline}");
    // A click there again: the cube inside, which the outer one hides; then round to the
    // outer one. (With the rotate tool: the select tool's gizmo, at the cube's middle, would
    // take the press.)
    for (virtual_key, scancode, pressed) in [
        (0x10, 0x2A, true),
        (0x52, 0x13, true),
        (0x52, 0x13, false),
        (0x10, 0x2A, false),
    ] {
        send(
            &mut game,
            WindowEventKind::Key {
                virtual_key,
                scancode,
                extended: false,
                pressed,
                repeat: false,
            },
        );
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    left_click(&mut player, &mut game, &mut overlay);
    assert_eq!(game.selection(), [inner], "cycled to the one behind");
    left_click(&mut player, &mut game, &mut overlay);
    assert_eq!(game.selection(), [cube], "round to the nearest");
    // The sky: the viewport's top left, away from the cube.
    send(
        &mut game,
        WindowEventKind::CursorMoved {
            x: (x + 12) as i32,
            y: (y + 40) as i32,
        },
    );
    for _ in 0..6 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(game.hovered(), None);
    left_click(&mut player, &mut game, &mut overlay);
    assert!(game.selection().is_empty(), "a click on nothing clears it");
    // A box over the whole view takes what shows: the outer cube; with Alt the one inside
    // too, the nearest active.
    let corners = (
        ((x + w - 20) as i32, (y + 20) as i32),
        ((x + 20) as i32, (y + h - 20) as i32),
    );
    drag_box(&mut player, &mut game, &mut overlay, corners, false);
    assert_eq!(game.selection(), [cube], "what shows in the box");
    drag_box(&mut player, &mut game, &mut overlay, corners, true);
    assert_eq!(game.selection(), [inner, cube], "hidden ones too");
    // F frames them: the cube in the view's middle, larger.
    let red = |rgba: &[u8]| {
        let (mut sum, mut n) = ((0u64, 0u64), 0u64);
        for py in (y..y + h).step_by(2) {
            for px in (x..x + w).step_by(2) {
                let i = ((py * width + px) * 4) as usize;
                let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from);
                if r > 1.6 * g && r > 1.6 * b && r > 40.0 {
                    sum = (sum.0 + u64::from(px), sum.1 + u64::from(py));
                    n += 1;
                }
            }
        }
        ((sum.0 / n.max(1)) as i32, (sum.1 / n.max(1)) as i32, n)
    };
    let before = red(&player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap());
    for pressed in [true, false] {
        send(
            &mut game,
            WindowEventKind::Key {
                virtual_key: 0x46,
                scancode: 0x21,
                extended: false,
                pressed,
                repeat: false,
            },
        );
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    // Its move takes a fifth of a second.
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_millis(400) {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let after = red(&player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap());
    let middle = ((x + w / 2) as i32, (y + h / 2) as i32);
    println!("framed: {before:?} -> {after:?}, the view's middle {middle:?}");
    assert!(
        (after.0 - middle.0).abs() <= 12 && (after.1 - middle.1).abs() <= 12,
        "in the middle: {after:?} {middle:?}"
    );
    assert!(
        after.2 > before.2 * 3 / 2,
        "larger: {before:?} -> {after:?}"
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// End on the player (ADR 0107 E2-5): a cube floating above a floor drops onto it, its
/// bottom on the floor, through the GPU's surface pick looking down; Ctrl+Z puts it back.
#[test]
fn end_drops_the_selection_onto_the_floor() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-drop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -1.0, 0.0).with_scale(Vec3::splat(10.0)),
        Model::new(builtin::PLANE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Cube"),
        Transform::from_xyz(0.5, 3.0, -0.5),
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
    let paths = Assets::new(dir.clone()).paths().clone();
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("editor drop test"),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..20 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let cube = engine_scene::find_named(player.world_mut(), "Cube").unwrap();
    game.select(Some(cube));
    let key = |game: &mut EditorGame<NoGame>, virtual_key, scancode, extended, pressed| {
        send(
            game,
            WindowEventKind::Key {
                virtual_key,
                scancode,
                extended,
                pressed,
                repeat: false,
            },
        );
    };
    for _ in 0..3 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    // End.
    for pressed in [true, false] {
        key(&mut game, 0x23, 0x4F, true, pressed);
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    for _ in 0..12 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let y = |player: &mut Player| {
        player
            .world_mut()
            .get::<Transform>(cube)
            .unwrap()
            .translation
    };
    let at = y(&mut player);
    println!("dropped to {at}");
    assert!((at.y - -0.5).abs() < 0.02, "its bottom on the floor: {at}");
    assert!(
        (at.x - 0.5).abs() < 1e-4 && (at.z - -0.5).abs() < 1e-4,
        "straight down: {at}"
    );
    // Ctrl+Z.
    key(&mut game, 0x11, 0x1D, false, true);
    for pressed in [true, false] {
        key(&mut game, 0x5A, 0x2C, false, pressed);
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    key(&mut game, 0x11, 0x1D, false, false);
    editor_frame(&mut player, &mut game, &mut overlay);
    assert!(
        (y(&mut player).y - 3.0).abs() < 1e-4,
        "undone: {}",
        y(&mut player)
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Overlays on the player (ADR 0107 E2-7): a point light, a spot light and a camera show as
/// icons (dark disks with their glyphs) where they are; the selected spot light's cone is
/// drawn too. `ENGINE_EDITOR_OVERLAYS_PNG=<file>` writes the frame.
#[test]
fn lights_and_cameras_show_as_icons() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-icons-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -1.0, 0.0).with_scale(Vec3::splat(10.0)),
        Model::new(builtin::PLANE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Lamp"),
        Transform::from_xyz(-2.0, 0.5, 0.0),
        PointLight::new(Vec3::new(4.0, 3.0, 2.0), 3.0),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Spot"),
        Transform::from_xyz(2.0, 2.0, 0.0).with_rotation(Quat::from_rotation_x(-1.2)),
        SpotLight::new(Vec3::new(2.0, 3.0, 5.0), 4.0, 0.3, 0.5),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_xyz(0.0, 1.0, 3.0),
        Camera::perspective(1.0, 0.1, 100.0),
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
    let paths = Assets::new(dir.clone()).paths().clone();
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor icons test"),
        window: Window::Offscreen { width, height },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..20 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let world = player.world_mut();
    let named: Vec<_> = ["Lamp", "Spot", "Camera", "Sun"]
        .map(|n| engine_scene::find_named(world, n).unwrap())
        .into();
    let spot = named[1];
    game.select(Some(spot));
    for _ in 0..3 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let icons = game.view_icons();
    println!("icons {icons:?}");
    for e in &named {
        assert!(icons.iter().any(|(i, _)| i == e), "{e:?} has an icon");
    }
    let rgba = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    if let Ok(path) = std::env::var("ENGINE_EDITOR_OVERLAYS_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // Each icon: its disk darker than what is around it (it is translucent), its glyph
    // light within a few pixels of its middle.
    let lum = |px: i32, py: i32| {
        let i = ((py as u32 * width + px as u32) * 4) as usize;
        (f32::from(rgba[i]) + f32::from(rgba[i + 1]) + f32::from(rgba[i + 2])) / 3.0
    };
    // The mean brightness between radii `r0` and `r1` around `(x, y)`, and the brightest
    // pixel within `r0`.
    let ring = |x: f32, y: f32, r0: f32, r1: f32| {
        let (mut sum, mut n, mut top) = (0.0, 0.0, 0.0f32);
        for dy in -16..=16 {
            for dx in -16..=16 {
                let d = ((dx * dx + dy * dy) as f32).sqrt();
                let l = lum(x as i32 + dx, y as i32 + dy);
                if d >= r0 && d <= r1 {
                    sum += l;
                    n += 1.0;
                }
                if d < r0 {
                    top = top.max(l);
                }
            }
        }
        (sum / n, top)
    };
    for (e, [x, y]) in &icons {
        // The selected one sits under the gizmo's middle.
        if *e == spot {
            continue;
        }
        let (disk, glyph) = ring(*x, *y, 8.0, 10.0);
        let (around, _) = ring(*x, *y, 14.0, 16.0);
        assert!(disk < around * 0.8, "{e:?}'s disk {disk} against {around}");
        assert!(glyph > 170.0, "{e:?}'s glyph {glyph}");
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The editor's loop: a frame, the view's rectangle, then the pick's entity.
/// Drawing (ADR 0110 E2b-3): B, the pointer over a floor 0.5 m below the ground, three
/// clicks with the height typed between. The first corner lands on the floor that the
/// surface pick finds, not on the ground, and the brush is meshed as it is drawn.
/// `ENGINE_EDITOR_DRAW_PNG=<file>` writes a frame of it being drawn.
#[test]
fn b_draws_a_brush_on_the_floor_under_the_pointer() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-draw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.55, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
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
    let paths = Assets::new(dir.clone()).paths().clone();
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("editor draw test"),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..20 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let point = |game: &mut EditorGame<NoGame>, p: Vec3| {
        let [x, y] = game.on_screen(p).unwrap();
        send(
            game,
            WindowEventKind::CursorMoved {
                x: x.round() as i32,
                y: y.round() as i32,
            },
        );
    };
    let click =
        |player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay| {
            for pressed in [true, false] {
                send(
                    game,
                    WindowEventKind::MouseButton {
                        button: MouseButton::Left,
                        pressed,
                    },
                );
                editor_frame(player, game, overlay);
            }
        };
    // B, and the pointer over the floor until its surface is found.
    for pressed in [true, false] {
        send(
            &mut game,
            WindowEventKind::Key {
                virtual_key: 0x42,
                scancode: 0x30,
                extended: false,
                pressed,
                repeat: false,
            },
        );
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    point(&mut game, Vec3::new(0.5, -0.5, 0.5));
    for _ in 0..8 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    click(&mut player, &mut game, &mut overlay);
    point(&mut game, Vec3::new(2.5, -0.5, 1.5));
    editor_frame(&mut player, &mut game, &mut overlay);
    click(&mut player, &mut game, &mut overlay);
    send(&mut game, WindowEventKind::Char('1'));
    for _ in 0..4 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(player.blockout().meshes(), 1, "meshed as it is drawn");
    if let Ok(path) = std::env::var("ENGINE_EDITOR_DRAW_PNG") {
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
    click(&mut player, &mut game, &mut overlay);
    let brush = engine_scene::find_named(player.world_mut(), "Box Brush").unwrap();
    assert_eq!(game.selected(), Some(brush), "made and selected");
    let world = player.world_mut();
    let (op, size) = {
        let b = world.get::<Brush>(brush).unwrap();
        (b.op, b.size)
    };
    let at = world.get::<Transform>(brush).unwrap().translation;
    println!("drawn: {at}, {size}");
    assert_eq!(op, BrushOp::Add);
    assert!(
        (at.y - -0.5).abs() < 1e-3,
        "on the floor, not the ground: {at}"
    );
    assert!(
        (size - Vec3::new(2.0, 1.0, 1.0)).abs().max_element() < 0.05,
        "{size}"
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A crate brush selected shows its side handles (`ENGINE_EDITOR_HANDLES_PNG=<file>`); then
/// B with the pointer a few pixels from its corner: the new brush's first corner lands on
/// it exactly, through the real surface pick and projection (`ENGINE_EDITOR_LANDED_PNG`).
#[test]
fn a_brushs_handles_show_and_a_drawn_corner_lands_on_its_corner() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-land-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.05, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    let group = world.spawn((
        SceneId::new(),
        Name::new("Blockout"),
        Transform::IDENTITY,
        BrushGroup::default(),
    ));
    let crate_ = world.spawn((
        SceneId::new(),
        Name::new("Crate"),
        Transform::IDENTITY,
        Brush::new(BrushShape::Box, Vec3::new(2.0, 1.0, 2.0)),
    ));
    world.set_parent(crate_, Some(group)).unwrap();
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let paths = Assets::new(dir.clone()).paths().clone();
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("editor landing test"),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..20 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let crate_ = engine_scene::find_named(player.world_mut(), "Crate").unwrap();
    game.select(Some(crate_));
    for _ in 0..4 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let write = |player: &mut Player,
                 game: &mut EditorGame<NoGame>,
                 overlay: &mut EditorOverlay,
                 var: &str| {
        if let Ok(path) = std::env::var(var) {
            let rgba = player
                .capture_overlaid(game, Some(overlay))
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
    };
    write(
        &mut player,
        &mut game,
        &mut overlay,
        "ENGINE_EDITOR_HANDLES_PNG",
    );
    let point = |game: &mut EditorGame<NoGame>, p: Vec3, off: (i32, i32)| {
        let [x, y] = game.on_screen(p).unwrap();
        send(
            game,
            WindowEventKind::CursorMoved {
                x: x.round() as i32 + off.0,
                y: y.round() as i32 + off.1,
            },
        );
    };
    let click =
        |player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay| {
            for pressed in [true, false] {
                send(
                    game,
                    WindowEventKind::MouseButton {
                        button: MouseButton::Left,
                        pressed,
                    },
                );
                editor_frame(player, game, overlay);
            }
        };
    for pressed in [true, false] {
        send(
            &mut game,
            WindowEventKind::Key {
                virtual_key: 0x42,
                scancode: 0x30,
                extended: false,
                pressed,
                repeat: false,
            },
        );
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    // Three pixels off the crate's corner on the floor.
    point(&mut game, Vec3::new(1.0, 0.0, 1.0), (3, -2));
    for _ in 0..8 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    write(
        &mut player,
        &mut game,
        &mut overlay,
        "ENGINE_EDITOR_LANDED_PNG",
    );
    click(&mut player, &mut game, &mut overlay);
    point(&mut game, Vec3::new(3.0, 0.0, 2.5), (0, 0));
    editor_frame(&mut player, &mut game, &mut overlay);
    click(&mut player, &mut game, &mut overlay);
    send(&mut game, WindowEventKind::Char('1'));
    for _ in 0..3 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    click(&mut player, &mut game, &mut overlay);
    let drawn = engine_scene::find_named(player.world_mut(), "Box Brush").unwrap();
    let world = player.world_mut();
    let size = world.get::<Brush>(drawn).unwrap().size;
    let at = world.get::<Transform>(drawn).unwrap().translation;
    println!("drawn at {at}, {size}");
    // Its first corner, half its size back from its base's middle, is the crate's.
    let first = Vec3::new(at.x - size.x * 0.5, at.y, at.z - size.z * 0.5);
    assert!(first.distance(Vec3::new(1.0, 0.0, 1.0)) < 1e-4, "{first}");
    // Blockout: Export to glTF writes the group beside the scene, as the player draws it.
    assert!(game.run_command(player.world_mut(), "blockout.export"));
    editor_frame(&mut player, &mut game, &mut overlay);
    let bytes = std::fs::read(dir.join("blockout.glb")).expect("blockout.glb written");
    let scene = engine_assets::import_gltf(&bytes, None).unwrap();
    assert_eq!(scene.meshes.len(), 1);
    let triangles: usize = scene.meshes[0]
        .primitives
        .iter()
        .map(|p| p.indices.len() / 3)
        .sum();
    assert!(triangles >= 20, "both boxes' faces: {triangles}");
    // View: Colliders shows both brushes' pieces (`ENGINE_EDITOR_COLLIDERS_PNG`).
    assert!(game.run_command(player.world_mut(), "view.colliders"));
    for _ in 0..3 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    write(
        &mut player,
        &mut game,
        &mut overlay,
        "ENGINE_EDITOR_COLLIDERS_PNG",
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// View: Navigation (ADR 0110 E2b-4b): a floor brush and a wall brush with a gap at its
/// end, baked in the background a moment after the overlay turns on; the mesh lies on the
/// floor and goes round the wall. Then the wall moved: baked again
/// (`ENGINE_EDITOR_NAVIGATION_PNG=<file>`).
#[test]
fn view_navigation_bakes_the_brushes_in_the_background() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-nav-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = World::new();
    let group = world.spawn((
        SceneId::new(),
        Name::new("Blockout"),
        Transform::IDENTITY,
        BrushGroup::default(),
    ));
    for (name, at, size) in [
        (
            "Floor",
            Vec3::new(0.0, -1.0, 0.0),
            Vec3::new(12.0, 1.0, 12.0),
        ),
        ("Wall", Vec3::new(0.0, 0.0, -1.0), Vec3::new(0.4, 2.5, 8.0)),
    ] {
        let e = world.spawn((
            SceneId::new(),
            Name::new(name),
            Transform::from_translation(at),
            Brush::new(BrushShape::Box, size),
        ));
        world.set_parent(e, Some(group)).unwrap();
    }
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let paths = Assets::new(dir.clone()).paths().clone();
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("editor navigation test"),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..10 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.navigation_edges().is_empty(), "off at first");
    assert!(game.run_command(player.world_mut(), "view.navigation"));
    // Frames until the bake answers (it waits 0.3 s after the brushes last changed).
    let baked =
        |player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay| {
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_secs(20) {
                editor_frame(player, game, overlay);
                let edges = game.navigation_edges();
                if !edges.is_empty() {
                    return edges;
                }
            }
            panic!("no navigation baked");
        };
    let edges = baked(&mut player, &mut game, &mut overlay);
    println!("{} navigation edges", edges.len());
    // On the floor's top (y = 0, within a voxel or two), and round the wall: edges on both
    // its sides, beside it within the agent's radius and a cell.
    assert!(edges.iter().all(|e| e.0.y.abs() < 0.3 && e.1.y.abs() < 0.3));
    // A boundary along the wall: both ends beside it, running most of its 8 m.
    let along = |edges: &[(Vec3, Vec3, bool)], side: f32| {
        edges.iter().any(|e| {
            e.2 && (e.0.x - side).abs() < 0.3
                && (e.1.x - side).abs() < 0.3
                && (e.0.z - e.1.z).abs() > 6.0
        })
    };
    assert!(
        along(&edges, -0.7) && along(&edges, 0.7),
        "boundaries on both sides of the wall"
    );
    for _ in 0..3 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    if let Ok(path) = std::env::var("ENGINE_EDITOR_NAVIGATION_PNG") {
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
    // The wall moved 3 m along x: baked again, the boundaries follow it.
    let wall = engine_scene::find_named(player.world_mut(), "Wall").unwrap();
    player
        .world_mut()
        .get_mut::<Transform>(wall)
        .unwrap()
        .translation
        .x = 3.0;
    let before = edges;
    let start = std::time::Instant::now();
    let edges = loop {
        editor_frame(&mut player, &mut game, &mut overlay);
        let edges = game.navigation_edges();
        if edges != before {
            break edges;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "baked again"
        );
    };
    assert!(
        along(&edges, 2.3) && along(&edges, 3.7),
        "the boundaries moved with it"
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tab over the viewport edits the selected brush's faces (ADR 0110 E2b-5): the face under
/// the pointer lights (`ENGINE_EDITOR_FACES_PNG=<file>`), a drag moves it, and the player
/// meshes the brush's planes.
#[test]
fn tab_edits_a_brushs_faces_and_the_player_meshes_them() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-faces-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.05, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    let group = world.spawn((
        SceneId::new(),
        Name::new("Blockout"),
        Transform::IDENTITY,
        BrushGroup::default(),
    ));
    let crate_ = world.spawn((
        SceneId::new(),
        Name::new("Crate"),
        Transform::IDENTITY,
        Brush::new(BrushShape::Box, Vec3::new(2.0, 1.0, 2.0)),
    ));
    world.set_parent(crate_, Some(group)).unwrap();
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let paths = Assets::new(dir.clone()).paths().clone();
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("editor faces test"),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..20 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let crate_ = engine_scene::find_named(player.world_mut(), "Crate").unwrap();
    game.select(Some(crate_));
    let point = |game: &mut EditorGame<NoGame>, p: Vec3| {
        let [x, y] = game.on_screen(p).unwrap();
        send(
            game,
            WindowEventKind::CursorMoved {
                x: x.round() as i32,
                y: y.round() as i32,
            },
        );
    };
    let on_top = Vec3::new(0.4, 1.0, 0.3);
    point(&mut game, on_top);
    for _ in 0..3 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    for pressed in [true, false] {
        send(
            &mut game,
            WindowEventKind::Key {
                virtual_key: 0x09,
                scancode: 0x0F,
                extended: false,
                pressed,
                repeat: false,
            },
        );
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    for _ in 0..2 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    if let Ok(path) = std::env::var("ENGINE_EDITOR_FACES_PNG") {
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
    // The top dragged up a metre.
    let button = |pressed| WindowEventKind::MouseButton {
        button: MouseButton::Left,
        pressed,
    };
    send(&mut game, button(true));
    editor_frame(&mut player, &mut game, &mut overlay);
    point(&mut game, on_top + Vec3::Y);
    editor_frame(&mut player, &mut game, &mut overlay);
    send(&mut game, button(false));
    for _ in 0..3 {
        editor_frame(&mut player, &mut game, &mut overlay);
    }
    let world = player.world_mut();
    let b = world.get::<Brush>(crate_).unwrap();
    assert_eq!(b.shape, BrushShape::Planes);
    let top = b
        .planes
        .iter()
        .find(|q| q.normal == [0, 1, 0])
        .map(|q| q.offset)
        .unwrap();
    println!("the top at {top} mm");
    assert!((top - 2000).abs() <= 30, "{top}");
    assert_eq!(player.blockout().meshes(), 1, "meshed from its planes");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

fn editor_frame(player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay) {
    player.frame_overlaid(game, Some(overlay)).unwrap();
    player.set_view_rect(game.view_rect());
    game.sync_assets(player);
}

/// A window event, as the player hands it to the editor.
fn send(game: &mut EditorGame<NoGame>, kind: WindowEventKind) {
    game.event(&WindowEvent {
        window: WindowId(0),
        time: std::time::Instant::now(),
        kind,
    });
}

/// A left click where the pointer is, then frames enough for its pick to answer (the frames
/// in flight and a few more).
fn left_click(player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay) {
    for pressed in [true, false] {
        send(
            game,
            WindowEventKind::MouseButton {
                button: MouseButton::Left,
                pressed,
            },
        );
        editor_frame(player, game, overlay);
    }
    for _ in 0..6 {
        editor_frame(player, game, overlay);
    }
}

/// A box dragged from `from` to `to` (the window's pixels), with Alt held when `alt`, then
/// frames enough for its last box to answer.
fn drag_box(
    player: &mut Player,
    game: &mut EditorGame<NoGame>,
    overlay: &mut EditorOverlay,
    (from, to): ((i32, i32), (i32, i32)),
    alt: bool,
) {
    let key = |game: &mut EditorGame<NoGame>, pressed| {
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
    if alt {
        key(game, true);
    }
    let button = |pressed| WindowEventKind::MouseButton {
        button: MouseButton::Left,
        pressed,
    };
    send(
        game,
        WindowEventKind::CursorMoved {
            x: from.0,
            y: from.1,
        },
    );
    editor_frame(player, game, overlay);
    send(game, button(true));
    editor_frame(player, game, overlay);
    // Across in a few steps, a frame each.
    for k in 1..=4 {
        let at = (
            from.0 + (to.0 - from.0) * k / 4,
            from.1 + (to.1 - from.1) * k / 4,
        );
        send(game, WindowEventKind::CursorMoved { x: at.0, y: at.1 });
        editor_frame(player, game, overlay);
    }
    // `ENGINE_EDITOR_BOX_PNG=<file>` writes the frame of the box being dragged.
    if !alt && let Ok(path) = std::env::var("ENGINE_EDITOR_BOX_PNG") {
        let rgba = player
            .capture_overlaid(game, Some(overlay))
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
    send(game, button(false));
    editor_frame(player, game, overlay);
    for _ in 0..6 {
        editor_frame(player, game, overlay);
    }
    if alt {
        key(game, false);
        editor_frame(player, game, overlay);
    }
}

/// A few frames for the edits to reach the screen, then the frame's pixels.
fn frames(
    player: &mut Player,
    game: &mut EditorGame<NoGame>,
    overlay: &mut EditorOverlay,
) -> Vec<u8> {
    for _ in 0..4 {
        player.frame_overlaid(game, Some(&mut *overlay)).unwrap();
    }
    player
        .capture_overlaid(game, Some(&mut *overlay))
        .unwrap()
        .unwrap()
}

/// The quick start's game, what `ofeliaengine new` starts a game from.
#[path = "../../engine_app/examples/quick_start.rs"]
#[allow(dead_code)]
mod quick_start;

/// What `ofeliaengine edit` runs for a game made by `ofeliaengine new`: the editor on the
/// game's own settings and `start` (its `main!` with the `editor` feature), offscreen.
/// The scene opens with its materials found.
#[test]
fn the_quick_start_opens_in_the_editor() {
    let project =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine_app/examples/quick_start");
    assert!(project.join("scenes/main.scene").is_file());
    let mut settings = quick_start::settings().unwrap();
    settings.project = Some(project);
    settings.window = Window::Offscreen {
        width: 1280,
        height: 720,
    };
    settings.process_setup = false;
    settings.pipeline_cache = Some(None);
    settings.warm_up = false;
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, quick_start::start).unwrap();
    for _ in 0..3 {
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
        player.set_view_rect(game.view_rect());
        game.sync_assets(&mut player);
    }
    for name in ["Floor", "Cube", "Sun", "Camera"] {
        assert!(
            engine_scene::find_named(player.world_mut(), name).is_some(),
            "no {name}"
        );
    }
    for name in ["Floor", "Cube"] {
        let entity = engine_scene::find_named(player.world_mut(), name).unwrap();
        let model = *player.world_mut().get::<Model>(entity).unwrap();
        assert_ne!(model.material, builtin::MATERIAL, "{name}'s material");
        assert_eq!(player.assets().problem(model.material), None, "{name}");
        // Drawn: its mesh and material were made.
        let world = player.world_mut();
        assert!(
            world.get::<engine_runtime::MeshRenderer>(entity).is_some(),
            "{name}"
        );
    }
    overlay.destroy(&mut player).unwrap();
}
