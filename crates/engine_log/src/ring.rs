//! The log ring: a fixed array of entries that any thread appends to without locks or
//! allocation, overwriting the oldest (ADR 0061).
//!
//! A record takes one or more consecutive entries (its text split into 232-byte parts),
//! claimed with one `fetch_add` on `head`. Each entry is a seqlock: its `state` is odd
//! while a writer fills it and `2 × seq + 2` once committed, so readers (the writer
//! thread, crash reports) copy an entry and keep the copy only if `state` did not change
//! meanwhile. The text is stored in atomic words, which keeps concurrent reads and writes
//! free of data races without locks.
//!
//! A writer that finds its entry still being written by a writer one lap behind drops its
//! part instead of waiting (counted in [`Ring::lost`]); readers skip entries that were
//! overwritten before they got to them.

use crate::atomic::{AtomicU64, Ordering, fence};
use log::Level;

/// Text bytes per entry (256-byte entries; small under loom to keep the model small).
#[cfg(not(loom))]
pub(crate) const PART_BYTES: usize = 232;
#[cfg(loom)]
pub(crate) const PART_BYTES: usize = 16;
const PART_WORDS: usize = PART_BYTES / 8;
/// At most this many entries per record (the meta field has 4 bits for the count).
pub(crate) const MAX_PARTS: usize = 15;

/// Keeps the contended `head` on its own cache lines (adjacent-line prefetch pairs them).
#[repr(C, align(128))]
struct Padded<T>(T);

/// One ring entry: 256 bytes.
#[repr(C, align(64))]
struct Entry {
    /// 0: never written; odd: being written; `2 × seq + 2`: holds record part `seq`.
    state: AtomicU64,
    /// Nanoseconds since the logger started.
    time: AtomicU64,
    /// [`Meta`], packed.
    meta: AtomicU64,
    /// Text bytes, little-endian in words.
    text: [AtomicU64; PART_WORDS],
}

impl Entry {
    fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
            time: AtomicU64::new(0),
            meta: AtomicU64::new(0),
            text: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// What an entry says about its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) level: Level,
    /// Index of this part in its record.
    pub(crate) part: u8,
    /// Parts in the record.
    pub(crate) parts: u8,
    /// Text bytes in this entry.
    pub(crate) len: u8,
    /// Bytes of the record's text that are its target (they come first).
    pub(crate) target_len: u16,
    /// The OS id of the thread that logged it.
    pub(crate) thread: u32,
}

impl Meta {
    fn pack(self) -> u64 {
        (self.level as u64)
            | u64::from(self.part) << 4
            | u64::from(self.parts) << 8
            | u64::from(self.len) << 12
            | u64::from(self.target_len & 0xFFF) << 20
            | u64::from(self.thread) << 32
    }

    fn unpack(v: u64) -> Self {
        let level = match v & 0xF {
            1 => Level::Error,
            2 => Level::Warn,
            3 => Level::Info,
            4 => Level::Debug,
            _ => Level::Trace,
        };
        Self {
            level,
            part: ((v >> 4) & 0xF) as u8,
            parts: ((v >> 8) & 0xF) as u8,
            len: ((v >> 12) & 0xFF) as u8,
            target_len: ((v >> 20) & 0xFFF) as u16,
            thread: (v >> 32) as u32,
        }
    }
}

/// A copied entry.
#[derive(Clone, Copy)]
pub(crate) struct Part {
    pub(crate) time: u64,
    pub(crate) meta: Meta,
    pub(crate) text: [u8; PART_BYTES],
}

impl Part {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.text[..usize::from(self.meta.len).min(PART_BYTES)]
    }
}

/// What reading one sequence number found.
// A copied part lives on the stack: boxing it would allocate in crash handlers.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Read {
    /// The part written for this sequence number.
    Part(Part),
    /// Not written yet (or its writer is still at it).
    Pending,
    /// Overwritten by a later record, or dropped by its writer.
    Gone,
}

/// The ring.
pub(crate) struct Ring {
    head: Padded<AtomicU64>,
    lost: AtomicU64,
    entries: Box<[Entry]>,
    mask: u64,
}

