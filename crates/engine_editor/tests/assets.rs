//! The project's assets in the editor (ADR 0108, ADR 0107 E4-1): the asset database opened
//! with the editor, each foreign file given a `.meta`, the player told every file's path by
//! its id, and the folder followed as files come, move and go.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{BakedBrush, Game, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::Vec3;
use engine_ecs::{Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind, WindowId};
use engine_scene::{AssetId, AssetKind, AssetMeta, AssetRef, Name, SceneId};
use std::path::Path;
use std::time::{Duration, Instant};

struct NoGame;

impl Game for NoGame {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn frame(player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay) {
    player.frame_overlaid(game, Some(overlay)).unwrap();
    player.set_view_rect(game.view_rect());
    game.sync_assets(player);
}

/// Frames until `done`, at most five seconds of them (the folder's watcher and the quiet
/// time before a scan run in real time).
fn until(
    player: &mut Player,
    game: &mut EditorGame<NoGame>,
    overlay: &mut EditorOverlay,
    done: impl Fn(&EditorGame<NoGame>) -> bool,
) {
    let started = Instant::now();
    while !done(game) && started.elapsed() < Duration::from_secs(5) {
        frame(player, game, overlay);
    }
}

fn meta_id(file: &Path) -> AssetId {
    let text = std::fs::read_to_string(AssetMeta::path(file)).unwrap();
    AssetMeta::parse(&text).unwrap().1.id.unwrap()
}

#[test]
fn the_editor_knows_the_projects_assets_and_follows_the_folder() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-assets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("props")).unwrap();
    let scene = AssetId::new();
    std::fs::write(
        dir.join("main.scene"),
        format!("[scene version=3 id={scene}]\n"),
    )
    .unwrap();
    std::fs::write(dir.join("props/rock.png"), b"a rock").unwrap();
    let settings = PlayerSettings {
        title: String::from("editor assets test"),
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
    // Opened with the editor: the scene by its header, the rock given a `.meta`.
    assert_eq!(game.asset("main.scene").unwrap().id, scene);
    let rock = game.asset("props/rock.png").unwrap();
    assert_eq!(rock.kind, AssetKind::Texture);
    assert_eq!(meta_id(&dir.join("props/rock.png")), rock.id);
    assert_eq!(
        player.assets().paths().get(AssetRef::file(rock.id)),
        Some("props/rock.png")
    );
    assert!(
        dir.join(".ofelia/assets.index").is_file(),
        "the index written"
    );
    // The FileSystem dock lists them (`ENGINE_EDITOR_BROWSER_PNG`).
    if let Ok(path) = std::env::var("ENGINE_EDITOR_BROWSER_PNG") {
        for _ in 0..3 {
            frame(&mut player, &mut game, &mut overlay);
        }
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
    // A new file in the folder: found, given a `.meta`, its path told to the player.
    std::fs::write(dir.join("props/crate.glb"), b"a crate").unwrap();
    until(&mut player, &mut game, &mut overlay, |g| {
        g.asset("props/crate.glb").is_some()
    });
    let crate_ = game.asset("props/crate.glb").expect("the new file found");
    assert_eq!(meta_id(&dir.join("props/crate.glb")), crate_.id);
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(
        player.assets().paths().get(AssetRef::file(crate_.id)),
        Some("props/crate.glb")
    );
    // Moved outside the editor with its `.meta`: the same id at its new path.
    std::fs::rename(dir.join("props/crate.glb"), dir.join("crate.glb")).unwrap();
    std::fs::rename(dir.join("props/crate.glb.meta"), dir.join("crate.glb.meta")).unwrap();
    until(&mut player, &mut game, &mut overlay, |g| {
        g.asset("crate.glb").is_some()
    });
    assert_eq!(game.asset("crate.glb").unwrap().id, crate_.id);
    assert!(game.asset("props/crate.glb").is_none());
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(
        player.assets().paths().get(AssetRef::file(crate_.id)),
        Some("crate.glb")
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A box of one meter standing on its origin, in one tint: what a bake makes a model of.
fn box_brush() -> BakedBrush {
    let mut b = BakedBrush {
        tint: [0.8, 0.45, 0.2, 1.0],
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        indices: Vec::new(),
    };
    // Each face: its normal and two directions along it whose cross is the normal.
    let faces = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    for (n, u, v) in faces {
        let (n, u, v) = (Vec3::from(n), Vec3::from(u), Vec3::from(v));
        let center = Vec3::new(0.0, 0.5, 0.0) + n * 0.5;
        let base = b.positions.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            b.positions
                .push((center + u * (su * 0.5) + v * (sv * 0.5)).to_array());
            b.normals.push(n.to_array());
            b.uvs.push([su, sv]);
        }
        b.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    b
}

/// The pointer at `at`, the left button pressed or let go with it (window events).
fn pointer(game: &mut EditorGame<NoGame>, at: [f32; 2], pressed: Option<bool>) {
    let mut send = |kind| {
        game.event(&WindowEvent {
            window: WindowId(0),
            time: Instant::now(),
            kind,
        });
    };
    send(WindowEventKind::CursorMoved {
        x: at[0].round() as i32,
        y: at[1].round() as i32,
    });
    if let Some(pressed) = pressed {
        send(WindowEventKind::MouseButton {
            button: MouseButton::Left,
            pressed,
        });
    }
}

#[test]
fn a_model_dragged_from_the_browser_rests_on_the_floor_and_draws() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-drop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("props")).unwrap();
    // A floor whose top is at y = -0.5, and a crate model beside the scene.
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.55, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    let options = engine_scene::SaveOptions {
        id: Some(AssetId::new()),
        assets: None,
        prefabs: None,
    };
    let mut text = String::new();
    engine_scene::save_with(
        &mut world,
        &engine_scene::SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let baked = engine_app::bake::glb("crate", &[box_brush()], &engine_app::grid_png());
    let (mesh, material) = baked.parts[0].clone();
    std::fs::write(dir.join("props/crate.glb"), baked.bytes).unwrap();
    // An image beside it, red growing to the right and green downwards: its tile's
    // thumbnail shows it upright.
    let rgba: Vec<u8> = (0..96 * 64)
        .flat_map(|i| {
            [
                ((i % 96) * 255 / 95) as u8,
                ((i / 96) * 255 / 63) as u8,
                40,
                255,
            ]
        })
        .collect();
    let png =
        engine_render::png::encode_png(96, 64, engine_render::png::PngFormat::Rgba, &rgba).unwrap();
    std::fs::write(dir.join("props/paint.png"), png).unwrap();
    let red = engine_app::MaterialFile {
        base_color: [0.85, 0.12, 0.1, 1.0],
        roughness: 0.35,
        ..engine_app::MaterialFile::default()
    };
    std::fs::write(dir.join("props/red.material"), red.to_text(AssetId::new())).unwrap();
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
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    // The props folder opened in the browser.
    let folder = game.text_at("props").expect("the props folder listed");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, folder, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    frame(&mut player, &mut game, &mut overlay);
    // The crate's file pressed, dragged a little, then over the viewport's middle.
    let file = game.text_at("crate.glb").expect("the crate's file listed");
    pointer(&mut game, file, None);
    frame(&mut player, &mut game, &mut overlay);
    pointer(&mut game, file, Some(true));
    frame(&mut player, &mut game, &mut overlay);
    pointer(&mut game, [file[0] + 12.0, file[1]], None);
    frame(&mut player, &mut game, &mut overlay);
    let [x, y, w, h] = game.view_rect().unwrap();
    let middle = [(x + w / 2) as f32, (y + h / 2) as f32];
    pointer(&mut game, middle, None);
    // Held there: it rests on the floor once the surface is found, and stays there (the
    // pick under the pointer leaves the crate itself out).
    let mut heights = Vec::new();
    for _ in 0..30 {
        frame(&mut player, &mut game, &mut overlay);
        let world = player.world_mut();
        if let Some(e) = engine_scene::find_named(world, "crate") {
            heights.push(world.get::<Transform>(e).unwrap().translation.y);
        }
    }
    let last = *heights.last().expect("placed while held over the view");
    assert!((last + 0.5).abs() < 1e-3, "on the floor's top: {heights:?}");
    assert!(
        heights
            .iter()
            .rev()
            .take(10)
            .all(|y| (y + 0.5).abs() < 1e-3),
        "it stays: {heights:?}"
    );
    // Let go: kept, drawn from the file's mesh and material.
    pointer(&mut game, middle, Some(false));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    let crate_ = game.asset("props/crate.glb").unwrap();
    let world = player.world_mut();
    let e = engine_scene::find_named(world, "crate").expect("kept");
    let model = *world.get::<Model>(e).unwrap();
    assert_eq!(model.mesh, AssetRef::part(crate_.id, &mesh));
    assert_eq!(model.material, AssetRef::part(crate_.id, &material));
    assert!(
        world.get::<engine_runtime::MeshRenderer>(e).is_some(),
        "the player draws it"
    );
    assert_eq!(player.assets().problem(model.mesh), None);
    assert_eq!(player.assets().problem(model.material), None);
    assert_eq!(game.selection(), [e]);
    // The tiles' thumbnails: the image as itself, the model and the material drawn by the
    // player (ADR 0112).
    let tiles = ["props/crate.glb", "props/paint.png", "props/red.material"];
    let started = Instant::now();
    while !tiles.iter().all(|t| game.thumbnail_ready(t)) {
        let ready: Vec<bool> = tiles.iter().map(|t| game.thumbnail_ready(t)).collect();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "thumbnails made: {ready:?}"
        );
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(
        dir.join(".ofelia/thumbnails").read_dir().unwrap().count() >= 6,
        "each cached with its key"
    );
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    if let Ok(path) = std::env::var("ENGINE_EDITOR_DROP_PNG") {
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
    // The file written anew outside the editor (a re-export): seen by the folder's
    // watcher, made again, drawn as it was until the new one is whole.
    let before = *player
        .world_mut()
        .get::<engine_runtime::MeshRenderer>(e)
        .unwrap();
    let mut taller = box_brush();
    for p in &mut taller.positions {
        p[1] *= 2.0;
    }
    let again = engine_app::bake::glb("crate", &[taller], &engine_app::grid_png());
    std::fs::write(dir.join("props/crate.glb"), again.bytes).unwrap();
    let started = Instant::now();
    loop {
        frame(&mut player, &mut game, &mut overlay);
        let r = player
            .world_mut()
            .get::<engine_runtime::MeshRenderer>(e)
            .copied()
            .expect("drawn while it is made again");
        if r.mesh != before.mesh && player.assets().loading() == 0 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "made again from the new file"
        );
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
