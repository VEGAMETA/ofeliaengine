//! Crash reports (CLAUDE.md §14, ADR 0061): a minidump and a text report for every crash
//! and panic, written by a separate monitor process.
//!
//! [`install`] starts the monitor (this same executable, told by an environment variable
//! to run [`run_monitor_if_requested`] instead of its `main`), connects to it and hooks
//! the process's crash paths through Embark's `crash-handler`: unhandled exceptions
//! (access violations, stack overflows, illegal instructions…), `abort`, pure virtual
//! calls, invalid CRT parameters, heap corruption. A panic goes the same way from the
//! panic hook. On a crash the process sends the monitor its context and asks it for a
//! dump; the monitor (`minidumper`) writes `<app>-<time>-<pid>.dmp` with
//! `MiniDumpWriteDump` from outside, where a corrupted heap or a broken stack cannot stop
//! it, and `<app>-<time>-<pid>.txt` with:
//!
//! - what happened (exception code or panic message and backtrace, thread);
//! - the report sections other subsystems registered (`engine_log::report`; the GPU
//!   breadcrumbs of every device);
//! - the last log lines (`engine_log::write_recent`).
//!
//! The context is formatted on the crashing thread into a buffer allocated at install
//! time, without the locks the crashed code may hold.
//!
//! ```no_run
//! // First thing in `main`:
//! if engine_crash::run_monitor_if_requested() {
//!     return; // this process was the crash monitor
//! }
//! let _crash = engine_crash::install(&engine_crash::CrashConfig::new("mygame"));
//! // …
//! ```
//!
//! Nothing is installed unless a program asks: tests stay without it unless they opt in.

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, Ordering};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Environment variable that turns a process into the crash monitor: the socket path.
const MONITOR_ENV: &str = "ENGINE_CRASH_MONITOR";
/// The report directory, for the monitor.
const DIR_ENV: &str = "ENGINE_CRASH_DIR";
/// The application name, for the monitor.
const APP_ENV: &str = "ENGINE_CRASH_APP";
/// The monitored process, for the monitor.
const PID_ENV: &str = "ENGINE_CRASH_PID";
/// Message kind carrying the crash context text.
const CONTEXT_MESSAGE: u32 = 1;
/// Exception code of the simulated exception for a panic: an application-defined code
/// (`0xE` = error severity with the customer bit), "PAN" in ASCII.
pub const PANIC_CODE: i32 = 0xE050_414E_u32 as i32;
/// Exception code of a report written by [`CrashGuard::report_now`].
pub const REQUESTED_CODE: i32 = 0xE052_4551_u32 as i32;
/// `STATUS_FATAL_APP_EXIT`: what `abort()` raises.
const ABORT_CODE: i32 = 0x4000_0015;
/// Bytes preallocated for the crash context.
const CONTEXT_BYTES: usize = 512 * 1024;
/// How long connecting to the monitor may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Crash reporting settings.
#[derive(Debug, Clone)]
pub struct CrashConfig {
    /// Name used in report file names (`<app>-<time>-<pid>.dmp`).
    pub app: String,
    /// Where reports go; `None`: `engine_log::report::dir()`.
    pub dir: Option<PathBuf>,
    /// Log lines included in a report.
    pub log_lines: usize,
    /// Arguments the monitor process is started with (it is this executable). Programs
    /// that call [`run_monitor_if_requested`] first thing in `main` need none; a test
    /// harness passes the arguments that run the test that calls it.
    pub monitor_args: Vec<String>,
}

impl CrashConfig {
    /// Defaults for `app`: the default directory, 300 log lines.
    pub fn new(app: &str) -> Self {
        Self {
            app: app.to_owned(),
            dir: None,
            log_lines: 300,
            monitor_args: Vec::new(),
        }
    }
}

/// Why [`install`] failed.
#[derive(Debug)]
pub enum CrashError {
    /// The report directory could not be created.
    Dir(std::io::Error),
    /// The monitor process could not be started.
    Spawn(std::io::Error),
    /// The crash handler could not be attached (another one is attached).
    Attach(String),
    /// Crash reporting is already installed in this process.
    AlreadyInstalled,
    /// Not supported on this platform.
    Unsupported,
}

impl fmt::Display for CrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dir(e) => write!(f, "crash report directory: {e}"),
            Self::Spawn(e) => write!(f, "cannot start the crash monitor: {e}"),
            Self::Attach(e) => write!(f, "cannot attach the crash handler: {e}"),
            Self::AlreadyInstalled => write!(f, "crash reporting is already installed"),
            Self::Unsupported => write!(f, "crash reporting is Windows-only"),
        }
    }
}

