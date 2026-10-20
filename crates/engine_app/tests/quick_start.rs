//! The quick start's game (`examples/quick_start.rs` and its scene in
//! `examples/quick_start/`, what `ofeliaengine new` starts a game from) renders its scene:
//! offscreen, the red cube stands on the grey floor in the middle of the picture.
//! `ENGINE_QUICK_START_PNG=<file>` writes the frame.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

#[path = "../examples/quick_start.rs"]
#[allow(dead_code)]
mod quick_start;

use engine_app::{Player, Window};

#[test]
fn the_quick_start_shows_its_cube() {
    let (width, height) = (640u32, 360u32);
    let mut settings = quick_start::settings().unwrap();
    // The game's folder, where `scenes/main.scene` is.
    settings.project =
        Some(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/quick_start"));
    settings.window = Window::Offscreen { width, height };
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
    let mut game = quick_start::start(&mut player).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..30 {
        player.frame(&mut game).unwrap();
    }
    let rgba = player.capture(&mut game).unwrap().unwrap();
    if let Ok(path) = std::env::var("ENGINE_QUICK_START_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let pixel = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    // The cube in the middle is red; the floor below it grey.
    let [r, g, b] = pixel(width / 2, height / 2);
    assert!(r > 1.5 * g && r > 1.5 * b, "middle {r} {g} {b}");
    let [r, g, b] = pixel(width / 2, height - 8);
    assert!(
        (r - g).abs() < 30.0 && (g - b).abs() < 30.0 && r > 20.0,
        "floor {r} {g} {b}"
    );
}
