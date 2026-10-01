//! The engine's logging (CLAUDE.md §14, ADR 0061).
//!
//! Code logs through the [`log`] facade (`log::info!`, `log::warn!(target: "vulkan", …)`),
//! so the engine and its dependencies share one pipeline. This crate is the backend:
//!
//! - **Hot path, no locks, no allocation:** a record that passes the filter is formatted
//!   into a buffer on the stack and copied into a ring of fixed entries (4096 × 256 bytes
//!   by default) with one atomic add; the oldest lines are overwritten. In profiling
//!   builds it also goes to Tracy as a colored message.
//! - **Writer thread:** drains the ring to the sinks (stderr, the debugger's output
//!   window, a file). It wakes every 50 ms, and at once for warnings and errors.
//! - **Filters:** per target (module path) and level, from `ENGINE_LOG` or
//!   [`LogConfig::filter`], changeable while running ([`set_filter`]).
//! - **Crash reports:** [`write_recent`] copies the last lines out of the ring without
//!   locks or allocation, and [`report`] holds the sections other subsystems add.
//!
//! ```no_run
//! let _log = engine_log::init(&engine_log::LogConfig::default()).expect("logger");
//! log::info!("adapter: {}", "RTX 4060 Ti");
//! log::warn!(target: "vulkan", "validation: {}", "…");
//! ```

mod atomic;
mod filter;
pub mod report;
mod ring;
mod text;

pub use filter::Filter;
pub use log;
pub use text::{SliceWriter, StackText, UtcTime};

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use log::{Level, Log, Metadata, Record};
use ring::{PART_BYTES, Read, Ring};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Longest record text (target and message) kept; longer messages are cut with `...`.
pub const MAX_RECORD_BYTES: usize = PART_BYTES * ring::MAX_PARTS;
/// How often the writer thread drains the ring when nothing urgent was logged.
const DRAIN_PERIOD: Duration = Duration::from_millis(50);

/// Logger settings.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Filter spec ([`Filter`]); `None` reads `ENGINE_LOG`, falling back to `info`.
    pub filter: Option<String>,
    /// Write lines to stderr.
    pub stderr: bool,
    /// Write lines to the debugger's output window while a debugger is attached.
    pub debugger: bool,
    /// Also write lines to this file (created or truncated).
    pub file: Option<PathBuf>,
    /// Send records to Tracy as messages (profiling builds).
    pub tracy: bool,
    /// Ring entries (256 bytes each; a record takes one per 232 bytes of text).
    pub capacity: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: None,
            stderr: true,
            debugger: true,
            file: None,
            tracy: true,
            capacity: 4096,
        }
    }
}

/// Errors from [`init`] and [`set_filter`].
#[derive(Debug)]
pub enum LogError {
    /// A logger is already installed (by an earlier `init` or another crate).
    AlreadyInstalled,
    /// The filter spec is invalid.
    Filter(String),
    /// The log file could not be created.
    File(std::io::Error),
    /// The writer thread could not be started.
    Thread(std::io::Error),
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInstalled => write!(f, "a logger is already installed"),
            Self::Filter(e) => write!(f, "log filter: {e}"),
            Self::File(e) => write!(f, "log file: {e}"),
            Self::Thread(e) => write!(f, "log writer thread: {e}"),
        }
    }
}

impl std::error::Error for LogError {}

/// Counters since [`init`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogStats {
    /// Ring entries written (a record takes one per 232 bytes).
    pub entries: u64,
    /// Entries that never reached the sinks: overwritten before the writer thread got to
    /// them, or dropped by a writer that caught up with one a lap behind.
    pub lost: u64,
}

struct Shared {
    ring: Ring,
    filter: AtomicPtr<Filter>,
    start: Instant,
    tracy: bool,
    writer: Mutex<Writer>,
    thread: OnceLock<std::thread::Thread>,
    stop: AtomicBool,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

impl Shared {
    #[inline]
    fn filter(&self) -> &Filter {
        // SAFETY: the pointer comes from `Box::into_raw` and is never freed: replaced
        // filters are leaked (a few hundred bytes each time someone edits the filter).
        unsafe { &*self.filter.load(Ordering::Acquire) }
    }

    fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

struct Logger;

static LOGGER: Logger = Logger;

impl Log for Logger {
    #[inline]
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        SHARED
            .get()
            .is_some_and(|s| metadata.level() <= s.filter().level_for(metadata.target()))
    }

    #[inline]
    fn log(&self, record: &Record<'_>) {
        let Some(shared) = SHARED.get() else {
            return;
        };
        let level = record.level();
        let target = record.target();
        if level > shared.filter().level_for(target) {
            return;
        }
        let mut text = StackText::<MAX_RECORD_BYTES>::new();
        let mut target_len = target.len().min(255);
        while !target.is_char_boundary(target_len) {
            target_len -= 1;
        }
        let _ = text.write_str(&target[..target_len]);
        let _ = text.write_fmt(*record.args());
        let time = shared.start.elapsed().as_nanos() as u64;
        shared.ring.push(
            level,
            thread_id(),
            time,
            target_len as u16,
            text.as_str().as_bytes(),
        );
        if engine_profiler::ENABLED && shared.tracy {
            let color = match level {
                Level::Error => 0xFF4040,
                Level::Warn => 0xFFC040,
                Level::Info => 0xE0E0E0,
                Level::Debug => 0x80A0FF,
                Level::Trace => 0x808080,
            };
            engine_profiler::message_color(&text.as_str()[target_len..], color);
        }
        if level <= Level::Warn {
            shared.wake();
        }
    }

    fn flush(&self) {
        flush();
    }
}

/// The OS id of the calling thread (what debuggers and minidumps show).
#[cfg(windows)]
#[inline]
pub fn thread_id() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    // SAFETY: no preconditions.
    unsafe { GetCurrentThreadId() }
}

/// A small per-thread number (non-Windows builds).
#[cfg(not(windows))]
#[inline]
pub fn thread_id() -> u32 {
    use core::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local!(static ID: u32 = NEXT.fetch_add(1, Ordering::Relaxed));
    ID.with(|id| *id)
}

/// Installs the logger and starts its writer thread. Keep the guard until exit: dropping
/// it writes out what is left in the ring and stops the thread.
///
/// Also receives the `log` records of every dependency. Fails if a logger is already
/// installed.
pub fn init(config: &LogConfig) -> Result<LogGuard, LogError> {
    let spec = match &config.filter {
        Some(spec) => spec.clone(),
        None => std::env::var("ENGINE_LOG").unwrap_or_else(|_| String::from("info")),
    };
    let filter = Filter::parse(&spec).map_err(LogError::Filter)?;
    let file = match &config.file {
        Some(path) => {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(LogError::File)?;
            }
            Some(std::io::BufWriter::new(
                std::fs::File::create(path).map_err(LogError::File)?,
            ))
        }
        None => None,
    };
    let max = filter.max();
    let mut created = false;
    let shared = SHARED.get_or_init(|| {
        created = true;
        Shared {
            ring: Ring::new(config.capacity),
            filter: AtomicPtr::new(Box::into_raw(Box::new(filter))),
            start: Instant::now(),
            tracy: config.tracy,
            writer: Mutex::new(Writer {
                cursor: 0,
                stalled: None,
                lost: 0,
                reported_lost: 0,
                record: Vec::with_capacity(MAX_RECORD_BYTES),
                line: String::with_capacity(MAX_RECORD_BYTES + 64),
                wide: Vec::new(),
                sinks: Sinks {
                    stderr: config.stderr,
                    debugger: config.debugger,
                    file,
                },
            }),
            thread: OnceLock::new(),
            stop: AtomicBool::new(false),
        }
    });
    if !created || log::set_logger(&LOGGER).is_err() {
        return Err(LogError::AlreadyInstalled);
    }
    log::set_max_level(max);
    let thread = std::thread::Builder::new()
        .name(String::from("engine-log"))
        .spawn(move || {
            engine_profiler::set_thread_name("engine-log");
            while !shared.stop.load(Ordering::Acquire) {
                std::thread::park_timeout(DRAIN_PERIOD);
                if let Ok(mut writer) = shared.writer.lock() {
                    writer.drain(shared);
                }
            }
        })
        .map_err(LogError::Thread)?;
    let _ = shared.thread.set(thread.thread().clone());
    log::info!(
        target: "engine_log",
        "log started {}; filter `{spec}`",
        UtcTime::now()
    );
    Ok(LogGuard {
        thread: Some(thread),
    })
}

