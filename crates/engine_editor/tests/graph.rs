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
        "[session version=1]\n\n[editor]\ngraph = 0\nnode_previews = false\n\n[graph]\npath = \
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
        "[session version=1]\n\n[editor]\ngraph = 0\nnode_previews = false\n\n[graph]\npath = \
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
        "[session version=1]\n\n[editor]\ngraph = 0\nnode_previews = false\n\n[graph]\npath = \
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

/// E10-3g-2b: a function's tab previews the function, its first output on the sphere, as
/// its tab holds it: the session's unsaved edit (its input green), not its file's value
/// (magenta).
#[test]
fn a_functions_tab_previews_it_as_the_tab_holds_it() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-function-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["materials", ".ofelia/autosave"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    let id = AssetId::new();
    let tint = |value: &str| {
        format!(
            "[mfunc version=1 id={id}]\n\n\
             nodes[\"c\"].kind = \"input_float3\"\nnodes[\"c\"].param = \"color\"\n\
             nodes[\"c\"].value = [{value}]\n\
             nodes[\"o\"].kind = \"output_float3\"\nnodes[\"o\"].at = [300, 0]\n\
             nodes[\"o\"].param = \"tinted\"\n\
             links[\"o.value\"] = \"c.value\"\n"
        )
    };
    std::fs::write(dir.join("materials/tint.mfunc"), tint("1, 0, 1, 1")).unwrap();
    std::fs::write(
        dir.join(".ofelia/autosave/tint.mfunc"),
        tint("0.1, 0.8, 0.1, 1"),
    )
    .unwrap();
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \
         \"materials/tint.mfunc\"\nautosave = \"autosave/tint.mfunc\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor function test"),
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
    let count = |rgba: &[u8], test: &dyn Fn(&[u8]) -> bool| {
        rgba.chunks_exact(4).filter(|p| test(p)).count()
    };
    let green = |p: &[u8]| p[1] > 150 && p[0] < 90 && p[2] < 90;
    let magenta = |p: &[u8]| p[0] > 180 && p[2] > 180 && p[1] < 90;
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        count(shot, &green) > 3_000
    });
    println!(
        "the function's tab drew its unsaved edit after {took:.2} s, {frames} frames: {} \
         green pixels, {} magenta",
        count(&shot, &green),
        count(&shot, &magenta)
    );
    assert!(
        count(&shot, &magenta) < 500,
        "the file's value drawn, not the tab's"
    );
    if let Ok(path) = std::env::var("ENGINE_EDITOR_FUNCTION_PNG") {
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

/// E10-4g: a material graph's nodes show their values under their ports, drawn by the
/// engine from the graph's editing build: a red and a green colour as squares of their
/// colours (each far larger than its swatch), the surface's normal on a sphere; Graph: Node
/// Previews turns them off, the boxes shorter again.
#[test]
fn a_graphs_nodes_show_their_previews_under_their_ports() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-tiles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    let id = AssetId::new();
    std::fs::write(
        dir.join("materials/tiles.mgraph"),
        format!(
            "[mgraph version=2 id={id}]\n\nmodel = \"unlit\"\n\
             nodes[\"a\"].kind = \"color\"\nnodes[\"a\"].at = [0, 0]\n\
             nodes[\"a\"].value = [0.9, 0.04, 0.04, 1]\n\
             nodes[\"b\"].kind = \"color\"\nnodes[\"b\"].at = [0, 180]\n\
             nodes[\"b\"].value = [0.04, 0.9, 0.04, 1]\n\
             nodes[\"n\"].kind = \"normal\"\nnodes[\"n\"].at = [200, 180]\n\
             nodes[\"s\"].kind = \"add\"\nnodes[\"s\"].at = [200, 0]\n\
             nodes[\"out\"].kind = \"output\"\nnodes[\"out\"].at = [400, 0]\n\
             links[\"s.a\"] = \"a.rgba\"\nlinks[\"s.b\"] = \"b.rgba\"\n\
             links[\"out.base\"] = \"s.out\"\n"
        ),
    )
    .unwrap();
    // The last session left the graph's tab shown (previews on, as by default).
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \
         \"materials/tiles.mgraph\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor node previews test"),
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
    let count = |rgba: &[u8], test: &dyn Fn(f32, f32, f32) -> bool| {
        rgba.chunks_exact(4)
            .filter(|p| test(f32::from(p[0]), f32::from(p[1]), f32::from(p[2])))
            .count()
    };
    let red = |rgba: &[u8]| count(rgba, &|r, g, b| r > 150.0 && g < 90.0 && b < 90.0);
    let green = |rgba: &[u8]| count(rgba, &|r, g, b| g > 150.0 && r < 90.0 && b < 90.0);
    let blue = |rgba: &[u8]| count(rgba, &|r, g, b| b > 150.0 && r < 90.0 && g < 90.0);
    // Their tiles, each far more than a colour's swatch (28 by 14 points); the normal as
    // it is, so blue alone where it points away from x and y (a quarter of the sphere).
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        red(shot) > 2_000 && green(shot) > 2_000 && blue(shot) > 600
    });
    println!(
        "the nodes' previews drawn {took:.2} s ({frames} frames) after the tab: red {}, green \
         {}, blue {}",
        red(&shot),
        green(&shot),
        blue(&shot)
    );
    if let Ok(path) = std::env::var("ENGINE_EDITOR_TILES_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &shot,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    // Numbers live: the red colour selected by its title, its green typed in the inspector
    // (as the first test does), its tile and its swatch yellow with no code compiled.
    pointer(&mut game, 140, 138);
    click(&mut player, &mut game, &mut overlay);
    pointer(&mut game, 1140, 155);
    click(&mut player, &mut game, &mut overlay);
    for c in "0.9".chars() {
        send(&mut game, WindowEventKind::Char(c));
    }
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    key(&mut game, 0x0D, 0x1C, true);
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        red(shot) < 1_000
    });
    key(&mut game, 0x0D, 0x1C, false);
    println!(
        "a value typed: on its tile after {took:.3} s, {frames} frames ({} red pixels)",
        red(&shot)
    );
    // A change of code: the normal linked into the sum in the green's place, by a drag from
    // its port; the sum's tile and the sphere no longer yellow once the module is compiled
    // again (the positions are this graph's at its framing, the first test's way).
    let yellow = |rgba: &[u8]| count(rgba, &|r, g, b| r > 150.0 && g > 150.0 && b < 90.0);
    let before = yellow(&shot);
    pointer(&mut game, 398, 341);
    button(&mut game, true);
    player
        .frame_overlaid(&mut game, Some(&mut overlay))
        .unwrap();
    for (x, y) in [(340, 260), (289, 183)] {
        pointer(&mut game, x, y);
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
    }
    button(&mut game, false);
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        yellow(shot) < before - 6_000
    });
    println!(
        "a link changing the code: on the sphere and its tile after {took:.3} s, {frames} \
         frames ({before} -> {} yellow pixels)",
        yellow(&shot)
    );
    // Off: no tiles, the colours' swatches alone.
    assert!(game.run_command(player.world_mut(), "graph.node_previews"));
    let (shot, ..) = until(&mut player, &mut game, &mut overlay, &|shot| {
        red(shot) < 1_000 && green(shot) < 1_000
    });
    assert!(
        blue(&shot) < 100,
        "the normal's sphere gone: {}",
        blue(&shot)
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// E10-4g-3: a function call shows what it gives (green inverted to magenta, its output
/// read by nothing, so only its tile is magenta); in the function's own tab, its nodes show
/// theirs: the input the value its preview's call gives it (red), the One Minus and the
/// output what they make of it (cyan).
#[test]
fn a_calls_and_a_functions_nodes_show_their_previews() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-call-tiles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    let function = AssetId::new();
    std::fs::write(
        dir.join("materials/invert.mfunc"),
        format!(
            "[mfunc version=1 id={function}]\n\n\
             nodes[\"c\"].kind = \"input_float3\"\nnodes[\"c\"].param = \"color\"\n\
             nodes[\"c\"].value = [0.9, 0.04, 0.04, 1]\n\
             nodes[\"s\"].kind = \"one_minus\"\nnodes[\"s\"].at = [200, 0]\n\
             nodes[\"o\"].kind = \"output_float3\"\nnodes[\"o\"].at = [400, 0]\n\
             nodes[\"o\"].param = \"inverted\"\n\
             links[\"s.in\"] = \"c.value\"\nlinks[\"o.value\"] = \"s.out\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("materials/uses.mgraph"),
        format!(
            "[mgraph version=3 id={}]\n\nmodel = \"unlit\"\n\
             nodes[\"g\"].kind = \"color\"\nnodes[\"g\"].value = [0.04, 0.9, 0.04, 1]\n\
             nodes[\"f\"].kind = \"function\"\nnodes[\"f\"].at = [200, 0]\n\
             nodes[\"f\"].function = \"invert\"\n\
             nodes[\"f\"].module = asset(\"materials/invert.mfunc\", {function})\n\
             nodes[\"f\"].signature = \"void(float3 color, out float3 inverted)\"\n\
             nodes[\"out\"].kind = \"output\"\nnodes[\"out\"].at = [460, 0]\n\
             links[\"f.color\"] = \"g.rgba\"\nlinks[\"out.base\"] = \"g.rgba\"\n",
            AssetId::new()
        ),
    )
    .unwrap();
    // Both open, the graph's tab shown.
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \
         \"materials/uses.mgraph\"\n\n[graph]\npath = \"materials/invert.mfunc\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor call previews test"),
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
    let count = |rgba: &[u8], test: &dyn Fn(f32, f32, f32) -> bool| {
        rgba.chunks_exact(4)
            .filter(|p| test(f32::from(p[0]), f32::from(p[1]), f32::from(p[2])))
            .count()
    };
    let red = |rgba: &[u8]| count(rgba, &|r, g, b| r > 150.0 && g < 90.0 && b < 90.0);
    let green = |rgba: &[u8]| count(rgba, &|r, g, b| g > 150.0 && r < 90.0 && b < 90.0);
    let magenta = |rgba: &[u8]| count(rgba, &|r, g, b| r > 150.0 && b > 150.0 && g < 90.0);
    let cyan = |rgba: &[u8]| count(rgba, &|r, g, b| g > 150.0 && b > 150.0 && r < 90.0);
    let png = |name: &str, shot: &[u8]| {
        if let Ok(dir) = std::env::var("ENGINE_EDITOR_TILES_DIR") {
            let png = engine_render::png::encode_png(
                width,
                height,
                engine_render::png::PngFormat::Rgba,
                shot,
            )
            .unwrap();
            std::fs::write(std::path::Path::new(&dir).join(name), png).unwrap();
        }
    };
    // The call's tile (a tile's 4,096 pixels, nothing else magenta).
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        magenta(shot) > 3_000 && green(shot) > 3_000
    });
    println!(
        "the call's preview drawn {took:.2} s ({frames} frames) after the tab: magenta {}, \
         green {}",
        magenta(&shot),
        green(&shot)
    );
    png("call.png", &shot);
    // The function's tab: its input red, its One Minus and its output cyan, the sphere too.
    assert!(game.run_command(player.world_mut(), "file.next_tab"));
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        red(shot) > 3_000 && cyan(shot) > 2 * 4_096 + 2_000
    });
    println!(
        "the function's previews drawn {took:.2} s ({frames} frames) after its tab: red {}, \
         cyan {}",
        red(&shot),
        cyan(&shot)
    );
    png("function.png", &shot);
    assert!(
        magenta(&shot) < 500,
        "the graph's tiles gone: {}",
        magenta(&shot)
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// An animation graph (ADR 0124 E12-5d-1): Idle (the entry), Walk and Run; transitions both
/// ways between Idle and Walk, one from Walk into Run, and one from Any State into Run whose
/// condition names a parameter the graph lacks.
fn hero(id: AssetId) -> String {
    format!(
        "[agraph version=1 id={id}]\n\n\
         params[\"speed\"].kind = Float\n\
         layers[\"base\"].name = \"Base\"\nlayers[\"base\"].root = \"m\"\n\
         nodes[\"m\"].kind = Machine\nnodes[\"m\"].entry = \"idle\"\n\
         nodes[\"m\"].at = [-220, -120]\n\
         nodes[\"ci\"].clip = \"Idle\"\nnodes[\"cw\"].clip = \"Walk\"\n\
         nodes[\"cr\"].clip = \"Run\"\n\
         states[\"idle\"].machine = \"m\"\nstates[\"idle\"].name = \"Idle\"\n\
         states[\"idle\"].pose = \"ci\"\n\
         states[\"walk\"].at = [320, 0]\nstates[\"walk\"].machine = \"m\"\n\
         states[\"walk\"].name = \"Walk\"\nstates[\"walk\"].pose = \"cw\"\n\
         states[\"run\"].at = [320, 200]\nstates[\"run\"].machine = \"m\"\n\
         states[\"run\"].name = \"Run\"\nstates[\"run\"].pose = \"cr\"\n\
         transitions[\"t1\"].from = \"idle\"\ntransitions[\"t1\"].to = \"walk\"\n\
         transitions[\"t1\"].when = \"speed > 0.1\"\n\
         transitions[\"t2\"].from = \"walk\"\ntransitions[\"t2\"].to = \"idle\"\n\
         transitions[\"t2\"].when = \"speed <= 0.1\"\n\
         transitions[\"t3\"].from = \"walk\"\ntransitions[\"t3\"].to = \"run\"\n\
         transitions[\"t3\"].when = \"speed > 3\"\n\
         transitions[\"t4\"].to = \"run\"\ntransitions[\"t4\"].when = \"sprint\"\n"
    )
}

/// ADR 0124 E12-5d-1: an animation graph's tab draws its canvas where the viewport is, under
/// the panels: the states' title bars (the entry's amber, the others' blue), the Entry tag,
/// the Any State box, the transitions' arrows, the one whose condition does not compile in
/// red; all gone behind the scene's tab. `ENGINE_EDITOR_AGRAPH_PNG=<file>` writes the frame.
#[test]
fn an_animation_graphs_tab_draws_its_states_and_transitions() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-agraph-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["anim", ".ofelia"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    std::fs::write(dir.join("anim/hero.agraph"), hero(AssetId::new())).unwrap();
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \"anim/hero.agraph\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor animation graph test"),
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
    let count = |rgba: &[u8], test: &dyn Fn(f32, f32, f32) -> bool| {
        rgba.chunks_exact(4)
            .filter(|p| test(f32::from(p[0]), f32::from(p[1]), f32::from(p[2])))
            .count()
    };
    // sRGB about 120 74 23, 55 74 119, 30 95 105, 40 110 60, 236 104 96.
    let amber = |r: f32, g: f32, b: f32| r > 1.4 * g && g > 2.0 * b && (95.0..150.0).contains(&r);
    let blue = |r: f32, g: f32, b: f32| b > 1.8 * r && b > 1.4 * g && (95.0..145.0).contains(&b);
    let teal =
        |r: f32, g: f32, b: f32| g > 2.4 * r && b > 2.4 * r && (g - b).abs() < 25.0 && g < 130.0;
    let green = |r: f32, g: f32, b: f32| g > 2.2 * r && g > 1.5 * b && (90.0..135.0).contains(&g);
    let red = |r: f32, g: f32, b: f32| r > 200.0 && g < 130.0 && b < 125.0 && r > 1.8 * g;
    let (shot, frames, took) = until(&mut player, &mut game, &mut overlay, &|shot| {
        count(shot, &amber) > 1_000
    });
    let said = game.message().unwrap_or_default();
    assert!(!said.contains("could not"), "{said}");
    let tests: [&dyn Fn(f32, f32, f32) -> bool; 5] = [&amber, &blue, &teal, &green, &red];
    let [a, b, t, g, r] = tests.map(|f| count(&shot, f));
    println!(
        "drawn after {took:.2} s, {frames} frames: amber {a}, blue {b}, teal {t}, green {g}, \
         red {r}"
    );
    if let Ok(path) = std::env::var("ENGINE_EDITOR_AGRAPH_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &shot,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    assert!(a > 2_000, "the entry's title bar: {a}");
    assert!(b > 4_000, "two states' title bars: {b}");
    assert!(t > 2_000, "the Any State box: {t}");
    assert!(g > 500, "the Entry tag: {g}");
    assert!(r > 100, "the broken condition's arrow and why: {r}");
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
    let [a0, t0, g0] = [tests[0], tests[2], tests[3]].map(|f| count(&scene, f));
    println!("the scene's tab: amber {a0}, teal {t0}, green {g0}");
    assert!(
        a0 < 200 && t0 < 200 && g0 < 200,
        "no canvas behind the scene"
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
