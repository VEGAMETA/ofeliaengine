//! The project's assets in the editor (ADR 0108, ADR 0107 E4-1): the asset database opened
//! with the editor, each foreign file given a `.meta`, the player told every file's path by
//! its id, and the folder followed as files come, move and go.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_ecs::World;
use engine_editor::{EditorGame, EditorOverlay};
use engine_scene::{AssetId, AssetKind, AssetMeta, AssetRef};
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