/// Returned by [`init`]; see there.
#[must_use = "dropping the guard stops the log writer"]
#[derive(Debug)]
pub struct LogGuard {
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        if let Some(shared) = SHARED.get() {
            shared.stop.store(true, Ordering::Release);
            shared.wake();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        flush();
    }
}

/// Replaces the filter while running (the editor's per-subsystem filters).
pub fn set_filter(spec: &str) -> Result<(), LogError> {
    let filter = Filter::parse(spec).map_err(LogError::Filter)?;
    let max = filter.max();
    if let Some(shared) = SHARED.get() {
        // The old filter is leaked: another thread may be reading it.
        shared
            .filter
            .store(Box::into_raw(Box::new(filter)), Ordering::Release);
        log::set_max_level(max);
    }
    Ok(())
}

/// Writes out everything logged so far (and flushes the file). Blocks while the writer
/// thread is writing.
pub fn flush() {
    if let Some(shared) = SHARED.get()
        && let Ok(mut writer) = shared.writer.lock()
    {
        writer.drain(shared);
    }
}

/// Counters since [`init`] (zeros without a logger).
pub fn stats() -> LogStats {
    SHARED.get().map_or(LogStats::default(), |s| LogStats {
        entries: s.ring.head(),
        lost: s.ring.lost() + s.writer.try_lock().map_or(0, |w| w.lost),
    })
}

/// Writes the last `max_records` records still in the ring, oldest first, one line each.
/// For crash reports: no locks, no allocation (records are assembled on the stack).
pub fn write_recent(out: &mut dyn fmt::Write, max_records: usize) -> fmt::Result {
    let Some(shared) = SHARED.get() else {
        return writeln!(out, "(no logger installed)");
    };
    let ring = &shared.ring;
    let head = ring.head();
    let start = head.saturating_sub(ring.capacity());
    // The first pass counts the records, the second writes the last `max_records`.
    let mut total = 0usize;
    walk(ring, start, head, |_, _| {
        total += 1;
        Ok(())
    })?;
    let mut skip = total.saturating_sub(max_records);
    walk(ring, start, head, |first, text| {
        if skip > 0 {
            skip -= 1;
            return Ok(());
        }
        write_line(out, first, text)
    })
}

/// Calls `emit` for every record between `start` and `end` whose first part is still
/// there, with that part and the text of its parts up to the first missing one.
fn walk(
    ring: &Ring,
    start: u64,
    end: u64,
    mut emit: impl FnMut(&ring::Part, &[u8]) -> fmt::Result,
) -> fmt::Result {
    let mut text = [0u8; MAX_RECORD_BYTES];
    let mut seq = start;
    while seq < end {
        let first = match ring.read(seq) {
            Read::Part(p) if p.meta.part == 0 => p,
            _ => {
                seq += 1;
                continue;
            }
        };
        let parts = u64::from(first.meta.parts.max(1));
        let mut len = 0;
        for k in 0..parts {
            let part = if k == 0 {
                first
            } else {
                match ring.read(seq + k) {
                    Read::Part(p) if u64::from(p.meta.part) == k => p,
                    _ => break,
                }
            };
            let bytes = part.bytes();
            text[len..len + bytes.len()].copy_from_slice(bytes);
            len += bytes.len();
        }
        emit(&first, &text[..len])?;
        seq += parts;
    }
    Ok(())
}

/// `    12.345678 ERROR engine_rhi::fault [4312] message`
fn write_line(out: &mut dyn fmt::Write, first: &ring::Part, text: &[u8]) -> fmt::Result {
    let secs = first.time / 1_000_000_000;
    let micros = first.time % 1_000_000_000 / 1000;
    let target_len = usize::from(first.meta.target_len).min(text.len());
    let (target, message) = text.split_at(target_len);
    write!(out, "{secs:>6}.{micros:06} {:<5} ", first.meta.level)?;
    text::write_lossy(out, target)?;
    write!(out, " [{}] ", first.meta.thread)?;
    text::write_lossy(out, message)?;
    out.write_char('\n')
}

