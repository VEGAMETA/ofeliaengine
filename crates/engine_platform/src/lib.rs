//! Thin Windows 11 platform layer (CLAUDE.md §0.4, §5).
//!
//! - `time`: high-resolution waitable timers and a precise sleeper for frame pacing;
//! - `cpu`: CPU set topology (P/E cores, caches) from `GetSystemCpuSetInformation`;
//! - `thread`: CPU set placement, priorities, MMCSS, thread descriptions;
//! - `process`: process-wide settings such as disabling power throttling;
//! - `window`: windows on a dedicated thread, events through a lock-free ring;
//! - `display`: HDR state, SDR white level, luminance range and refresh rate of a monitor;
//! - `raw_input`: raw mouse/keyboard on a time-critical thread, with timestamps;
//! - `fs_watch`: directory change notifications for hot reload.
//!
//! Everything here is Windows-only. On other hosts the crate compiles to nothing, which
//! keeps the workspace buildable in Linux containers. Bindings come from `windows-sys`
//! (ADR 0004).

#[cfg(windows)]
pub mod cpu;
#[cfg(windows)]
pub mod display;
#[cfg(any(windows, test))]
pub mod fs_watch;
#[cfg(windows)]
pub mod process;
#[cfg(windows)]
pub mod raw_input;
#[cfg(windows)]
pub mod thread;
#[cfg(windows)]
pub mod time;
#[cfg(windows)]
pub mod window;

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}
