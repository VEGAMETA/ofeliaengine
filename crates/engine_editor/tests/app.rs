//! The editor around a game, offscreen (ADR 0107): a scene file opened, the shell painted
//! over the frame, the scene drawn into the viewport panel from the editor's camera.
//! `ENGINE_EDITOR_PNG=<file>` writes the frame.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Assets, Game, MaterialFile, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Transform, World};
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
    let pixel = |px: u32, py: u32| {
        let i = ((py * width + px) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    // The red cube is in the viewport, drawn by the editor's camera.
    let mut red = 0;
    for py in (y..y + h).step_by(4) {
        for px in (x..x + w).step_by(4) {
            let [r, g, b] = pixel(px, py);
            if r > 1.6 * g && r > 1.6 * b && r > 40.0 {
                red += 1;
            }
        }
    }
    assert!(red > 50, "red pixels in the viewport: {red}");
    // Outside the viewport the shell's panels: none of the scene's red.
    let mut red_outside = 0;
    for py in (0..height).step_by(8) {
        for px in (0..width).step_by(8) {
            let inside = px >= x && px < x + w && py >= y && py < y + h;
            let [r, g, b] = pixel(px, py);
            if !inside && r > 1.6 * g && r > 1.6 * b && r > 40.0 {
                red_outside += 1;
            }
        }
    }
    assert!(
        red_outside < 10,
        "red pixels outside the viewport: {red_outside}"
    );

    // Selecting the cube: the inspector lists its components (`ENGINE_EDITOR_PNG2`).
    let world = player.world_mut();
    let mut names = engine_ecs::QueryState::<(engine_ecs::Entity, &Name)>::new(world);
    let cube = names
        .iter_readonly(world)
        .find(|(_, n)| n.as_str() == "Cube")
        .map(|(e, _)| e)
        .unwrap();
    game.select(Some(cube));
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    let rgba = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
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
    assert_eq!(game.selected(), Some(cube));
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
