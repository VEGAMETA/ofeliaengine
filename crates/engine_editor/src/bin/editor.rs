//! The editor without a game: the shell over the player (ADR 0107), on a scene file or an
//! empty world. A game runs the editor in its own build instead (`cargo run --features
//! editor`), with its components and systems native.
//!
//! ```text
//! cargo run -p engine_editor --release --bin editor -- [scene] [--project DIR] [--frames N]
//! ```
//!
//! - `scene`: a scene file relative to the project's folder (`--project`, the working
//!   folder by default).
//! - `--frames N` renders N frames and exits (smoke tests), printing the cold start and
//!   the time per frame.

#[cfg(not(windows))]
fn main() {
    #![allow(clippy::print_stdout)]
    println!("The editor runs on Windows only.");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    use engine_app::{PlayerSettings, Window};
    use std::time::Instant;

    // The crash monitor is this executable started by `engine_crash::install`.
    if engine_crash::run_monitor_if_requested() {
        return Ok(());
    }
    let started = Instant::now();
    // Log to stderr and to %LOCALAPPDATA%/ofeliaengine/logs/editor.log; crashes, panics
    // and GPU faults leave reports in ../reports (ADR 0061).
    let _log = engine_log::init(&engine_log::LogConfig {
        file: Some(engine_log::report::log_file("editor")),
        ..engine_log::LogConfig::default()
    });
    let _crash = engine_crash::install(&engine_crash::CrashConfig::new("editor"))
        .inspect_err(|e| log::warn!("no crash reports: {e}"));

    let mut frames = None;
    let mut scene = None;
    let mut project = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => frames = args.next().and_then(|n| n.parse::<u64>().ok()),
            "--project" => project = args.next().map(std::path::PathBuf::from),
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`").into());
            }
            path => scene = Some(std::path::PathBuf::from(path)),
        }
    }
    let settings = PlayerSettings {
        title: String::from("ofeliaengine editor"),
        window: Window::Windowed {
            width: 1600,
            height: 900,
        },
        scene,
        project,
        ..PlayerSettings::default()
    };
    let result = engine_editor::run_frames(settings, |_| Ok(NoGame), frames);
    match &result {
        Ok(ran) if frames.is_some() => {
            let total = started.elapsed().as_secs_f64();
            println!("{ran} frames, {total:.2} s with the start");
        }
        Ok(_) => {}
        Err(e) => log::error!("{e}"),
    }
    result.map(|_| ())
}

/// No game: the editor on its own.
#[cfg(windows)]
struct NoGame;

#[cfg(windows)]
impl engine_app::Game for NoGame {
    fn tick(&mut self, _world: &mut engine_ecs::World, _dt: f32) {}
}
