//! Thin Windows 11 platform layer (CLAUDE.md §0.4, §5).
//!
//! - [`time`]: high-resolution waitable timers and a precise sleeper for frame pacing;
//! - [`cpu`]: CPU set topology (P/E cores, caches) from `GetSystemCpuSetInformation`;
//! - [`thread`]: CPU set placement, priorities, MMCSS, thread descriptions;
//! - [`process`]: process-wide settings such as disabling power throttling.
//!
//! Everything here is Windows-only. On other hosts the crate compiles to nothing, which
//! keeps the workspace buildable in Linux containers. Bindings come from `windows-sys`
//! (ADR 0004).

#[cfg(windows)]
pub mod cpu;
#[cfg(windows)]
pub mod process;
#[cfg(windows)]
pub mod thread;
#[cfg(windows)]
pub mod time;

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}