struct Sinks {
    stderr: bool,
    debugger: bool,
    file: Option<std::io::BufWriter<std::fs::File>>,
}

/// The writer thread's state (also used by [`flush`]).
struct Writer {
    /// Next sequence number to write out.
    cursor: u64,
    /// A sequence number that was not written yet at the last drains, and how many
    /// drains saw it so: after three, its writer is assumed to have dropped it.
    stalled: Option<(u64, u32)>,
    lost: u64,
    reported_lost: u64,
    record: Vec<u8>,
    line: String,
    wide: Vec<u16>,
    sinks: Sinks,
}

impl Writer {
    fn drain(&mut self, shared: &Shared) {
        let ring = &shared.ring;
        let head = ring.head();
        if head - self.cursor > ring.capacity() {
            self.lost += head - self.cursor - ring.capacity();
            self.cursor = head - ring.capacity();
        }
        while self.cursor < head {
            let first = match ring.read(self.cursor) {
                Read::Part(p) if p.meta.part == 0 => p,
                Read::Part(_) | Read::Gone => {
                    self.lost += 1;
                    self.cursor += 1;
                    continue;
                }
                Read::Pending => {
                    if !self.give_up_on(self.cursor) {
                        break;
                    }
                    self.lost += 1;
                    self.cursor += 1;
                    continue;
                }
            };
            let parts = u64::from(first.meta.parts.max(1));
            self.record.clear();
            let mut complete = true;
            for k in 0..parts {
                let part = if k == 0 {
                    Read::Part(first)
                } else {
                    ring.read(self.cursor + k)
                };
                match part {
                    Read::Part(p) if u64::from(p.meta.part) == k => {
                        self.record.extend_from_slice(p.bytes());
                    }
                    Read::Pending if !self.give_up_on(self.cursor + k) => {
                        // The rest of the record is still being written.
                        self.finish();
                        return;
                    }
                    _ => {
                        complete = false;
                        break;
                    }
                }
            }
            if !complete {
                self.lost += 1;
            }
            self.stalled = None;
            self.report_lost();
            self.line.clear();
            let record = core::mem::take(&mut self.record);
            let _ = write_line(&mut self.line, &first, &record);
            self.record = record;
            self.emit();
            self.cursor += parts;
        }
        self.finish();
    }

    /// Whether to stop waiting for sequence number `seq`.
    fn give_up_on(&mut self, seq: u64) -> bool {
        match &mut self.stalled {
            Some((s, drains)) if *s == seq => {
                *drains += 1;
                if *drains >= 3 {
                    self.stalled = None;
                    return true;
                }
                false
            }
            _ => {
                self.stalled = Some((seq, 1));
                false
            }
        }
    }

    fn report_lost(&mut self) {
        if self.lost > self.reported_lost {
            self.line.clear();
            let _ = writeln!(
                self.line,
                "{} log entries lost (overwritten before they were written out)",
                self.lost - self.reported_lost
            );
            self.reported_lost = self.lost;
            self.emit();
        }
    }

    fn emit(&mut self) {
        if self.sinks.stderr {
            let _ = std::io::stderr().lock().write_all(self.line.as_bytes());
        }
        if let Some(file) = &mut self.sinks.file {
            let _ = file.write_all(self.line.as_bytes());
        }
        #[cfg(windows)]
        if self.sinks.debugger && debugger::attached() {
            self.wide.clear();
            self.wide.extend(self.line.encode_utf16());
            self.wide.push(0);
            debugger::output(&self.wide);
        }
    }

    fn finish(&mut self) {
        self.report_lost();
        if let Some(file) = &mut self.sinks.file {
            let _ = file.flush();
        }
    }
}

#[cfg(windows)]
mod debugger {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn IsDebuggerPresent() -> i32;
        fn OutputDebugStringW(text: *const u16);
    }

    pub(super) fn attached() -> bool {
        // SAFETY: no preconditions.
        unsafe { IsDebuggerPresent() != 0 }
    }

    /// `text` must end with a NUL.
    pub(super) fn output(text: &[u16]) {
        if text.last() != Some(&0) {
            return;
        }
        // SAFETY: `text` is NUL-terminated (checked above).
        unsafe { OutputDebugStringW(text.as_ptr()) }
    }
}
