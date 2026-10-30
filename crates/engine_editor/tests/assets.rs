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

/// A key pressed and let go (window events).
fn tap(game: &mut EditorGame<NoGame>, virtual_key: u16, scancode: u16) {
    for pressed in [true, false] {
        game.event(&WindowEvent {
            window: WindowId(0),
            time: Instant::now(),
            kind: WindowEventKind::Key {
                virtual_key,
                scancode,
                extended: false,
                pressed,
                repeat: false,
            },
        });
    }
}

/// A mono 16-bit WAV file of `frames` frames of silence at 48 kHz (heard as nothing, so
/// a test that plays it is quiet on a machine people work at).
fn silent_wav(frames: u32) -> Vec<u8> {
    wav(frames, |_| 0.0)
}

/// A mono 16-bit WAV file of `frames` frames at 48 kHz, frame `i` at `sample(i)`.
fn wav(frames: u32, sample: impl Fn(u32) -> f32) -> Vec<u8> {
    let data = frames * 2;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&96_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for i in 0..frames {
        let v = (sample(i).clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[test]
fn a_sound_picked_in_the_dock_plays_on_the_editors_own_bus() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("main.scene"),
        format!("[scene version=3 id={}]\n", AssetId::new()),
    )
    .unwrap();
    std::fs::write(dir.join("hush.wav"), silent_wav(48_000 * 3)).unwrap();
    // A tone at half scale after a quarter second, picked but never played.
    let tone = |i: u32| {
        if i >= 12_000 {
            0.5 * (i as f32 * 0.03).sin()
        } else {
            0.0
        }
    };
    std::fs::write(dir.join("tone.wav"), wav(96_000, tone)).unwrap();
    let settings = PlayerSettings {
        title: String::from("editor preview test"),
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
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    // Its tile picked, then Space: played as it is, while the world stands still.
    let tile = game.text_at("hush.wav").expect("the sound listed");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, tile, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    tap(&mut game, 0x20, 0x39); // Space
    let started = Instant::now();
    while !player.sound_preview().is_some_and(|p| p.playing)
        && started.elapsed() < Duration::from_secs(5)
    {
        frame(&mut player, &mut game, &mut overlay);
    }
    let state = player.sound_preview().expect("a preview asked");
    assert!(state.playing, "{state:?}");
    assert!(state.failed.is_none(), "{state:?}");
    assert_eq!(state.file, dir.join("hush.wav"));
    assert_eq!(state.length, Some(3.0));
    // Escape stops it.
    tap(&mut game, 0x1B, 0x01);
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(player.sound_preview().is_none());
    // The tone picked (Auto Play is off: nothing plays): the inspector reads it on a thread
    // of its own and shows its facts and its waveform, the picture uploaded by the overlay
    // (`ENGINE_EDITOR_SOUND_PNG` keeps the frame).
    let tile = game.text_at("tone.wav").expect("the tone listed");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, tile, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    until(&mut player, &mut game, &mut overlay, |g| {
        g.text_at("0:02.00").is_some()
    });
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.text_at("\u{2212}6.0 dBFS").is_some(), "its peak");
    assert!(game.text_at("0.25 s first").is_some(), "its silence");
    assert!(
        game.text_at("Reading\u{2026}").is_none(),
        "its picture shown"
    );
    assert!(player.sound_preview().is_none(), "picked, not played");
    if let Ok(path) = std::env::var("ENGINE_EDITOR_SOUND_PNG") {
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
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bus_added_in_the_mixer_panel_is_the_players_at_once_and_its_file_once_saved() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-mixer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("main.scene"),
        format!("[scene version=3 id={}]\n", AssetId::new()),
    )
    .unwrap();
    let settings = PlayerSettings {
        title: String::from("editor mixer test"),
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
    frame(&mut player, &mut game, &mut overlay);
    let buses = |player: &mut Player| -> Vec<String> {
        let mixer = player.world_mut().resource::<engine_app::Mixer>().unwrap();
        mixer.buses().map(str::to_owned).collect()
    };
    assert_eq!(
        buses(&mut player),
        ["master", "effects", "music", "ui", "voice"]
    );
    // Alt+A's command, then Add Bus: the player's mixer has it by the next frame.
    assert!(game.run_command(player.world_mut(), "view.mixer"));
    assert!(game.run_command(player.world_mut(), "audio.add_bus"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(
        buses(&mut player).contains(&String::from("bus")),
        "{:?}",
        buses(&mut player)
    );
    assert!(!dir.join("main.mixer").exists(), "unsaved: no file yet");
    // Ctrl+S writes it with the scene; undone, the player's mixer loses the bus again.
    assert!(game.run_command(player.world_mut(), "file.save"));
    let written = engine_app::MixerFile::load(&dir.join("main.mixer")).unwrap();
    assert!(written.buses.contains_key("bus"));
    assert!(game.run_command(player.world_mut(), "edit.undo"));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(!buses(&mut player).contains(&String::from("bus")));
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
fn key(game: &mut EditorGame<NoGame>, vk: u16, scancode: u16, down: bool) {
    // The arrows are extended keys.
    let extended = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E);
    game.event(&WindowEvent {
        window: WindowId(0),
        time: Instant::now(),
        kind: WindowEventKind::Key {
            virtual_key: vk,
            scancode,
            extended,
            pressed: down,
            repeat: false,
        },
    });
}

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

/// E10-4d: the selected object's material shows its preview at the top of its inspector
/// section: the material drawn on the object's mesh by the engine, red in the inspector's
/// column while the object itself is far out of view.
#[test]
fn a_materials_preview_draws_in_its_inspector_section() {
    let dir = std::env::temp_dir().join(format!(
        "ofelia-editor-material-preview-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("props")).unwrap();
    let red_id = AssetId::new();
    let red = engine_app::MaterialFile {
        base_color: [0.85, 0.08, 0.06, 1.0],
        roughness: 0.4,
        ..engine_app::MaterialFile::default()
    };
    std::fs::write(dir.join("props/red.material"), red.to_text(red_id)).unwrap();
    AssetMeta::default()
        .write(&dir.join("props/red.material"), red_id)
        .unwrap();
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Far box"),
        Transform::from_xyz(0.0, 0.0, -5000.0),
        Model::new(builtin::CUBE, AssetRef::file(red_id)),
    ));
    let mut paths = engine_scene::AssetPaths::new();
    paths.insert(AssetRef::file(red_id), "props/red.material");
    let options = engine_scene::SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
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
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor material preview test"),
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
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let far = engine_scene::find_named(player.world_mut(), "Far box").expect("the box");
    game.select(Some(far));
    // Red in the inspector's column: the preview, the box being far out of view.
    let red_in_inspector = |rgba: &[u8]| {
        rgba.chunks_exact(4)
            .enumerate()
            .filter(|(i, p)| {
                (*i as u32 % width) >= width - 360 && p[0] > 120 && p[1] < 60 && p[2] < 60
            })
            .count()
    };
    let started = Instant::now();
    let mut count = 0;
    while started.elapsed() < Duration::from_secs(30) {
        frame(&mut player, &mut game, &mut overlay);
        let shot = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        count = red_in_inspector(&shot);
        if count > 2_000 {
            break;
        }
    }
    println!(
        "the material's preview drawn in the inspector after {:.2} s: {count} red pixels",
        started.elapsed().as_secs_f64()
    );
    assert!(
        count > 2_000,
        "no preview in the inspector: {count} red pixels"
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// File `file` of the listing pressed and dragged a little, then the pointer over `to`, a
/// frame each.
fn drag_over(
    player: &mut Player,
    game: &mut EditorGame<NoGame>,
    overlay: &mut EditorOverlay,
    file: [f32; 2],
    to: [f32; 2],
) {
    pointer(game, file, None);
    frame(player, game, overlay);
    pointer(game, file, Some(true));
    frame(player, game, overlay);
    pointer(game, [file[0] + 12.0, file[1]], None);
    frame(player, game, overlay);
    pointer(game, to, None);
}

/// E10-4e: a material dragged over an object shows on it in the view before the drop, and
/// the object has its own back once the pointer leaves it. One that cuts the object away
/// entirely (masked, its alpha zero) stays on it while the pointer moves over it: the
/// object is picked as it was, not through the material (otherwise each pick would see
/// through it, and the material would go off and on as the pointer moves).
#[test]
fn a_material_dragged_over_an_object_shows_on_it_before_the_drop() {
    let dir =
        std::env::temp_dir().join(format!("ofelia-editor-try-material-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("props")).unwrap();
    let red = engine_app::MaterialFile {
        base_color: [0.85, 0.08, 0.06, 1.0],
        roughness: 0.4,
        ..engine_app::MaterialFile::default()
    };
    let hole = engine_app::MaterialFile {
        base_color: [1.0, 1.0, 1.0, 0.0],
        alpha: engine_app::Alpha::Mask,
        alpha_cutoff: 0.5,
        ..engine_app::MaterialFile::default()
    };
    let (red_id, hole_id) = (AssetId::new(), AssetId::new());
    for (name, file, id) in [("red", &red, red_id), ("hole", &hole, hole_id)] {
        let path = dir.join(format!("props/{name}.material"));
        std::fs::write(&path, file.to_text(id)).unwrap();
        AssetMeta::default().write(&path, id).unwrap();
    }
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Box"),
        Transform::default(),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        // Its icon away from the box.
        Transform::from_xyz(4.0, 6.0, 4.0),
        engine_runtime::Sun {
            color: Vec3::splat(2.0),
            ambient: Vec3::splat(0.4),
        },
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
    let settings = PlayerSettings {
        title: String::from("editor material tried test"),
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
    let folder = game.text_at("props").expect("the props folder listed");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, folder, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    frame(&mut player, &mut game, &mut overlay);
    let boxed = engine_scene::find_named(player.world_mut(), "Box").expect("the box");
    let material = |player: &mut Player| player.world_mut().get::<Model>(boxed).unwrap().material;
    let [x, y, w, h] = game.view_rect().unwrap();
    let middle = [(x + w / 2) as f32, (y + h / 2) as f32];
    let corner = [(x + 24) as f32, (y + 24) as f32];
    let width = player.extent().width as usize;
    let red_at = |rgba: &[u8], [px, py]: [f32; 2]| {
        let i = (py as usize * width + px as usize) * 4;
        let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]].map(u32::from);
        r > 80 && r > 2 * g.max(b)
    };
    // Red over the box: it shows red in the view before the drop.
    let file = game
        .text_at("red.material")
        .expect("the red material listed");
    drag_over(&mut player, &mut game, &mut overlay, file, middle);
    let started = Instant::now();
    let mut shown = false;
    let mut frames = 0;
    while started.elapsed() < Duration::from_secs(10) {
        frame(&mut player, &mut game, &mut overlay);
        frames += 1;
        if material(&mut player) == AssetRef::file(red_id) {
            let shot = player
                .capture_overlaid(&mut game, Some(&mut overlay))
                .unwrap()
                .unwrap();
            if red_at(&shot, middle) {
                shown = true;
                break;
            }
        }
    }
    println!("red shown on the box {frames} frames after the pointer reached it");
    assert!(shown, "red on the box in the view while dragged over it");
    // Off the box (nothing there): its own back, not red.
    pointer(&mut game, corner, None);
    let started = Instant::now();
    while material(&mut player) != builtin::MATERIAL {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "its own back off it"
        );
        frame(&mut player, &mut game, &mut overlay);
    }
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let shot = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    assert!(!red_at(&shot, middle), "its own drawn again");
    // Escape: the drag over, nothing given.
    tap(&mut game, 0x1B, 0x01);
    frame(&mut player, &mut game, &mut overlay);
    pointer(&mut game, corner, Some(false));
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(material(&mut player), builtin::MATERIAL);
    // The material that cuts the box away: kept on it while the pointer moves over it,
    // a pick under the moving pointer every frame.
    let file = game
        .text_at("hole.material")
        .expect("the hole material listed");
    drag_over(&mut player, &mut game, &mut overlay, file, middle);
    let started = Instant::now();
    while material(&mut player) != AssetRef::file(hole_id) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the hole material tried on the box"
        );
        frame(&mut player, &mut game, &mut overlay);
    }
    let mut kept = Vec::new();
    for k in 0..60 {
        let dx = [-2.0, 0.0, 2.0][k % 3];
        pointer(&mut game, [middle[0] + dx, middle[1]], None);
        frame(&mut player, &mut game, &mut overlay);
        kept.push(material(&mut player) == AssetRef::file(hole_id));
    }
    assert!(
        kept.iter().all(|&k| k),
        "kept on the box as the pointer moves: {kept:?}"
    );
    // Let go over it: given.
    pointer(&mut game, middle, Some(false));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(material(&mut player), AssetRef::file(hole_id));
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-1b: a skinned model selected shows its clips in the inspector with its
/// file's numbers; P plays it in place (the view changes from moment to moment, the scene
/// file untouched) and P again puts it back in its rest pose. E12-1c: its skeleton's sticks
/// follow it; Tab over the view picks its joints, as the inspector's Skeleton does.
/// `ENGINE_EDITOR_JOINTS_PNG=<file>` writes the window while a joint is picked.
#[test]
fn a_selected_characters_clips_show_and_p_plays_it_in_place() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "ofelia-editor-clips-in-place-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join("models").join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("models/CesiumMan.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "Cesium_Man"),
        AssetRef::part(id, "Cesium_Man-effect"),
    );
    let mut paths = engine_scene::AssetPaths::new();
    paths.insert(mesh, "models/CesiumMan.gltf#Cesium_Man");
    paths.insert(material, "models/CesiumMan.gltf#Cesium_Man-effect");
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Man"),
        Transform::from_rotation(engine_core::math::Quat::from_rotation_x(
            -core::f32::consts::FRAC_PI_2,
        )),
        Model::new(mesh, material),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(engine_core::math::Quat::from_rotation_x(-1.0)),
        engine_app::prelude::Sun {
            color: Vec3::splat(1.5),
            ambient: Vec3::splat(0.25),
        },
    ));
    let options = engine_scene::SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
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
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor clips in place test"),
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
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let man = engine_scene::find_named(player.world_mut(), "Man").expect("the man");
    game.select(Some(man));
    // F frames him; his skin is made in the background, then his Clips show.
    tap(&mut game, 0x46, 0x21);
    let started = Instant::now();
    while game.clips().0.is_empty() && started.elapsed() < Duration::from_secs(20) {
        frame(&mut player, &mut game, &mut overlay);
    }
    let (rows, _) = game.clips();
    println!(
        "clips after {:.2} s: {rows:?}",
        started.elapsed().as_secs_f64()
    );
    assert_eq!(rows.len(), 1, "CesiumMan has one clip: {rows:?}");
    assert!(
        rows[0].contains("2.00 s") && rows[0].contains("in place"),
        "{rows:?}"
    );
    assert!(
        rows[0].contains("jumps 3.1 cm at leg_joint_R_1"),
        "{rows:?}"
    );
    // The view alone (the inspector's time changes as he plays).
    let [vx, vy, vw, vh] = game.view_rect().expect("the view's rectangle");
    let shot = |player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay| {
        for _ in 0..20 {
            frame(player, game, overlay);
        }
        let rgba = player
            .capture_overlaid(game, Some(overlay))
            .unwrap()
            .unwrap();
        let mut view = Vec::with_capacity((vw * vh * 4) as usize);
        for y in vy..vy + vh {
            let at = ((y * width + vx) * 4) as usize;
            view.extend_from_slice(&rgba[at..at + (vw * 4) as usize]);
        }
        view
    };
    let differ = |a: &[u8], b: &[u8]| {
        a.chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 24))
            .count()
    };
    // The camera's flight to him over first.
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(1) {
        frame(&mut player, &mut game, &mut overlay);
    }
    let rest = shot(&mut player, &mut game, &mut overlay);
    // His skeleton's sticks while he is selected (ADR 0124's E12-1c): a stick a joint
    // below his root, within his height.
    let [bones, others] = game.skeleton_lines();
    let joints = player.model_rig(man).unwrap().skeleton().joint_count();
    println!("{} sticks of {joints} joints", bones.len());
    assert!(
        bones.len() + 1 >= joints && bones.len() < joints,
        "{}",
        bones.len()
    );
    assert!(others.is_empty(), "no other character's");
    let top = bones.iter().flatten().map(|p| p.y).fold(f32::MIN, f32::max);
    assert!((1.0..1.6).contains(&top), "his head's joint at {top} m");
    // P: he walks in place.
    tap(&mut game, 0x50, 0x19);
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(game.playing_in_place(), 1);
    let first = shot(&mut player, &mut game, &mut overlay);
    let walking = game.skeleton_lines()[0].clone();
    let later = Instant::now();
    while later.elapsed() < Duration::from_millis(300) {
        frame(&mut player, &mut game, &mut overlay);
    }
    let second = shot(&mut player, &mut game, &mut overlay);
    let strode = walking
        .iter()
        .zip(&game.skeleton_lines()[0])
        .map(|(a, b)| (a[1] - b[1]).length())
        .fold(0.0f32, f32::max);
    println!("his joints moved up to {strode} m between the two moments");
    assert!(strode > 0.02, "the sticks follow his walk: {strode} m");
    let (moved, walked) = (differ(&rest, &first), differ(&first, &second));
    println!("in place: {moved} pixels off his rest pose, {walked} between two moments");
    assert!(moved > 500 && walked > 500, "{moved} / {walked}");
    // P again: his rest pose, as before.
    tap(&mut game, 0x50, 0x19);
    frame(&mut player, &mut game, &mut overlay);
    assert_eq!(game.playing_in_place(), 0);
    let back = shot(&mut player, &mut game, &mut overlay);
    let off = differ(&rest, &back);
    println!("stopped: {off} pixels off his rest pose");
    assert!(off < 200, "{off} pixels off his rest pose after P");
    // Tab over the view: his joints take the pointer (E12-1c); a click on one picks it,
    // the selection staying his.
    let [vx, vy, vw, vh] = [vx as f32, vy as f32, vw as f32, vh as f32];
    pointer(&mut game, [vx + vw * 0.5, vy + vh * 0.5], None);
    tap(&mut game, 0x09, 0x0F);
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.picked_joint().1, "Tab enters his joints");
    // A joint in the view with no other within 12 pixels of it (F framed his origin, so
    // his upper joints may be above the view).
    let names = player.model_rig(man).unwrap().skeleton().names().to_vec();
    let points: Vec<(String, [f32; 2])> = names
        .iter()
        .filter_map(|n| Some((n.clone(), game.screen_point(game.joint_point(n)?)?)))
        .collect();
    let apart = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]) >= 12.0;
    let (joint, at) = points
        .iter()
        .find(|(n, p)| {
            p[0] > vx + 8.0
                && p[0] < vx + vw - 8.0
                && p[1] > vy + 8.0
                && p[1] < vy + vh - 8.0
                && points.iter().all(|(m, q)| m == n || apart(*p, *q))
        })
        .cloned()
        .expect("a joint alone in the view");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, at, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(
        game.picked_joint().0.as_deref(),
        Some(joint.as_str()),
        "the click picks the joint under it"
    );
    assert_eq!(game.selection(), [man], "and keeps him selected");
    // E12-3c: with the Animation tab open, the picked joint leaves a trail through his clip
    // in the view, a tick a frame, and its speed draws as a bar a frame under the ruler; a
    // press on the curve's middle puts the playhead at the clip's middle.
    key(&mut game, 0x12, 0x38, true);
    tap(&mut game, 0x4E, 0x31); // Alt+N
    key(&mut game, 0x12, 0x38, false);
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let samples = player.model_rig(man).unwrap().clips()[0].sample_count() as usize;
    let length = player.model_rig(man).unwrap().clips()[0].duration();
    let (lines, bars, label) = game.joint_trail();
    println!("trail: {lines} lines, {bars} bars; {label}");
    assert_eq!(bars, samples - 1, "a bar a step");
    assert!(
        !label.contains("stands still"),
        "a joint that moves: {label}"
    );
    assert_eq!(
        lines,
        2 * samples - 1,
        "the path's segments and a tick a sample"
    );
    assert!(
        label.starts_with(&format!("{joint}: the longest step")),
        "{label}"
    );
    let [x, y, w, h] = game.animation_speed_curve().expect("the speed curve");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, [x + w * 0.5, y + h * 0.5], pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    let played = game.clip_time().unwrap();
    println!("the curve's middle: {played:.3} s of {length:.3} s");
    assert!(
        (played - length * 0.5).abs() < 0.05,
        "the clip's middle: {played}"
    );
    if let Ok(out) = std::env::var("ENGINE_EDITOR_TRAIL_PNG") {
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(out, png).unwrap();
    }
    key(&mut game, 0x12, 0x38, true);
    tap(&mut game, 0x4E, 0x31); // Alt+N: the tab shut again
    key(&mut game, 0x12, 0x38, false);
    // The docks take the tab's room back (the inspector laid out again).
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(game.joint_trail().0, 0, "no trail with the tab shut");
    // The inspector's Skeleton lists his joints: a click on a row picks that one.
    let (name, row) = names
        .iter()
        .filter(|n| **n != joint)
        .find_map(|n| {
            let at = game
                .texts_at(n)
                .into_iter()
                .find(|p| p[0] >= 1280.0 - 360.0 && p[1] < 700.0)?;
            Some((n.clone(), at))
        })
        .expect("a joint's row in the Skeleton");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, row, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(
        game.picked_joint(),
        (Some(name), true),
        "the row's joint picked, the joints still taking the pointer"
    );
    if let Ok(out) = std::env::var("ENGINE_EDITOR_JOINTS_PNG") {
        pointer(&mut game, at, None);
        frame(&mut player, &mut game, &mut overlay);
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(out, png).unwrap();
    }
    // Escape leaves the joints; nothing selected, no sticks.
    tap(&mut game, 0x1B, 0x01);
    frame(&mut player, &mut game, &mut overlay);
    assert!(!game.picked_joint().1, "Escape leaves them");
    game.select(None);
    frame(&mut player, &mut game, &mut overlay);
    assert!(game.skeleton_lines()[0].is_empty());
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-1b: a skinned model file picked in the dock shows its clips in the
/// inspector under the engine's preview of it; ▶ plays the chosen clip there (the picture
/// changes from moment to moment) and ■ stops it where it is (the picture stays).
/// `ENGINE_EDITOR_CLIPS_PNG=<file>` writes the window while it plays.
#[test]
fn a_skinned_model_picked_in_the_dock_shows_its_clips_under_its_preview() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/Fox");
    if !source.join("Fox.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "ofelia-editor-model-preview-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(&source).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            std::fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
        }
    }
    AssetMeta::ensure(&dir.join("Fox.gltf")).unwrap();
    let mut world = World::new();
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
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor model preview test"),
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
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    // Its tile picked in the dock: its skin made, its clips under its preview.
    let tile = game.text_at("Fox.gltf").expect("the model listed");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, tile, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    let started = Instant::now();
    while !(game.clips().0.len() == 3 && game.model_preview().0)
        && started.elapsed() < Duration::from_secs(30)
    {
        frame(&mut player, &mut game, &mut overlay);
    }
    let (rows, _) = game.clips();
    println!(
        "clips and preview after {:.2} s: {rows:?}",
        started.elapsed().as_secs_f64()
    );
    assert_eq!(rows.len(), 3, "the Fox's three clips: {rows:?}");
    for name in ["Survey", "Walk", "Run"] {
        assert!(rows.iter().any(|r| r.starts_with(name)), "{name}: {rows:?}");
    }
    assert!(game.model_preview().0, "its preview drawn");
    // The inspector's column, where the preview is.
    let inspector =
        |player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay| {
            frame(player, game, overlay);
            let rgba = player
                .capture_overlaid(game, Some(overlay))
                .unwrap()
                .unwrap();
            rgba.chunks_exact(4)
                .enumerate()
                .filter(|(i, _)| (*i as u32 % width) >= width - 360)
                .flat_map(|(_, p)| p.iter().copied())
                .collect::<Vec<u8>>()
        };
    let differ = |a: &[u8], b: &[u8]| {
        a.chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 24))
            .count()
    };
    // ▶: the clip plays in the preview.
    let in_inspector = |at: &[[f32; 2]]| at.iter().copied().find(|p| p[0] >= (width - 360) as f32);
    let play =
        in_inspector(&game.texts_at(engine_editor::icons::PLAY)).expect("the Clips' play button");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, play, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.model_preview().1, "playing");
    let first = inspector(&mut player, &mut game, &mut overlay);
    let later = Instant::now();
    while later.elapsed() < Duration::from_millis(400) {
        frame(&mut player, &mut game, &mut overlay);
    }
    let second = inspector(&mut player, &mut game, &mut overlay);
    let moving = differ(&first, &second);
    if let Ok(out) = std::env::var("ENGINE_EDITOR_CLIPS_PNG") {
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(out, png).unwrap();
    }
    // ■: it stops where it is.
    let stop =
        in_inspector(&game.texts_at(engine_editor::icons::STOP)).expect("the Clips' stop button");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, stop, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(!game.model_preview().1, "stopped");
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let held = inspector(&mut player, &mut game, &mut overlay);
    for _ in 0..20 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let still = differ(&held, &inspector(&mut player, &mut game, &mut overlay));
    println!("playing: {moving} pixels changed in the inspector; stopped: {still}");
    assert!(moving > 300, "the preview plays: {moving}");
    assert!(still < 50, "the preview holds: {still}");
    // E12-1b-3: a click on a clip's row gives the Clips the arrows: ↓ the next clip, ↑ back.
    let walk = in_inspector(&game.texts_at("Walk")).expect("Walk's row");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, walk, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(game.chosen_clip(), Some(1), "Walk chosen by its row");
    for (vk, scancode, expected) in [(0x28, 0x50, 2), (0x26, 0x48, 1), (0x26, 0x48, 0)] {
        key(&mut game, vk, scancode, true);
        frame(&mut player, &mut game, &mut overlay);
        key(&mut game, vk, scancode, false);
        frame(&mut player, &mut game, &mut overlay);
        assert_eq!(game.chosen_clip(), Some(expected), "the arrow {vk:#x}");
    }
    // E12-1b-3b: Ctrl+click on Run: it plays beside Survey in the preview.
    let one = inspector(&mut player, &mut game, &mut overlay);
    let run = in_inspector(&game.texts_at("Run")).expect("Run's row");
    key(&mut game, 0x11, 0x1D, true);
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, run, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    key(&mut game, 0x11, 0x1D, false);
    assert_eq!(game.beside_clip(), Some(2), "Run beside");
    assert_eq!(game.chosen_clip(), Some(0), "Survey still the chosen one");
    let mut two = inspector(&mut player, &mut game, &mut overlay);
    let started = Instant::now();
    while differ(&one, &two) < 300 && started.elapsed() < Duration::from_secs(10) {
        two = inspector(&mut player, &mut game, &mut overlay);
    }
    println!("side by side: {} pixels changed", differ(&one, &two));
    assert!(differ(&one, &two) >= 300, "two in the preview");
    // E12-3a: Alt+N opens the Animation tab; a press on its ruler's middle puts the
    // playhead at the clip's middle; → after it steps a frame on.
    key(&mut game, 0x12, 0x38, true);
    key(&mut game, 0x4E, 0x31, true);
    frame(&mut player, &mut game, &mut overlay);
    key(&mut game, 0x4E, 0x31, false);
    key(&mut game, 0x12, 0x38, false);
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    let [x, y, w, h] = game.animation_ruler().expect("the Animation tab's ruler");
    let middle = [x + w * 0.5, y + h * 0.5];
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, middle, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    let length = 3.4167f32;
    let at = game.clip_time().unwrap();
    println!("the ruler's middle: {at:.3} s of Survey's {length} s");
    assert!((at - length * 0.5).abs() < 0.1, "the clip's middle: {at}");
    key(&mut game, 0x27, 0x4D, true);
    frame(&mut player, &mut game, &mut overlay);
    key(&mut game, 0x27, 0x4D, false);
    frame(&mut player, &mut game, &mut overlay);
    let stepped = game.clip_time().unwrap();
    println!("→: {stepped:.4} s");
    if let Ok(out) = std::env::var("ENGINE_EDITOR_ANIMATION_PNG") {
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(out, png).unwrap();
    }
    assert!(
        stepped > at && stepped - at < 0.1,
        "a frame on: {at} then {stepped}"
    );
    // E12-3b-2: E adds an event at the playhead's frame, written to Fox.gltf's `.meta` and
    // drawn on the strip, its name typed at once; Escape leaves the name, Delete removes the
    // event, Ctrl+Z brings it back.
    let meta = || std::fs::read_to_string(dir.join("Fox.gltf.meta")).unwrap();
    let tap = |game: &mut EditorGame<NoGame>,
               player: &mut Player,
               overlay: &mut EditorOverlay,
               vk: u16,
               sc: u16| {
        key(game, vk, sc, true);
        frame(player, game, overlay);
        key(game, vk, sc, false);
        frame(player, game, overlay);
    };
    tap(&mut game, &mut player, &mut overlay, 0x45, 0x12); // E
    frame(&mut player, &mut game, &mut overlay);
    println!("{}", meta());
    assert!(
        meta().contains("model.clips.0.name = \"Survey\"")
            && meta().contains("model.clips.0.events.0.name = \"event\""),
        "{}",
        meta()
    );
    let time: f32 = meta()
        .lines()
        .find_map(|l| l.strip_prefix("model.clips.0.events.0.time = "))
        .and_then(|t| t.trim().parse().ok())
        .unwrap();
    println!("an event at {time:.4} s");
    assert!(
        (time - stepped).abs() < 0.006,
        "at the playhead: {time} {stepped}"
    );
    assert!(game.text_at("event").is_some(), "its mark drawn");
    if let Ok(out) = std::env::var("ENGINE_EDITOR_EVENTS_PNG") {
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(out, png).unwrap();
    }
    tap(&mut game, &mut player, &mut overlay, 0x1B, 0x01); // Escape
    tap(&mut game, &mut player, &mut overlay, 0x2E, 0x53); // Delete
    assert!(!meta().contains("model.clips"), "{}", meta());
    key(&mut game, 0x11, 0x1D, true);
    tap(&mut game, &mut player, &mut overlay, 0x5A, 0x2C); // Ctrl+Z
    key(&mut game, 0x11, 0x1D, false);
    frame(&mut player, &mut game, &mut overlay);
    assert!(meta().contains("events.0.name = \"event\""), "{}", meta());
    // E12-3b-3: its sound typed into the row (after a click on its mark picks it); played
    // from a few frames before it, the preview passes it and plays its sound.
    let mut wav = Vec::new();
    let samples: Vec<u8> = (0..9600)
        .flat_map(|i: i32| (((i % 100) - 50) as i16 * 200).to_le_bytes())
        .collect();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    for v in [16u32, 1 | (1 << 16), 48_000, 96_000, 2 | (16 << 16)] {
        wav.extend_from_slice(&v.to_le_bytes());
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(&samples);
    std::fs::write(dir.join("step.wav"), wav).unwrap();
    let mark = game.text_at("event").expect("its mark");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, mark, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    let field = game
        .text_at("sound: a path in the project")
        .expect("the picked event's sound field");
    for pressed in [None, Some(true), Some(false)] {
        pointer(&mut game, field, pressed);
        frame(&mut player, &mut game, &mut overlay);
    }
    for c in "step.wav".chars() {
        game.event(&WindowEvent {
            window: WindowId(0),
            time: Instant::now(),
            kind: WindowEventKind::Char(c),
        });
    }
    frame(&mut player, &mut game, &mut overlay);
    tap(&mut game, &mut player, &mut overlay, 0x0D, 0x1C); // Enter
    assert!(
        meta().contains("events.0.sound = \"step.wav\""),
        "{}",
        meta()
    );
    for _ in 0..3 {
        tap(&mut game, &mut player, &mut overlay, 0x25, 0x4B); // ←
    }
    tap(&mut game, &mut player, &mut overlay, 0x20, 0x39); // Space: play
    let started = Instant::now();
    let heard = loop {
        frame(&mut player, &mut game, &mut overlay);
        let file = player
            .sound_preview()
            .map(|s| s.file.to_string_lossy().into_owned());
        if let Some(file) = file.filter(|f| f.ends_with("step.wav")) {
            break file;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the event's sound played"
        );
    };
    println!("the event's sound: {heard}");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-1c (question 6a): a character dragged from the dock into the view is one
/// entity, its skeleton's joints none: CesiumMan stands upright on the floor (his file's z
/// up turned to the scene's y, his feet on the floor's top), drawn skinned, his Clips shown
/// once he is selected. `ENGINE_EDITOR_CHARACTER_PNG=<file>` writes the window then.
#[test]
fn a_character_dragged_from_the_dock_is_one_entity_standing_on_the_floor() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "ofelia-editor-character-drop-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("CesiumMan.gltf")).unwrap();
    // A floor whose top is at y = -0.5, and the sun.
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
        Transform::from_rotation(engine_core::math::Quat::from_rotation_x(-1.0)),
        engine_app::prelude::Sun {
            color: Vec3::splat(1.5),
            ambient: Vec3::splat(0.25),
        },
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
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor character drop test"),
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
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let entities = player.world_mut().len();
    // His file pressed in the dock, dragged a little, then held over the view's middle
    // until he rests on the floor, and let go there.
    let file = game.text_at("CesiumMan.gltf").expect("his file listed");
    pointer(&mut game, file, None);
    frame(&mut player, &mut game, &mut overlay);
    pointer(&mut game, file, Some(true));
    frame(&mut player, &mut game, &mut overlay);
    pointer(&mut game, [file[0] + 12.0, file[1]], None);
    frame(&mut player, &mut game, &mut overlay);
    let [x, y, w, h] = game.view_rect().unwrap();
    let middle = [(x + w / 2) as f32, (y + h / 2) as f32];
    pointer(&mut game, middle, None);
    for _ in 0..30 {
        frame(&mut player, &mut game, &mut overlay);
    }
    pointer(&mut game, middle, Some(false));
    frame(&mut player, &mut game, &mut overlay);
    frame(&mut player, &mut game, &mut overlay);
    let world = player.world_mut();
    assert_eq!(world.len(), entities + 1, "one entity, his joints none");
    let man = engine_scene::find_named(world, "CesiumMan").expect("kept");
    assert_eq!(world.children(man).count(), 0);
    assert_eq!(
        world.get::<Model>(man).unwrap().mesh,
        AssetRef::part(id, "Cesium_Man")
    );
    let t = *world.get::<Transform>(man).unwrap();
    let up = t.rotation * Vec3::Z;
    println!(
        "placed at {} turned {:?}, his z up along {up}",
        t.translation, t.rotation
    );
    assert!(up.dot(Vec3::Y) > 0.999, "upright: {up}");
    assert!((t.translation.y + 0.5).abs() < 1e-3, "on the floor: {t:?}");
    assert_eq!(game.selection(), [man]);
    // Drawn skinned: his skin made in the background, then his Clips show.
    let started = Instant::now();
    while game.clips().0.is_empty() && started.elapsed() < Duration::from_secs(20) {
        frame(&mut player, &mut game, &mut overlay);
    }
    let (rows, _) = game.clips();
    assert_eq!(rows.len(), 1, "his walk: {rows:?}");
    assert!(player.model_rig(man).is_some(), "drawn skinned");
    if let Ok(out) = std::env::var("ENGINE_EDITOR_CHARACTER_PNG") {
        for _ in 0..30 {
            frame(&mut player, &mut game, &mut overlay);
        }
        let rgba = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(out, png).unwrap();
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
