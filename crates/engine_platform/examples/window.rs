//! Opens a window and prints window events and raw input until the window is closed.
//!
//! `cargo run -p engine_platform --example window --release`
//!
//! Move the mouse, press keys and resize the window: the main loop below keeps ticking at
//! 240 Hz even while the window is being dragged, because windows live on their own thread.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

#[cfg(not(windows))]
fn main() {
    println!("This example runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_platform::cpu::CpuTopology;
    use engine_platform::raw_input::{RawInput, RawInputConfig, RawInputKind};
    use engine_platform::time::PreciseSleeper;
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use std::time::{Duration, Instant};

    engine_platform::process::disable_power_throttling().unwrap();
    println!("CPU: {}", CpuTopology::query().unwrap().summary());

    let mut windows = WindowSystem::start(4096).unwrap();
    let window = windows
        .create_window(&WindowDesc {
            title: String::from("engine_platform example (close to exit)"),
            ..WindowDesc::default()
        })
        .unwrap();
    let mut input = RawInput::start(RawInputConfig::default()).unwrap();
    let mut sleeper = PreciseSleeper::new().unwrap();

    let frame = Duration::from_micros(4_167);
    let mut deadline = Instant::now();
    let mut ticks = 0u64;
    let mut motion = (0i64, 0i64);
    let mut input_lag = Duration::ZERO;
    let mut input_events = 0u64;
    let mut last_report = Instant::now();
    'main: loop {
        let now = Instant::now();
        let mut close = false;
        windows.drain_events(|event| match event.kind {
            WindowEventKind::CloseRequested => close = true,
            WindowEventKind::CursorMoved { .. } => {}
            kind => println!("window {:?}: {kind:?}", event.window),
        });
        if close {
            windows.destroy_window(window.id).unwrap();
            break 'main;
        }
        input.drain(|event| {
            input_events += 1;
            input_lag = input_lag.max(now.saturating_duration_since(event.time));
            match event.kind {
                RawInputKind::MouseMove { dx, dy, .. } => {
                    motion.0 += i64::from(dx);
                    motion.1 += i64::from(dy);
                }
                kind => println!("raw: {kind:?}"),
            }
        });
        ticks += 1;
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!(
                "{ticks} ticks/s, {input_events} raw events, mouse moved ({}, {}), worst input age at pickup {:?}, sleeper margin {:?}",
                motion.0,
                motion.1,
                input_lag,
                sleeper.margin()
            );
            ticks = 0;
            input_events = 0;
            motion = (0, 0);
            input_lag = Duration::ZERO;
            last_report = Instant::now();
        }
        deadline += frame;
        sleeper.sleep_until(deadline).unwrap();
    }
    println!(
        "dropped: {} window events, {} raw events",
        windows.dropped_events(),
        input.dropped_events()
    );
}
