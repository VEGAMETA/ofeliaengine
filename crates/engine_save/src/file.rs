//! Save files (ADR 0104): a header the load menu reads, the body compressed, a checksum;
//! written to a temporary file and swapped in, the previous save kept as `.bak`.
//!
//! ```text
//! "OFSAVE\0\x01"  magic
//! u32             container version (1)
//! u8              encoding: 0 binary, 1 text
//! u8, u16         flags, reserved
//! u32, …          header length, header: slot, engine and game versions, level, date,
//!                 play time, thumbnail (PNG)
//! u64, u64, …     the body's length, its stored (zstd) length, the stored body
//! u64             checksum of everything before it
//! ```

use crate::SaveError;
use crate::body::{LoadedSave, read_binary, write_binary};
use crate::capture::SaveImage;
use crate::wire::{Reader, write_str, write_varint};
use engine_core::hash::StateHasher;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

const MAGIC: &[u8; 8] = b"OFSAVE\0\x01";
const CONTAINER_VERSION: u32 = 1;
/// The largest body a file may claim: a damaged length must not allocate without end.
const MAX_BODY: u64 = 1 << 30;
/// The zstd level: fast enough to compress behind a frame (ADR 0104: 2 ms per 10,000
/// entities), within 10 % of the slow levels' size.
const LEVEL: i32 = 1;

/// The encoding a save is written in. Reading takes both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// A binary of named fields: what release builds write.
    Binary,
    /// The text form: what development builds write, to read and fix by hand.
    Text,
}

impl Encoding {
    /// The build's default (ADR 0104): text with debug assertions, binary without.
    pub const fn for_build() -> Self {
        if cfg!(debug_assertions) {
            Self::Text
        } else {
            Self::Binary
        }
    }
}

/// What the load menu shows of a save, read without its body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveHeader {
    /// The slot's name, as the game gave it.
    pub slot: String,
    /// The engine that wrote it (filled when written).
    pub engine_version: String,
    /// The game's version, as the game gives it.
    pub game_version: String,
    /// The level it was taken in.
    pub level: String,
    /// When, in milliseconds since 1970 (filled when written if 0).
    pub created_unix_ms: u64,
    /// Time played, in milliseconds.
    pub play_ms: u64,
    /// A picture for the load menu (PNG), or empty.
    pub thumbnail: Vec<u8>,
}

impl SaveHeader {
    fn write(&self, out: &mut Vec<u8>) {
        write_str(out, &self.slot);
        write_str(out, &self.engine_version);
        write_str(out, &self.game_version);
        write_str(out, &self.level);
        write_varint(out, self.created_unix_ms);
        write_varint(out, self.play_ms);
        write_varint(out, self.thumbnail.len() as u64);
        out.extend_from_slice(&self.thumbnail);
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, SaveError> {
        let slot = r.string()?.to_owned();
        let engine_version = r.string()?.to_owned();
        let game_version = r.string()?.to_owned();
        let level = r.string()?.to_owned();
        let created_unix_ms = r.varint()?;
        let play_ms = r.varint()?;
        let n = r.len()?;
        let thumbnail = r.bytes(n)?.to_vec();
        Ok(Self {
            slot,
            engine_version,
            game_version,
            level,
            created_unix_ms,
            play_ms,
            thumbnail,
        })
    }
}

/// What writing a save took.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WriteStats {
    /// The body before compression, bytes.
    pub body_bytes: usize,
    /// The file, bytes.
    pub file_bytes: usize,
    /// Encoding the body (and its text form), milliseconds.
    pub encode_ms: f64,
    /// Compressing it, milliseconds.
    pub compress_ms: f64,
    /// Writing and swapping the file in, milliseconds.
    pub write_ms: f64,
}

