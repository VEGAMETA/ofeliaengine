//! Crash reports end to end: child processes of this test crash (an access violation) or
//! panic with crash reporting installed; the monitor must leave a minidump and a text
//! report with the log lines and report sections. One test per file: installation is
//! process-wide.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TEST: &str = "crashes_and_panics_leave_reports";
const CHILD_ENV: &str = "ENGINE_CRASH_TEST_CHILD";
const DIR_ENV: &str = "ENGINE_CRASH_TEST_DIR";

fn harness_args() -> Vec<String> {
    ["--exact", TEST, "--nocapture", "--test-threads=1"]
        .map(String::from)
        .to_vec()
}

/// Runs in the child: installs everything, logs, then fails as `mode` says.
fn child(mode: &str) {
    let dir = PathBuf::from(std::env::var_os(DIR_ENV).unwrap());
    let _log = engine_log::init(&engine_log::LogConfig {
        stderr: false,
        ..engine_log::LogConfig::default()
    })
    .unwrap();
    let _section = engine_log::report::add_section("test section", |out| {
        writeln!(out, "context from a subsystem")
    });
    let mut config = engine_crash::CrashConfig::new("crashtest");
    config.dir = Some(dir);
    config.monitor_args = harness_args();
    let installing = Instant::now();
    let _crash = engine_crash::install(&config).unwrap();
    log::info!(
        "install took {:.1} ms",
        installing.elapsed().as_secs_f64() * 1e3
    );
    log::info!("about to fail with {mode}");
    match mode {
        "access-violation" => {
            // An aligned, non-null address nothing maps.
            let target = std::hint::black_box(0x10usize) as *mut u32;
            // SAFETY: none: the point is to crash.
            unsafe { target.write_volatile(1) };
        }
        _ => panic!("deliberate test panic"),
    }
}

fn files(dir: &Path, extension: &str) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == extension))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn crashes_and_panics_leave_reports() {
    if engine_crash::run_monitor_if_requested() {
        return;
    }
    if let Ok(mode) = std::env::var(CHILD_ENV) {
        child(&mode);
        return;
    }
    if !cfg!(windows) {
        return;
    }
    for (mode, expected) in [
        (
            "access-violation",
            "exception 0xc0000005 (access violation)",
        ),
        ("panic", "deliberate test panic"),
    ] {
        let dir =
            std::env::temp_dir().join(format!("engine_crash_test_{}_{mode}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let start = Instant::now();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(harness_args())
            .env(CHILD_ENV, mode)
            .env(DIR_ENV, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        println!(
            "{mode}: child exited with {status} after {:?}",
            start.elapsed()
        );
        assert!(!status.success());
        // The monitor writes the text report after the dump; give it a moment.
        let deadline = Instant::now() + Duration::from_secs(10);
        while files(&dir, "txt").is_empty() && Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(20));
        }
        let dumps = files(&dir, "dmp");
        let reports = files(&dir, "txt");
        assert_eq!(dumps.len(), 1, "{mode}: {dumps:?}");
        assert_eq!(reports.len(), 1, "{mode}: {reports:?}");
        let dump = std::fs::read(&dumps[0]).unwrap();
        assert_eq!(&dump[..4], b"MDMP", "{mode}: not a minidump");
        let report = std::fs::read_to_string(&reports[0]).unwrap();
        println!("{mode}: {} byte dump, report:\n{report}", dump.len());
        assert!(report.contains(expected), "{mode}");
        assert!(
            report.contains("== test section ==\ncontext from a subsystem"),
            "{mode}"
        );
        assert!(
            report.contains(&format!("about to fail with {mode}")),
            "{mode}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
