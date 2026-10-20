//! The editor around a game, offscreen (ADR 0107): a scene file opened, the shell painted
//! over the frame, the scene drawn into the viewport panel from the editor's camera; then
//! edited on the player (duplicate, move, undo) and saved back. `ENGINE_EDITOR_PNG=<file>`
//! writes the first frame, `ENGINE_EDITOR_PNG2` the edited one.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Assets, Game, MaterialFile, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_runtime::Sun;
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
        game.sync_assets(&player);
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