/// The whole file of `image` in `encoding`, and what it took.
fn encode_timed(
    image: &SaveImage,
    encoding: Encoding,
    header: &SaveHeader,
) -> Result<(Vec<u8>, WriteStats), SaveError> {
    engine_profiler::zone!("save::encode");
    let mut stats = WriteStats::default();
    let start = Instant::now();
    let mut body = Vec::with_capacity(image.bytes.len() + image.bytes.len() / 2 + 1024);
    write_binary(image, &mut body);
    if encoding == Encoding::Text {
        body = crate::text::write_text(&read_binary(body)?)?.into_bytes();
    }
    stats.encode_ms = start.elapsed().as_secs_f64() * 1e3;
    stats.body_bytes = body.len();
    let start = Instant::now();
    let out = container(encoding, header, &body)?;
    stats.compress_ms = start.elapsed().as_secs_f64() * 1e3;
    stats.file_bytes = out.len();
    Ok((out, stats))
}

/// A save file of a text body: an edited dump made a save again.
///
/// # Errors
///
/// If compression fails.
pub fn text_file(text: &str, header: &SaveHeader) -> Result<Vec<u8>, SaveError> {
    container(Encoding::Text, header, text.as_bytes())
}

/// The file around a body: header, compressed body, checksum.
fn container(encoding: Encoding, header: &SaveHeader, body: &[u8]) -> Result<Vec<u8>, SaveError> {
    let stored = zstd::bulk::compress(body, LEVEL)?;
    let mut header = header.clone();
    header.engine_version = env!("CARGO_PKG_VERSION").to_owned();
    if header.created_unix_ms == 0 {
        header.created_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
    }
    let mut head = Vec::new();
    header.write(&mut head);
    let mut out = Vec::with_capacity(stored.len() + head.len() + 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&CONTAINER_VERSION.to_le_bytes());
    out.push(match encoding {
        Encoding::Binary => 0,
        Encoding::Text => 1,
    });
    out.push(1); // zstd
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(head.len() as u32).to_le_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(&(stored.len() as u64).to_le_bytes());
    out.extend_from_slice(&stored);
    let mut h = StateHasher::new();
    h.write(&out);
    out.extend_from_slice(&h.finish().to_le_bytes());
    Ok(out)
}

/// The bytes of a save file of `image` in `encoding` (no file written).
///
/// # Errors
///
/// If compression fails.
pub fn encode(
    image: &SaveImage,
    encoding: Encoding,
    header: &SaveHeader,
) -> Result<Vec<u8>, SaveError> {
    encode_timed(image, encoding, header).map(|(bytes, _)| bytes)
}

/// The header, encoding and decompressed body of a save file's bytes, checked.
pub(crate) fn open(file: &[u8]) -> Result<(SaveHeader, Encoding, Vec<u8>), SaveError> {
    if file.len() < 8 + 4 + 4 + 4 + 16 + 8 || &file[..8] != MAGIC {
        return Err(SaveError::damaged("not a save file"));
    }
    let (content, sum) = file.split_at(file.len() - 8);
    let mut h = StateHasher::new();
    h.write(content);
    if h.finish().to_le_bytes() != sum {
        return Err(SaveError::damaged(
            "its checksum does not match: the file is damaged",
        ));
    }
    let mut r = Reader::new(&content[8..]);
    let version = r.u32()?;
    if version == 0 || version > CONTAINER_VERSION {
        return Err(SaveError::damaged(format!(
            "container version {version}: written by a newer engine"
        )));
    }
    let encoding = match r.u8()? {
        0 => Encoding::Binary,
        1 => Encoding::Text,
        e => return Err(SaveError::damaged(format!("encoding {e}"))),
    };
    let flags = r.u8()?;
    r.bytes(2)?;
    let head_len = r.u32()? as usize;
    let header = SaveHeader::read(&mut Reader::new(r.bytes(head_len)?))?;
    let body_len = u64::from_le_bytes(r.bytes(8)?.try_into().unwrap_or_default());
    let stored_len = u64::from_le_bytes(r.bytes(8)?.try_into().unwrap_or_default());
    if body_len > MAX_BODY || stored_len > r.remaining() as u64 {
        return Err(SaveError::damaged("a body length past the file"));
    }
    let stored = r.bytes(stored_len as usize)?;
    let body = if flags & 1 != 0 {
        zstd::bulk::decompress(stored, body_len as usize)
            .map_err(|_| SaveError::damaged("the body does not decompress"))?
    } else {
        stored.to_vec()
    };
    if body.len() as u64 != body_len {
        return Err(SaveError::damaged("the body's length"));
    }
    Ok((header, encoding, body))
}

