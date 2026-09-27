//! Shows what the primary display reports (HDR, SDR white level, luminance range, refresh
//! rate), and turns HDR on or off as Win+Alt+B does, for testing HDR output:
//!
//! `cargo run -p engine_platform --example hdr --release [on|off|toggle]`
//!
//! Turning HDR on or off changes the Windows setting for every application; the display
//! goes dark for a moment while it switches.

#![allow(missing_docs, clippy::print_stdout)]

#[cfg(not(windows))]
fn main() {
    println!("This example runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_platform::display::DisplayQuery;
    use engine_platform::time::HighResTimer;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    // SAFETY: a plain query.
    let desktop = unsafe { GetDesktopWindow() } as isize;
    let mut query = DisplayQuery::new();
    let Some(info) = query.window(desktop) else {
        println!("no display to describe");
        std::process::exit(1);
    };
    println!("{info:#?}");
    let wanted = match std::env::args().nth(1).as_deref() {
        Some("on") => true,
        Some("off") => false,
        Some("toggle") => !info.hdr_enabled,
        _ => return,
    };
    if wanted == info.hdr_enabled {
        println!("HDR is already {}", if wanted { "on" } else { "off" });
        return;
    }
    if !info.hdr_supported && wanted {
        println!("the display does not support HDR");
        std::process::exit(1);
    }
    let start = Instant::now();
    if !query.set_hdr(info.monitor, wanted) {
        println!("Windows refused to change HDR");
        std::process::exit(1);
    }
    let timer = HighResTimer::new().ok();
    loop {
        if let Some(now) = query.window(desktop)
            && now.hdr_enabled == wanted
        {
            println!(
                "HDR {} after {:?}",
                if wanted { "on" } else { "off" },
                start.elapsed()
            );
            println!("{now:#?}");
            return;
        }
        if start.elapsed() > Duration::from_secs(10) {
            println!("the display did not switch within 10 s");
            std::process::exit(1);
        }
        if let Some(timer) = &timer {
            let _ = timer.sleep(Duration::from_millis(20));
        }
    }
}
