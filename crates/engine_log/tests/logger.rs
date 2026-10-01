//! The installed logger end to end: filters, the file sink, long records, the crash-report
//! view of the ring. One test per file: the logger is process-wide.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_log::{LogConfig, MAX_RECORD_BYTES};

#[test]
fn records_reach_the_file_and_the_crash_view() {
    let dir = std::env::temp_dir().join(format!("engine_log_test_{}", std::process::id()));
    let path = dir.join("test.log");
    let guard = engine_log::init(&LogConfig {
        filter: Some(String::from("info,noisy=error,chatty::inner=trace")),
        stderr: false,
        debugger: false,
        file: Some(path.clone()),
        tracy: false,
        capacity: 64,
    })
    .unwrap();
    assert!(matches!(
        engine_log::init(&LogConfig::default()),
        Err(engine_log::LogError::AlreadyInstalled)
    ));

    log::info!("hello {}", 42);
    log::debug!("hidden by the default level");
    log::warn!(target: "noisy", "hidden: noisy only shows errors");
    log::error!(target: "noisy", "noisy error shown");
    log::trace!(target: "chatty::inner::deep", "trace shown");
    let long: String = (0..1000)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    log::info!(target: "long", "{long}");
    let huge = "x".repeat(MAX_RECORD_BYTES * 2);
    log::info!(target: "huge", "{huge}");
    log::info!(target: "utf8", "{}", "жзи".repeat(200));

    let mut recent = String::new();
    engine_log::write_recent(&mut recent, 100).unwrap();
    println!("{recent}");
    assert!(
        recent.contains("INFO  logger [") && recent.contains("] hello 42\n"),
        "{recent}"
    );
    assert!(recent.contains("ERROR noisy ["), "{recent}");
    assert!(!recent.contains("hidden"), "{recent}");
    assert!(recent.contains("TRACE chatty::inner::deep ["), "{recent}");
    assert!(
        recent.contains(&long),
        "a 1000-byte record spans several entries"
    );
    assert!(
        recent.contains("xxx..."),
        "records longer than the limit are cut"
    );
    assert!(
        recent.contains(&"жзи".repeat(200)),
        "UTF-8 split across entries"
    );

    // Only the last records.
    let mut last = String::new();
    engine_log::write_recent(&mut last, 2).unwrap();
    assert_eq!(last.lines().count(), 2, "{last}");
    assert!(last.contains("huge") && last.contains("utf8"), "{last}");

    // The filter can change while running.
    engine_log::set_filter("warn").unwrap();
    log::info!("after the filter change");
    assert!(engine_log::set_filter("engine=loud").is_err());

    // Overrun the ring (64 entries): the file gets what the writer drained, and the
    // guard drains the rest on drop.
    engine_log::flush();
    engine_log::set_filter("info").unwrap();
    for i in 0..500 {
        log::info!(target: "flood", "line {i}");
    }
    drop(guard);
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("] hello 42\n"), "{file}");
    assert!(file.contains(&long));
    assert!(!file.contains("after the filter change"));
    assert!(
        file.contains("line 499"),
        "the guard drains the ring on drop"
    );
    let stats = engine_log::stats();
    assert!(stats.entries > 500);
    println!("{stats:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
