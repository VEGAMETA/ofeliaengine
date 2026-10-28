//! A material graph's tab on the player (ADR 0123 E10-3d): the session brings the tab back,
//! its canvas draws where the viewport is, under the panels: the grid, the boxes with their
//! title bars by group, the links in their outputs' colours. `ENGINE_EDITOR_GRAPH_PNG=<file>`
//! writes the frame.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_ecs::World;
use engine_platform::window::{WindowEvent, WindowEventKind, WindowId};
use engine_scene::AssetId;
use std::time::Instant;

struct NoGame;

impl Game for NoGame {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn key<G: Game>(game: &mut engine_editor::EditorGame<G>, vk: u16, scancode: u16, down: bool) {
    game.event(&WindowEvent {
        window: WindowId(0),
        time: Instant::now(),
        kind: WindowEventKind::Key {
            virtual_key: vk,
            scancode,
            extended: false,
            pressed: down,
            repeat: false,
        },
    });
}

/// Pixels of a hue in an RGBA frame: the inputs' teal title bars, the maths' blue one, the
/// pink link of the colour (a four-wide output).
fn hues(rgba: &[u8]) -> [usize; 3] {
    let count = |test: &dyn Fn(f32, f32, f32) -> bool| {
        rgba.chunks_exact(4)
            .filter(|p| test(f32::from(p[0]), f32::from(p[1]), f32::from(p[2])))
            .count()
    };
    [
        count(&|r, g, b| g > 1.6 * r && b > 1.5 * r && (g - b).abs() < 20.0 && g < 140.0),
        count(&|r, g, b| b > 1.8 * r && b > 1.4 * g && (90.0..170.0).contains(&b)),
        count(&|r, g, b| r > 1.2 * g && b > 1.1 * g && r > 150.0),
    ]
}

/// A lit graph: a colour multiplied by a number into the base, a number into the roughness.
fn moss(id: AssetId) -> String {
    format!(
        "[mgraph version=1 id={id}]\n\nmodel = \"lit\"\n\
         nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].at = [0, 0]\n\
         nodes[\"c\"].value = [0.25, 0.55, 0.2, 1]\nnodes[\"c\"].param = \"tint\"\n\
         nodes[\"k\"].kind = \"number\"\nnodes[\"k\"].at = [0, 110]\n\
         nodes[\"k\"].value = [0.8]\n\
         nodes[\"m\"].kind = \"multiply\"\nnodes[\"m\"].at = [220, 30]\n\
         nodes[\"r\"].kind = \"number\"\nnodes[\"r\"].at = [220, 190]\n\
         nodes[\"r\"].value = [0.6]\nnodes[\"r\"].param = \"roughness\"\n\
         nodes[\"out\"].kind = \"output\"\nnodes[\"out\"].at = [460, 0]\n\
         links[\"m.a\"] = \"c.rgba\"\nlinks[\"m.b\"] = \"k.value\"\n\
         links[\"out.base\"] = \"m.out\"\nlinks[\"out.roughness\"] = \"r.value\"\n"
    )
}

#[test]
fn a_graphs_tab_draws_its_canvas_where_the_viewport_is() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-graph-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    std::fs::write(dir.join("materials/moss.mgraph"), moss(AssetId::new())).unwrap();
    // The last session left the graph's tab shown.
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \
         \"materials/moss.mgraph\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor graph test"),
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
    game.begin_session(&mut player).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..4 {
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
        player.set_view_rect(game.view_rect());
    }
    let said = game.message().unwrap_or_default();
    assert!(!said.contains("could not"), "{said}");
    let rgba = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    if let Ok(path) = std::env::var("ENGINE_EDITOR_GRAPH_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // Ctrl+Tab: the scene's tab, the viewport where the canvas was.
    key(&mut game, 0x11, 0x1D, true);
    key(&mut game, 0x09, 0x0F, true);
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    key(&mut game, 0x09, 0x0F, false);
    key(&mut game, 0x11, 0x1D, false);
    for _ in 0..2 {
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
    }
    let scene = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    let ([teal, blue, pink], [t0, b0, p0]) = (hues(&rgba), hues(&scene));
    println!("graph: teal {teal}, blue {blue}, pink {pink}; scene: {t0}, {b0}, {p0}");
    assert!(teal > 4_000, "three input boxes' title bars: {teal}");
    assert!(blue > 1_500, "the multiply's title bar: {blue}");
    assert!(pink > 30, "the colour's link and its ports: {pink}");
    // The scene's tab: the canvas gone with its boxes and links (the viewport's own blue
    // aside).
    assert!(t0 < 500 && p0 < 10, "no canvas behind the scene: {t0} {p0}");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