impl std::error::Error for CrashError {}

/// State shared by the crash callback and the panic hook.
struct Reporter {
    /// Set by the connecting thread once the monitor accepted the connection.
    client: OnceLock<minidumper::Client>,
    context: Mutex<Box<[u8]>>,
    log_lines: usize,
    /// A panic produced a report; the `abort` that may follow (`panic = "abort"`) needs
    /// no second one.
    panicked: AtomicBool,
}

/// The panic message and backtrace, for the crash callback.
static PANIC_TEXT: Mutex<String> = Mutex::new(String::new());
static HANDLER: Mutex<Option<crash_handler::CrashHandler>> = Mutex::new(None);
static INSTALLED: OnceLock<()> = OnceLock::new();

impl Reporter {
    /// Formats the context into the preallocated buffer and sends it, then asks for the
    /// dump. Runs on the crashing thread.
    fn report(&self, crash: &crash_handler::CrashContext) -> crash_handler::CrashEventResult {
        if crash.exception_code == ABORT_CODE && self.panicked.load(Ordering::Acquire) {
            return crash_handler::CrashEventResult::Handled(false);
        }
        // A crash in the first milliseconds, before the monitor answered: wait for it.
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let client = loop {
            match self.client.get() {
                Some(client) => break client,
                None if Instant::now() < deadline => {
                    std::thread::park_timeout(Duration::from_millis(1));
                }
                None => return crash_handler::CrashEventResult::Handled(false),
            }
        };
        if let Ok(mut buffer) = self.context.try_lock() {
            let mut out = engine_log::SliceWriter::new(&mut buffer);
            let _ = write_context(&mut out, crash, self.log_lines);
            let _ = client.send_message(CONTEXT_MESSAGE, out.written());
        }
        let dumped = client.request_dump(crash).is_ok();
        crash_handler::CrashEventResult::Handled(dumped)
    }
}

fn write_context(
    out: &mut dyn fmt::Write,
    crash: &crash_handler::CrashContext,
    log_lines: usize,
) -> fmt::Result {
    let code = crash.exception_code;
    match code {
        PANIC_CODE => {
            writeln!(out, "panic on thread {}", crash.thread_id)?;
            match PANIC_TEXT.try_lock() {
                Ok(text) => writeln!(out, "{}", text.as_str())?,
                Err(_) => writeln!(out, "(panic message unavailable)")?,
            }
        }
        REQUESTED_CODE => writeln!(out, "report requested on thread {}", crash.thread_id)?,
        _ => writeln!(
            out,
            "exception {:#010x} ({}) on thread {}",
            code as u32,
            exception_name(code),
            crash.thread_id
        )?,
    }
    writeln!(out)?;
    engine_log::report::write_sections(out)?;
    writeln!(out, "== last log lines ==")?;
    engine_log::write_recent(out, log_lines)
}

fn exception_name(code: i32) -> &'static str {
    match code as u32 {
        0xC000_0005 => "access violation",
        0xC000_00FD => "stack overflow",
        0xC000_001D => "illegal instruction",
        0xC000_0094 => "integer division by zero",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "stack buffer overrun or fail-fast",
        0xC000_000D => "invalid parameter",
        0xC000_0025 => "non-continuable exception (pure virtual call)",
        0x8000_0003 => "breakpoint",
        0x4000_0015 => "abort",
        _ => "unknown",
    }
}

/// Keeps crash reporting installed; dropping it detaches the handler and lets the
/// monitor exit.
#[must_use = "dropping the guard uninstalls crash reporting"]
pub struct CrashGuard {
    monitor: std::process::Child,
    dir: PathBuf,
}

impl fmt::Debug for CrashGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrashGuard")
            .field("monitor", &self.monitor.id())
            .field("dir", &self.dir)
            .finish()
    }
}

impl CrashGuard {
    /// Where reports are written.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes a report (a minidump of this process and the context) without crashing,
    /// for states worth a dump that the program survives. Returns whether the monitor
    /// wrote it.
    pub fn report_now(&self) -> bool {
        HANDLER.lock().is_ok_and(|handler| {
            handler.as_ref().is_some_and(|h| {
                matches!(
                    h.simulate_exception(Some(REQUESTED_CODE)),
                    crash_handler::CrashEventResult::Handled(true)
                )
            })
        })
    }
}