impl Ring {
    /// A ring of `capacity` entries (rounded up to a power of two, at least 2).
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        Self {
            head: Padded(AtomicU64::new(0)),
            lost: AtomicU64::new(0),
            entries: (0..capacity).map(|_| Entry::new()).collect(),
            mask: capacity as u64 - 1,
        }
    }

    pub(crate) fn capacity(&self) -> u64 {
        self.mask + 1
    }

    /// Sequence number the next record gets.
    pub(crate) fn head(&self) -> u64 {
        self.head.0.load(Ordering::Acquire)
    }

    /// Parts writers dropped because their entry was still being written a lap earlier.
    pub(crate) fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    /// Appends a record: `text` (target first, `target_len` bytes of it) split into
    /// parts. Text beyond [`MAX_PARTS`] parts is cut. Never blocks, never allocates.
    pub(crate) fn push(&self, level: Level, thread: u32, time: u64, target_len: u16, text: &[u8]) {
        let text = &text[..text.len().min(PART_BYTES * MAX_PARTS)];
        let parts = text.len().div_ceil(PART_BYTES).max(1);
        let first = self.head.0.fetch_add(parts as u64, Ordering::Relaxed);
        for part in 0..parts {
            let seq = first + part as u64;
            let entry = &self.entries[(seq & self.mask) as usize];
            let current = entry.state.load(Ordering::Relaxed);
            // A writer a lap behind is still filling this entry, or a writer a lap ahead
            // got here first: drop the part rather than wait (a reader treats the
            // record as incomplete). Acquire: the text stores below come after those of
            // the part being replaced, so they cannot interleave with them.
            if current & 1 == 1
                || current > seq * 2 + 1
                || entry
                    .state
                    .compare_exchange(current, seq * 2 + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_err()
            {
                self.lost.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Readers that see the new text also see the odd state.
            fence(Ordering::Release);
            let start = part * PART_BYTES;
            let chunk = &text[start..(start + PART_BYTES).min(text.len())];
            entry.time.store(time, Ordering::Relaxed);
            entry.meta.store(
                Meta {
                    level,
                    part: part as u8,
                    parts: parts as u8,
                    len: chunk.len() as u8,
                    target_len,
                    thread,
                }
                .pack(),
                Ordering::Relaxed,
            );
            for (i, word) in entry.text.iter().enumerate() {
                let from = i * 8;
                if from >= chunk.len() {
                    break;
                }
                let mut bytes = [0u8; 8];
                let n = (chunk.len() - from).min(8);
                bytes[..n].copy_from_slice(&chunk[from..from + n]);
                word.store(u64::from_le_bytes(bytes), Ordering::Relaxed);
            }
            entry.state.store(seq * 2 + 2, Ordering::Release);
        }
    }

    /// Reads the part with sequence number `seq`.
    pub(crate) fn read(&self, seq: u64) -> Read {
        let entry = &self.entries[(seq & self.mask) as usize];
        let expected = seq * 2 + 2;
        let before = entry.state.load(Ordering::Acquire);
        if before != expected {
            // An older lap (or nothing) is still there, or our writer is at work.
            return if before <= seq * 2 + 1 {
                Read::Pending
            } else {
                Read::Gone
            };
        }
        let time = entry.time.load(Ordering::Relaxed);
        let meta = entry.meta.load(Ordering::Relaxed);
        let mut text = [0u8; PART_BYTES];
        for (i, word) in entry.text.iter().enumerate() {
            text[i * 8..i * 8 + 8].copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        // The copy is valid only if no writer started on the entry meanwhile.
        fence(Ordering::Acquire);
        if entry.state.load(Ordering::Relaxed) != expected {
            return Read::Gone;
        }
        Read::Part(Part {
            time,
            meta: Meta::unpack(meta),
            text,
        })
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn text_of(ring: &Ring, seq: u64) -> Vec<u8> {
        match ring.read(seq) {
            Read::Part(p) => p.bytes().to_vec(),
            Read::Pending => b"<pending>".to_vec(),
            Read::Gone => b"<gone>".to_vec(),
        }
    }

    #[test]
    fn meta_round_trips() {
        let meta = Meta {
            level: Level::Warn,
            part: 3,
            parts: 9,
            len: 231,
            target_len: 4000,
            thread: 0xDEAD_BEEF,
        };
        assert_eq!(Meta::unpack(meta.pack()), meta);
    }

    #[test]
    fn records_split_into_parts_and_read_back() {
        let ring = Ring::new(16);
        let long: Vec<u8> = (0..600u32).map(|i| b'a' + (i % 26) as u8).collect();
        ring.push(Level::Info, 7, 100, 3, b"tgtshort");
        ring.push(Level::Error, 7, 200, 0, &long);
        assert_eq!(ring.head(), 1 + 3);
        assert_eq!(text_of(&ring, 0), b"tgtshort");
        let mut joined = Vec::new();
        for seq in 1..4 {
            let Read::Part(p) = ring.read(seq) else {
                panic!("part {seq} missing")
            };
            assert_eq!(p.meta.part as u64, seq - 1);
            assert_eq!(p.meta.parts, 3);
            assert_eq!(p.meta.level, Level::Error);
            joined.extend_from_slice(p.bytes());
        }
        assert_eq!(joined, long);
        assert!(matches!(ring.read(4), Read::Pending));
    }

    #[test]
    fn old_entries_are_overwritten() {
        let ring = Ring::new(4);
        for i in 0..10u8 {
            ring.push(Level::Info, 1, u64::from(i), 0, &[b'0' + i]);
        }
        assert!(matches!(ring.read(2), Read::Gone));
        assert_eq!(text_of(&ring, 9), b"9");
        assert_eq!(text_of(&ring, 6), b"6");
        assert_eq!(ring.lost(), 0);
    }

    #[test]
    fn concurrent_writers_never_tear_records() {
        let ring = std::sync::Arc::new(Ring::new(64));
        let threads: Vec<_> = (0..4u8)
            .map(|t| {
                let ring = ring.clone();
                std::thread::spawn(move || {
                    for i in 0..20_000u32 {
                        // Every byte of a record equals the writer's id: a torn copy
                        // would mix them.
                        let len = 1 + (i as usize * 37) % 400;
                        let text = [b'A' + t; 400];
                        ring.push(Level::Debug, u32::from(t), 0, 0, &text[..len]);
                    }
                })
            })
            .collect();
        let mut checked = 0u64;
        while threads.iter().any(|t| !t.is_finished()) {
            let head = ring.head();
            for seq in head.saturating_sub(64)..head {
                if let Read::Part(p) = ring.read(seq) {
                    let first = p.bytes()[0];
                    assert!(p.bytes().iter().all(|&b| b == first), "torn entry");
                    assert_eq!(u32::from(first - b'A'), p.meta.thread);
                    checked += 1;
                }
            }
        }
        for t in threads {
            t.join().unwrap();
        }
        assert!(checked > 0);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    fn check(ring: &Ring, seq: u64) {
        if let Read::Part(p) = ring.read(seq) {
            let bytes = p.bytes();
            assert_eq!(bytes.len(), 9);
            assert!(bytes.iter().all(|&b| b == bytes[0]), "torn entry");
            assert_eq!(u32::from(bytes[0]), p.meta.thread);
        }
    }

    /// A reader copying an entry while a writer a lap ahead overwrites it (two-entry
    /// ring) either gets the old part whole or reports it gone: never a mix.
    #[test]
    fn loom_reader_racing_a_lapping_writer() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let ring = Arc::new(Ring::new(2));
            ring.push(Level::Info, u32::from(b'x'), 0, 0, &[b'x'; 9]);
            let writer = {
                let ring = ring.clone();
                loom::thread::spawn(move || {
                    ring.push(Level::Info, u32::from(b'y'), 0, 0, &[b'y'; 9]);
                    ring.push(Level::Info, u32::from(b'z'), 0, 0, &[b'z'; 9]);
                })
            };
            check(&ring, 0);
            check(&ring, 2);
            writer.join().unwrap();
            check(&ring, 1);
        });
    }

    /// Two writers lapping each other in a two-entry ring: parts are dropped rather than
    /// mixed.
    #[test]
    fn loom_writers_lapping_each_other() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(2);
        model.check(|| {
            let ring = Arc::new(Ring::new(2));
            let writers: Vec<_> = [b'x', b'y']
                .into_iter()
                .map(|byte| {
                    let ring = ring.clone();
                    loom::thread::spawn(move || {
                        ring.push(Level::Info, u32::from(byte), 0, 0, &[byte; 9]);
                        ring.push(Level::Info, u32::from(byte), 0, 0, &[byte; 9]);
                    })
                })
                .collect();
            for w in writers {
                w.join().unwrap();
            }
            for seq in 0..4 {
                check(&ring, seq);
            }
        });
    }
}
