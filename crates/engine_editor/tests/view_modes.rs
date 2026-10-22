//! The viewport's view modes (ADR 0116) in the editor's frame: Wireframe fills the view
//! with its background and draws the cube's edges; Lit + Wireframe draws its edges over the
//! lit cube; Unlit draws it in one flat color. `ENGINE_EDITOR_VIEW_MODES_PNG=<prefix>` writes
//! the frames.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Assets, Game, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_runtime::Sun;
use engine_scene::{AssetId, Name, SaveOptions, SceneId, SceneRegistry, save_with};
use std::path::{Path, PathBuf};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn start(_player: &mut Player) -> Result<Still, Box<dyn std::error::Error>> {
    Ok(Still)
}

/// One editor frame, as the editor's loop runs it.
fn frame<G: Game>(player: &mut Player, game: &mut EditorGame<G>, overlay: &mut EditorOverlay) {
    player.frame_overlaid(game, Some(overlay)).unwrap();
    player.set_view_rect(game.view_rect());
    game.sync_assets(player);
}

/// A cube in the middle, lit by the sun.
fn write_scene(dir: &Path) {
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Cube"),
        Transform::IDENTITY.with_scale(Vec3::splat(2.0)),
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
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The editor's frame as RGBA bytes, and its width.
fn capture<G: Game>(
    player: &mut Player,
    game: &mut EditorGame<G>,
    overlay: &mut EditorOverlay,
    name: &str,
) -> (Vec<u8>, u32) {
    let rgba = player
        .capture_overlaid(game, Some(overlay))
        .unwrap()
        .unwrap();
    let extent = player.extent();
    if let Ok(prefix) = std::env::var("ENGINE_EDITOR_VIEW_MODES_PNG") {
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(format!("{prefix}-{name}.png"), png).unwrap();
    }
    (rgba, extent.width)
}

#[test]
fn wireframe_modes_draw_the_edges_in_the_editors_view() {
    let dir = temp_dir("view-modes");
    write_scene(&dir);
    let settings = PlayerSettings {
        title: String::from("editor view modes test"),
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
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    let frames = |player: &mut Player,
                  game: &mut EditorGame<Still>,
                  overlay: &mut EditorOverlay,
                  n: usize| {
        for _ in 0..n {
            frame(player, game, overlay);
        }
    };
    frames(&mut player, &mut game, &mut overlay, 10);
    // The cube framed in the view, the camera at rest.
    assert!(game.run_command(player.world_mut(), "view.frame"));
    frames(&mut player, &mut game, &mut overlay, 180);
    let [vx, vy, vw, vh] = game.view_rect().unwrap();
    let (lit, width) = capture(&mut player, &mut game, &mut overlay, "lit");
    let pixel = |rgba: &[u8], x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    };
    // The view's pixels, away from its chips at the top left.
    let view = |rgba: &[u8]| -> Vec<[u8; 3]> {
        (vy + 40..vy + vh - 4)
            .step_by(2)
            .flat_map(|y| (vx + 4..vx + vw - 4).step_by(2).map(move |x| (x, y)))
            .map(|(x, y)| pixel(rgba, x, y))
            .collect()
    };
    // Wireframe: the background everywhere but on the edges, which are light.
    assert!(game.run_command(player.world_mut(), "view.wireframe"));
    frames(&mut player, &mut game, &mut overlay, 2);
    let (wire, _) = capture(&mut player, &mut game, &mut overlay, "wireframe");
    let background = [38u8, 40, 45];
    let near = |p: [u8; 3], q: [u8; 3]| p.iter().zip(q).all(|(&a, b)| a.abs_diff(b) <= 8);
    let pixels = view(&wire);
    let shown = pixels.iter().filter(|&&p| near(p, background)).count();
    let lines = pixels.iter().filter(|p| p.iter().all(|&c| c > 150)).count();
    println!(
        "wireframe: {shown} of {} pixels the background, {lines} on edges",
        pixels.len()
    );
    assert!(
        shown * 10 > pixels.len() * 9,
        "the background fills the view"
    );
    assert!(lines > 50, "the cube's edges");
    // Lit + Wireframe: the lit view, darker where the edges are.
    assert!(game.run_command(player.world_mut(), "view.lit_wireframe"));
    frames(&mut player, &mut game, &mut overlay, 2);
    let (over, _) = capture(&mut player, &mut game, &mut overlay, "lit-wireframe");
    let (before, after) = (view(&lit), view(&over));
    let darker = before
        .iter()
        .zip(&after)
        .filter(|(b, a)| {
            let sum = |p: &[u8; 3]| p.iter().map(|&c| u32::from(c)).sum::<u32>();
            sum(a) + 30 < sum(b)
        })
        .count();
    let same = before
        .iter()
        .zip(&after)
        .filter(|(b, a)| near(**b, **a))
        .count();
    println!(
        "lit + wireframe: {darker} pixels darker on edges, {same} of {} as lit",
        before.len()
    );
    assert!(darker > 30, "edges over the lit cube");
    assert!(same * 10 > before.len() * 9, "the rest as lit");
    // Unlit: the cube one flat color, every face alike, over the unlit view's background.
    assert!(game.run_command(player.world_mut(), "view.unlit"));
    frames(&mut player, &mut game, &mut overlay, 2);
    let (flat, _) = capture(&mut player, &mut game, &mut overlay, "unlit");
    let unlit_background = [63u8, 63, 66];
    let pixels = view(&flat);
    let surface: Vec<[u8; 3]> = pixels
        .iter()
        .copied()
        .filter(|&p| !near(p, unlit_background))
        .collect();
    let first = surface[0];
    let alike = surface.iter().filter(|&&p| near(p, first)).count();
    println!(
        "unlit: {} of {} pixels the cube, {alike} of them its one color {first:?}",
        surface.len(),
        pixels.len()
    );
    assert!(surface.len() > 1000, "the cube is drawn");
    assert!(alike * 100 > surface.len() * 95, "one flat color");
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "a measurement"]
fn measure_the_view_modes_over_ten_thousand_cubes() {
    let dir = temp_dir("view-modes-measure");
    let mut world = World::new();
    for i in 0..10_000 {
        let (x, z) = ((i % 100) as f32 * 1.5 - 75.0, (i / 100) as f32 * 1.5 - 75.0);
        world.spawn((
            SceneId::new(),
            Name::new(format!("Cube {i}")),
            Transform::from_xyz(x, 0.0, z).with_scale(Vec3::splat(0.5)),
            Model::new(builtin::CUBE, builtin::MATERIAL),
        ));
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
    let settings = PlayerSettings {
        title: String::from("editor view modes measurement"),
        window: Window::Offscreen {
            width: 1920,
            height: 1080,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut player = Player::new(&settings).unwrap();
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..10 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert!(game.run_command(player.world_mut(), "view.frame"));
    for _ in 0..180 {
        frame(&mut player, &mut game, &mut overlay);
    }
    println!("adapter: {}", player.device().adapter().summary());
    for mode in [
        "view.lit",
        "view.unlit",
        "view.wireframe",
        "view.lit_wireframe",
    ] {
        assert!(game.run_command(player.world_mut(), mode));
        let (mut gpu, mut cpu) = (Vec::new(), Vec::new());
        for k in 0..120 {
            let report = player
                .frame_overlaid(&mut game, Some(&mut overlay))
                .unwrap();
            player.set_view_rect(game.view_rect());
            game.sync_assets(&mut player);
            if k >= 20 {
                cpu.push(report.map_or(0.0, |r| r.compose));
                let wire = player
                    .graph()
                    .gpu_pass_times()
                    .iter()
                    .filter(|t| t.name == "wireframe" || t.name == "unlit")
                    .map(|t| t.gpu_us)
                    .sum::<f32>();
                gpu.push(f64::from(wire));
            }
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "{mode}: its pass {:.0} µs on the GPU; composing the frame {:.0} µs (medians of 100)",
            median(&mut gpu),
            median(&mut cpu)
        );
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