impl Drop for CrashGuard {
    fn drop(&mut self) {
        if let Ok(mut handler) = HANDLER.lock()
            && let Some(handler) = handler.take()
        {
            handler.detach();
        }
        // The monitor exits once this process has; do not wait for it.
        let _ = self.monitor.try_wait();
    }
}

/// Starts the monitor process and installs the crash handler and the panic hook.
///
/// Call it early in `main`, after the logger. Once per process.
pub fn install(config: &CrashConfig) -> Result<CrashGuard, CrashError> {
    if !cfg!(windows) {
        return Err(CrashError::Unsupported);
    }
    if INSTALLED.set(()).is_err() {
        return Err(CrashError::AlreadyInstalled);
    }
    let dir = config.dir.clone().unwrap_or_else(engine_log::report::dir);
    std::fs::create_dir_all(&dir).map_err(CrashError::Dir)?;
    let socket =
        std::env::temp_dir().join(format!("ofeliaengine-crash-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let exe = std::env::current_exe().map_err(CrashError::Spawn)?;
    let mut command = std::process::Command::new(exe);
    command
        .args(&config.monitor_args)
        .env(MONITOR_ENV, &socket)
        .env(DIR_ENV, &dir)
        .env(APP_ENV, &config.app)
        .env(PID_ENV, std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let monitor = command.spawn().map_err(CrashError::Spawn)?;

    // Connecting waits for the monitor process to start (10-20 ms): done on a thread, so
    // start-up does not wait. A crash before it connected waits in the handler.
    let reporter = Arc::new(Reporter {
        client: OnceLock::new(),
        context: Mutex::new(vec![0u8; CONTEXT_BYTES].into_boxed_slice()),
        log_lines: config.log_lines,
        panicked: AtomicBool::new(false),
    });
    let connecting = reporter.clone();
    let socket_text = socket.to_string_lossy().into_owned();
    let monitor_id = monitor.id();
    std::thread::Builder::new()
        .name(String::from("engine-crash-connect"))
        .spawn(move || {
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            loop {
                match minidumper::Client::with_name(minidumper::SocketName::path(&socket_text)) {
                    Ok(client) => {
                        let _ = connecting.client.set(client);
                        return;
                    }
                    Err(e) if Instant::now() > deadline => {
                        log::warn!(
                            target: "engine_crash",
                            "no crash reports: the monitor (process {monitor_id}) did not accept a connection: {e}"
                        );
                        return;
                    }
                    // Start-up only: a timed wait, not frame pacing.
                    Err(_) => std::thread::park_timeout(Duration::from_millis(1)),
                }
            }
        })
        .map_err(CrashError::Spawn)?;
    let in_handler = reporter.clone();
    // SAFETY: the callback only formats into memory allocated here, reads lock-free
    // state (the log ring; report sections and the panic text through `try_lock`) and
    // talks to the monitor over the socket connected here.
    let event = unsafe {
        crash_handler::make_crash_event(move |crash: &crash_handler::CrashContext| {
            in_handler.report(crash)
        })
    };
    let handler = match crash_handler::CrashHandler::attach(event) {
        Ok(handler) => handler,
        Err(e) => return Err(CrashError::Attach(e.to_string())),
    };
    if let Ok(mut slot) = HANDLER.lock() {
        *slot = Some(handler);
    }
    // Reserve the panic text now rather than while panicking.
    if let Ok(mut text) = PANIC_TEXT.lock() {
        text.reserve(16 * 1024);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        log::error!(target: "panic", "thread '{name}' {info}");
        if let Ok(mut text) = PANIC_TEXT.lock() {
            text.clear();
            let backtrace = std::backtrace::Backtrace::force_capture();
            let _ = write!(text, "thread '{name}' {info}\n\nbacktrace:\n{backtrace}");
        }
        reporter.panicked.store(true, Ordering::Release);
        if let Ok(handler) = HANDLER.lock()
            && let Some(handler) = handler.as_ref()
        {
            handler.simulate_exception(Some(PANIC_CODE));
        }
        log::logger().flush();
        previous(info);
    }));
    log::info!(
        target: "engine_crash",
        "crash reports go to {} (monitor process {})",
        dir.display(),
        monitor.id()
    );
    Ok(CrashGuard { monitor, dir })
}

/// If this process was started as a crash monitor by [`install`], runs the monitor until
/// the monitored process exits, then returns `true`: `main` should return right away.
/// Otherwise returns `false` at once. Call it first thing in `main`.
pub fn run_monitor_if_requested() -> bool {
    let Some(socket) = std::env::var_os(MONITOR_ENV) else {
        return false;
    };
    let dir = std::env::var_os(DIR_ENV).map_or_else(engine_log::report::dir, PathBuf::from);
    let app = std::env::var(APP_ENV).unwrap_or_else(|_| String::from("app"));
    let pid: u32 = std::env::var(PID_ENV)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let socket = socket.to_string_lossy().into_owned();
    let Ok(mut server) = minidumper::Server::with_name(minidumper::SocketName::path(&socket))
    else {
        return true;
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    // Stop when the monitored process is gone, even if it never connected. A crashed
    // process ends only after the monitor answered its dump request.
    let watch = shutdown.clone();
    std::thread::spawn(move || {
        wait_for_process(pid);
        watch.store(true, Ordering::Release);
    });
    let handler = Monitor {
        dir,
        app,
        pid,
        context: Mutex::new(Vec::new()),
        stem: Mutex::new(None),
    };
    let _ = server.run(Box::new(handler), &shutdown, None);
    let _ = std::fs::remove_file(&socket);
    true
}

/// The monitor's side: files named after the monitored process, the context kept until
/// the dump it belongs to is written.
struct Monitor {
    dir: PathBuf,
    app: String,
    pid: u32,
    context: Mutex<Vec<u8>>,
    stem: Mutex<Option<PathBuf>>,
}

impl minidumper::ServerHandler for Monitor {
    fn create_minidump_file(&self) -> Result<(std::fs::File, PathBuf), std::io::Error> {
        std::fs::create_dir_all(&self.dir)?;
        let time = engine_log::UtcTime::now().compact();
        let base = format!("{}-{}-{}", self.app, time.as_str(), self.pid);
        let mut stem = self.dir.join(&base);
        let mut n = 1;
        while stem.with_extension("dmp").exists() {
            n += 1;
            stem = self.dir.join(format!("{base}-{n}"));
        }
        let path = stem.with_extension("dmp");
        let file = std::fs::File::create(&path)?;
        if let Ok(mut s) = self.stem.lock() {
            *s = Some(stem);
        }
        Ok((file, path))
    }

    fn on_minidump_created(
        &self,
        result: Result<minidumper::MinidumpBinary, minidumper::Error>,
    ) -> minidumper::LoopAction {
        let stem = self.stem.lock().ok().and_then(|mut s| s.take());
        let context = self
            .context
            .lock()
            .map(|mut c| core::mem::take(&mut *c))
            .unwrap_or_default();
        if let Some(stem) = stem {
            let mut text = String::new();
            let _ = writeln!(
                text,
                "ofeliaengine crash report\napp: {}\nprocess: {}\ntime: {}",
                self.app,
                self.pid,
                engine_log::UtcTime::now()
            );
            match result {
                Ok(mut dump) => {
                    use std::io::Write as _;
                    let _ = dump.file.flush();
                    let _ = writeln!(text, "minidump: {}", dump.path.display());
                }
                Err(e) => {
                    let _ = writeln!(text, "minidump failed: {e}");
                }
            }
            let _ = writeln!(text);
            text.push_str(&String::from_utf8_lossy(&context));
            let _ = std::fs::write(stem.with_extension("txt"), text);
        }
        minidumper::LoopAction::Continue
    }

    fn on_message(&self, kind: u32, buffer: Vec<u8>) {
        if kind == CONTEXT_MESSAGE
            && let Ok(mut context) = self.context.lock()
        {
            *context = buffer;
        }
    }

    fn on_client_disconnected(&self, clients: usize) -> minidumper::LoopAction {
        if clients == 0 {
            minidumper::LoopAction::Exit
        } else {
            minidumper::LoopAction::Continue
        }
    }
}

/// Blocks until process `pid` exits (returns at once if it cannot be opened).
#[cfg(windows)]
fn wait_for_process(pid: u32) {
    type Handle = *mut core::ffi::c_void;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const INFINITE: u32 = u32::MAX;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    // SAFETY: plain Win32 calls; the handle is closed once, after the wait.
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return;
        }
        WaitForSingleObject(handle, INFINITE);
        CloseHandle(handle);
    }
}

#[cfg(not(windows))]
fn wait_for_process(_pid: u32) {}