/// A save file's bytes read back.
///
/// # Errors
///
/// If they are not a save, are damaged, or were written by a newer engine.
pub fn decode(file: &[u8]) -> Result<LoadedSave, SaveError> {
    engine_profiler::zone!("save::decode");
    let (_, encoding, body) = open(file)?;
    match encoding {
        Encoding::Binary => read_binary(body),
        Encoding::Text => crate::text::read_text(
            core::str::from_utf8(&body)
                .map_err(|_| SaveError::damaged("text that is not UTF-8"))?,
        ),
    }
}

/// A save file's header, checked.
///
/// # Errors
///
/// As [`decode`].
pub fn read_header(file: &[u8]) -> Result<SaveHeader, SaveError> {
    open(file).map(|(header, _, _)| header)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Writes `image` to `path`: a temporary file next to it, flushed to the disk, then swapped
/// in; the previous save becomes `<path>.bak`. Power lost at any point leaves a complete
/// save under one of the three names, which [`read_save`] finds.
///
/// # Errors
///
/// If the file system refuses.
pub fn write_save(
    path: &Path,
    image: &SaveImage,
    encoding: Encoding,
    header: &SaveHeader,
) -> Result<WriteStats, SaveError> {
    engine_profiler::zone!("save::write_save");
    let (bytes, mut stats) = encode_timed(image, encoding, header)?;
    let start = Instant::now();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = with_suffix(path, ".tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    drop(f);
    if path.exists() {
        std::fs::rename(path, with_suffix(path, ".bak"))?;
    }
    std::fs::rename(&tmp, path)?;
    stats.write_ms = start.elapsed().as_secs_f64() * 1e3;
    Ok(stats)
}

/// Reads the save at `path`; if it is missing or damaged, the complete one a write left
/// behind (`.tmp`), then the previous one (`.bak`).
///
/// # Errors
///
/// The first file's error when none of them is a usable save.
pub fn read_save(path: &Path) -> Result<(SaveHeader, LoadedSave), SaveError> {
    engine_profiler::zone!("save::read_save");
    let mut first = None;
    for candidate in [
        path.to_owned(),
        with_suffix(path, ".tmp"),
        with_suffix(path, ".bak"),
    ] {
        let attempt = std::fs::read(&candidate)
            .map_err(SaveError::from)
            .and_then(|bytes| Ok((read_header(&bytes)?, decode(&bytes)?)));
        match attempt {
            Ok(found) => return Ok(found),
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    Err(first.unwrap_or_else(|| SaveError::damaged("no save")))
}

/// The file of a slot in `dir`: its name kept where Windows allows it, a hash added
/// when anything had to change (so two names never share a file).
pub fn slot_path(dir: &Path, slot: &str) -> PathBuf {
    let clean: String = slot
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let clean = clean.trim().to_owned();
    let name = if clean == slot && !clean.is_empty() {
        clean
    } else {
        let mut h = StateHasher::new();
        h.write(slot.as_bytes());
        format!(
            "{}-{:08x}",
            if clean.is_empty() { "slot" } else { &clean },
            h.finish() as u32
        )
    };
    dir.join(format!("{name}.save"))
}

/// A save slot found in a folder.
#[derive(Debug, Clone)]
pub struct SlotInfo {
    /// Its file.
    pub path: PathBuf,
    /// Its header.
    pub header: SaveHeader,
}

/// The saves in `dir` with their headers, newest first; damaged ones left out.
pub fn list_slots(dir: &Path) -> Vec<SlotInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut slots: Vec<SlotInfo> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "save"))
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let header = read_header(&bytes).ok()?;
            Some(SlotInfo { path, header })
        })
        .collect();
    slots.sort_by(|a, b| b.header.created_unix_ms.cmp(&a.header.created_unix_ms));
    slots
}
