//! A reader in the program (the editor's Output panel): the records logged since its last
//! read, with their levels and targets; those the ring overwrote first counted as lost.
//! One test per file: the logger is process-wide.

#![allow(clippy::unwrap_used)]

use engine_log::{LogConfig, LogReader};

#[test]
fn a_reader_takes_what_was_logged_since_its_last_read() {
    let _guard = engine_log::init(&LogConfig {
        filter: Some(String::from("info")),
        stderr: false,
        debugger: false,
        file: None,
        tracy: false,
        capacity: 64,
    })
    .unwrap();
    log::info!("before the reader");
    let mut reader = LogReader::new();
    log::info!("first");
    log::warn!(target: "assets", "props/lamp.scene: no such part");
    log::info!(target: "long", "{}", "x".repeat(1000));
    let mut seen = Vec::new();
    let lost = reader.read(|r| seen.push((r.level, r.target.to_owned(), r.message.to_owned())));
    assert_eq!(lost, 0);
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(seen[0].0, log::Level::Info);
    assert_eq!(seen[0].2, "first");
    assert_eq!(
        seen[1],
        (
            log::Level::Warn,
            String::from("assets"),
            String::from("props/lamp.scene: no such part")
        )
    );
    assert_eq!(seen[2].2.len(), 1000, "a record of several entries whole");
    // Nothing new: nothing read.
    assert_eq!(reader.read(|_| panic!("nothing was logged")), 0);
    // More than the ring holds before the next read: the oldest lost, the newest read.
    for i in 0..100 {
        log::info!("line {i}");
    }
    let mut last = String::new();
    let mut count = 0;
    let lost = reader.read(|r| {
        count += 1;
        last = r.message.to_owned();
    });
    assert!(lost > 0, "the ring of 64 entries overwrote some");
    assert_eq!(last, "line 99");
    assert!(count > 0 && count <= 64, "{count}");
    // From the oldest still in the ring.
    let mut oldest = LogReader::from_oldest();
    let mut again = 0;
    oldest.read(|_| again += 1);
    assert!(again >= count, "{again} >= {count}");
}
