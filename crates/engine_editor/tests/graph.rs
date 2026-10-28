//! A material graph's tab on the player (ADR 0123 E10-3d): the session brings the tab back,
//! its canvas draws where the viewport is, under the panels: the grid, the boxes with their
//! title bars by group, the links in their outputs' colours; the sphere of its preview in
//! the corner (E10-3f-2), following a value typed and a link made. `ENGINE_EDITOR_GRAPH_PNG=
//! <file>` writes the frame.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_ecs::World;
use engine_editor::{EditorGame, EditorOverlay};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind, WindowId};
use engine_scene::AssetId;
use std::time::Instant;

struct NoGame;

impl Game for NoGame {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn key<G: Game>(game: &mut EditorGame<G>, vk: u16, scancode: u16, down: bool) {
    send(
        game,
        WindowEventKind::Key {
            virtual_key: vk,
            scancode,
            extended: false,
            pressed: down,
            repeat: false,
        },
    );
}

fn send<G: Game>(game: &mut EditorGame<G>, kind: WindowEventKind) {
    game.event(&WindowEvent {
        window: WindowId(0),
        time: Instant::now(),
        kind,
    });
}

fn pointer<G: Game>(game: &mut EditorGame<G>, x: i32, y: i32) {
    send(game, WindowEventKind::CursorMoved { x, y });
}

fn button<G: Game>(game: &mut EditorGame<G>, pressed: bool) {
    send(
        game,
        WindowEventKind::MouseButton {
            button: MouseButton::Left,
            pressed,
        },
    );
}

/// A left click where the pointer is, a frame each for the press and the release.
fn click(player: &mut Player, game: &mut EditorGame<NoGame>, overlay: &mut EditorOverlay) {
    player.frame_overlaid(game, Some(&mut *overlay)).unwrap();
    for pressed in [true, false] {
        button(game, pressed);
        player.frame_overlaid(game, Some(&mut *overlay)).unwrap();
    }
}

/// Frames, the assets the editor asks for given before each as the editor's binary does,
/// until `done` holds of one: that frame, how many, and the seconds they took.
fn until(
    player: &mut Player,
    game: &mut EditorGame<NoGame>,
    overlay: &mut EditorOverlay,
    done: &dyn Fn(&[u8]) -> bool,
) -> (Vec<u8>, usize, f64) {
    let timer = engine_platform::time::HighResTimer::new().unwrap();
    let started = Instant::now();
    let mut frames = 0;
    loop {
        game.sync_assets(player);
        let shot = player
            .capture_overlaid(game, Some(&mut *overlay))
            .unwrap()
            .unwrap();
        frames += 1;
        if done(&shot) {
            return (shot, frames, started.elapsed().as_secs_f64());
        }
        assert!(
            started.elapsed().as_secs() < 90,
            "not drawn after {frames} frames"
        );
        let _ = timer.sleep(std::time::Duration::from_millis(5));
    }
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

/// A lit graph: a colour multiplied by a number (through a reroute) into the base, the two
/// in a frame, a number into the roughness, a magenta colour linked to nothing.
fn moss(id: AssetId) -> String {
    format!(
        "[mgraph version=2 id={id}]\n\nmodel = \"lit\"\n\
         nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].at = [0, 0]\n\
         nodes[\"c\"].value = [0.25, 0.55, 0.2, 1]\nnodes[\"c\"].param = \"tint\"\n\
         nodes[\"k\"].kind = \"number\"\nnodes[\"k\"].at = [0, 110]\n\
         nodes[\"k\"].value = [0.8]\n\
         nodes[\"m\"].kind = \"multiply\"\nnodes[\"m\"].at = [220, 30]\n\
         nodes[\"r\"].kind = \"number\"\nnodes[\"r\"].at = [220, 190]\n\
         nodes[\"r\"].value = [0.6]\nnodes[\"r\"].param = \"roughness\"\n\
         nodes[\"e\"].kind = \"color\"\nnodes[\"e\"].at = [0, 185]\n\
         nodes[\"e\"].value = [1, 0, 1, 1]\n\
         nodes[\"rr\"].kind = \"reroute\"\nnodes[\"rr\"].at = [160, 150]\n\
         frames[\"frame1\"].at = [-16, -40]\nframes[\"frame1\"].size = [200, 210]\n\
         frames[\"frame1\"].title = \"Tint\"\n\
         nodes[\"out\"].kind = \"output\"\nnodes[\"out\"].at = [460, 0]\n\
         links[\"m.a\"] = \"c.rgba\"\nlinks[\"rr.in\"] = \"k.value\"\nlinks[\"m.b\"] = \"rr.out\"\n\
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
    // The preview (E10-3f-2): a sphere of the graph in the canvas's top right once its
    // editing build is compiled; its green against the canvas (the colour's swatch is
    // under 500 pixels of it).
    let green = |rgba: &[u8]| {
        rgba.chunks_exact(4)
            .filter(|p| {
                let (r, g, b) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
                g > 1.3 * r && g > 1.3 * b && g > 60.0
            })
            .count()
    };
    let (rgba, _, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        green(shot) > 3_000
    });
    println!(
        "the preview drawn {took:.2} s after the tab: {} green pixels",
        green(&rgba)
    );
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
    // Numbers live (the team's «а», E10-3f-2): the colour clicked on the canvas, its green
    // typed in the inspector, the sphere loses its green with no code compiled (the editing
    // build's numbers are parameters); then the unlinked magenta colour linked into the
    // emission, a parameter the module did not have, compiled. The positions are this
    // graph's at its framing (100 %) in the window's first layout.
    let before = green(&rgba);
    pointer(&mut game, 100, 173);
    click(&mut player, &mut game, &mut overlay);
    pointer(&mut game, 1140, 155);
    click(&mut player, &mut game, &mut overlay);
    for c in "0.05".chars() {
        send(&mut game, WindowEventKind::Char(c));
    }
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    key(&mut game, 0x0D, 0x1C, true);
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        green(shot) < 300
    });
    key(&mut game, 0x0D, 0x1C, false);
    println!(
        "a value typed: on the sphere after {took:.3} s, {frames} frames ({before} -> {} \
         green pixels)",
        green(&shot)
    );
    let pink = |rgba: &[u8]| {
        rgba.chunks_exact(4)
            .filter(|p| p[0] > 180 && p[2] > 180 && p[1] < 150)
            .count()
    };
    let before = pink(&shot);
    pointer(&mut game, 169, 382);
    button(&mut game, true);
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    for (x, y) in [(350, 330), (519, 284)] {
        pointer(&mut game, x, y);
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
    }
    button(&mut game, false);
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        pink(shot) > before + 3_000
    });
    println!(
        "a link with a new parameter: on the sphere after {took:.3} s, {frames} frames \
         ({before} -> {} magenta pixels)",
        pink(&shot)
    );
    // Space over the canvas: the node search at the pointer, "mul" typed
    // (`ENGINE_EDITOR_GRAPH_SEARCH_PNG=<file>` writes it); Escape closes it.
    if let Ok(path) = std::env::var("ENGINE_EDITOR_GRAPH_SEARCH_PNG") {
        pointer(&mut game, 200, 430);
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
        key(&mut game, 0x20, 0x39, true);
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
        key(&mut game, 0x20, 0x39, false);
        for c in "mul".chars() {
            send(&mut game, WindowEventKind::Char(c));
        }
        for _ in 0..3 {
            player
                .frame_overlaid(&mut game, Some(&mut overlay))
                .unwrap();
        }
        let shot = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &shot,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
        key(&mut game, 0x1B, 0x01, true);
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
        key(&mut game, 0x1B, 0x01, false);
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

/// E10-3g-1: Slang's error in an Expression node's code framed at the node, what it says
/// in the bar, the sphere not drawn; kept while the node is dragged (the code the same, no
/// frame without it); the code typed right in the inspector, the frame gone and the sphere
/// drawn.
#[test]
fn an_expressions_slang_error_is_framed_at_its_node() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-expression-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    std::fs::write(
        dir.join("materials/glow.mgraph"),
        format!(
            "[mgraph version=2 id={}]\n\nmodel = \"lit\"\n\
             nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].value = [0.2, 0.6, 0.2, 1]\n\
             nodes[\"x\"].kind = \"expression\"\nnodes[\"x\"].at = [200, 0]\n\
             nodes[\"x\"].code = \"a * oops\"\n\
             nodes[\"out\"].kind = \"output\"\nnodes[\"out\"].at = [460, 0]\n\
             links[\"x.a\"] = \"c.rgba\"\nlinks[\"out.base\"] = \"x.out\"\n",
            AssetId::new()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \
         \"materials/glow.mgraph\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor expression test"),
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
    // The error's colour on the canvas (left of the docks, under the bar): the node's
    // frame and the message under it (the inspector's X labels are red too).
    let red = |rgba: &[u8]| {
        rgba.chunks_exact(4)
            .enumerate()
            .filter(|(i, p)| {
                let (x, y) = (*i as u32 % width, *i as u32 / width);
                x < 680 && (64..495).contains(&y) && p[0] > 190 && p[1] < 120 && p[2] < 120
            })
            .count()
    };
    let green = |rgba: &[u8]| {
        rgba.chunks_exact(4)
            .filter(|p| {
                let (r, g, b) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
                g > 1.3 * r && g > 1.3 * b && g > 60.0
            })
            .count()
    };
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        red(shot) > 150
    });
    println!(
        "Slang's error at its node after {took:.2} s, {frames} frames: {} red pixels",
        red(&shot)
    );
    let said = game.message().unwrap_or_default();
    assert!(!said.contains("could not"), "{said}");
    assert!(green(&shot) < 3_000, "no sphere: {}", green(&shot));
    // The node dragged by its title bar: the same code, the frame on every frame.
    pointer(&mut game, 330, 180);
    button(&mut game, true);
    for (x, y) in [(334, 182), (340, 186), (346, 190)] {
        pointer(&mut game, x, y);
        game.sync_assets(&mut player);
        let shot = player
            .capture_overlaid(&mut game, Some(&mut overlay))
            .unwrap()
            .unwrap();
        assert!(red(&shot) > 150, "framed while dragged: {}", red(&shot));
    }
    button(&mut game, false);
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    // The code typed right in the inspector's Code row (the node selected by its drag):
    // compiled, the frame gone, the sphere drawn.
    pointer(&mut game, 1160, 129);
    click(&mut player, &mut game, &mut overlay);
    key(&mut game, 0x11, 0x1D, true);
    key(&mut game, 0x41, 0x1E, true);
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    key(&mut game, 0x41, 0x1E, false);
    key(&mut game, 0x11, 0x1D, false);
    for c in "a * 0.9".chars() {
        send(&mut game, WindowEventKind::Char(c));
    }
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    key(&mut game, 0x0D, 0x1C, true);
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        red(shot) < 10 && green(shot) > 3_000
    });
    key(&mut game, 0x0D, 0x1C, false);
    println!(
        "the code typed right: drawn after {took:.2} s, {frames} frames ({} red, {} green \
         pixels)",
        red(&shot),
        green(&shot)
    );
    if let Ok(path) = std::env::var("ENGINE_EDITOR_EXPRESSION_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &shot,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A project module's `[Node]` function: the colour it is given toward magenta as `amount`
/// falls (magenta where nothing is linked to it).
const TINT: &str = "#include \"nodes.slang\"\n\n\
[Node(\"Tint\", \"Color\")]\n\
float3 tint(MaterialContext ctx, float3 color, float amount)\n\
{\n    return lerp(float3(1.0, 0.0, 1.0), color, amount);\n}\n";

/// E10-3g-3b: a call whose kept signature is behind its function (the module changed since
/// the graph was written) is made the function's again once the editor has scanned the
/// module; the preview's module includes it and the sphere draws what the function gives,
/// magenta, which the stale signature could not compile.
#[test]
fn a_calls_signature_follows_its_function_and_the_preview_draws_it() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-call-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["materials", "shaders", ".ofelia"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    let module = AssetId::new();
    std::fs::write(dir.join("shaders/tint.slang"), TINT).unwrap();
    std::fs::write(
        dir.join("shaders/tint.slang.meta"),
        format!("[asset version=1 id={module}]\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("materials/tinted.mgraph"),
        format!(
            "[mgraph version=3 id={}]\n\nmodel = \"unlit\"\n\
             nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].value = [0.1, 0.8, 0.1, 1]\n\
             nodes[\"t\"].kind = \"call\"\nnodes[\"t\"].at = [200, 0]\n\
             nodes[\"t\"].function = \"tint\"\n\
             nodes[\"t\"].module = asset(\"shaders/tint.slang\", {module})\n\
             nodes[\"t\"].signature = \"float3(MaterialContext ctx, float3 color)\"\n\
             nodes[\"out\"].kind = \"output\"\nnodes[\"out\"].at = [460, 0]\n\
             links[\"t.color\"] = \"c.rgba\"\nlinks[\"out.base\"] = \"t.out\"\n",
            AssetId::new()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \
         \"materials/tinted.mgraph\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor call test"),
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
    let magenta = |rgba: &[u8]| {
        rgba.chunks_exact(4)
            .filter(|p| p[0] > 180 && p[2] > 180 && p[1] < 90)
            .count()
    };
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        magenta(shot) > 3_000
    });
    println!(
        "the call made its function's and drawn after {took:.2} s, {frames} frames: {} \
         magenta pixels",
        magenta(&shot)
    );
    if let Ok(path) = std::env::var("ENGINE_EDITOR_CALL_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &shot,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
