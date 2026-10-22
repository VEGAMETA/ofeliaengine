//! The Output panel (ADR 0107 E6) on the player: the engine's log read from its ring, the
//! same message folded, opened with "View: Output" (Ctrl+J) under the viewport, which gives
//! it room. `ENGINE_EDITOR_OUTPUT_PNG=<file>` writes the frame with it open.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_ecs::World;

struct NoGame;

impl Game for NoGame {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn frames<G: Game>(
    player: &mut Player,
    game: &mut engine_editor::EditorGame<G>,
    overlay: &mut engine_editor::EditorOverlay,
    n: usize,
) {
    for _ in 0..n {
        // As the editor's own loop does: the frame's report for the profiler's panel.
        if let Some(report) = player.frame_overlaid(game, Some(overlay)).unwrap() {
            game.note_frame(&report, player);
        }
        player.set_view_rect(game.view_rect());
    }
}

#[test]
fn the_output_shows_the_log_under_the_viewport() {
    // The log the panel reads: the process's own (another test may have installed it).
    let _log = engine_log::init(&engine_log::LogConfig {
        stderr: false,
        debugger: false,
        tracy: false,
        ..engine_log::LogConfig::default()
    });
    let dir = std::env::temp_dir().join(format!("ofelia-editor-output-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor output test"),
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
    frames(&mut player, &mut game, &mut overlay, 5);
    let [_, _, _, closed] = game.view_rect().unwrap();
    log::info!(target: "game", "the level loaded in 12 ms");
    for _ in 0..30 {
        log::error!(target: "assets", "props/lamp.scene: the file has no part named Shade");
    }
    log::warn!(target: "physics", "a body fell below the world (y = -1000)");
    assert!(game.run_command(player.world_mut(), "view.output"));
    frames(&mut player, &mut game, &mut overlay, 5);
    let [_, _, _, open] = game.view_rect().unwrap();
    assert!(
        open + 200 < closed,
        "the viewport gives the Output room: {closed} -> {open}"
    );
    if let Ok(path) = std::env::var("ENGINE_EDITOR_OUTPUT_PNG") {
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
        std::fs::write(path, png).unwrap();
    }
    // Closed again: the viewport as it was.
    assert!(game.run_command(player.world_mut(), "view.output"));
    frames(&mut player, &mut game, &mut overlay, 3);
    assert_eq!(game.view_rect().unwrap()[3], closed);
    // The profiler's panel: the frames drawn since, their stages and the GPU's passes.
    assert!(game.run_command(player.world_mut(), "view.profiler"));
    frames(&mut player, &mut game, &mut overlay, 10);
    if let Ok(path) = std::env::var("ENGINE_EDITOR_PROFILER_PNG") {
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
        std::fs::write(path, png).unwrap();
    }
    // The Tasks below and the History in the tree's place (`ENGINE_EDITOR_PANELS_PNG=<file>`).
    assert!(game.run_command(player.world_mut(), "view.tasks"));
    assert!(game.run_command(player.world_mut(), "view.history"));
    frames(&mut player, &mut game, &mut overlay, 3);
    if let Ok(path) = std::env::var("ENGINE_EDITOR_PANELS_PNG") {
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
        std::fs::write(path, png).unwrap();
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
