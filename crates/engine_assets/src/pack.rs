//! Cooked packs (ADR 0056): the runtime's asset files. A pack is a header, entries
//! (blobs of cooked data, each starting on a [`BLOCK`] boundary so it is read unbuffered
//! straight into memory) and, at the end, a table of contents.
//!
//! Layout (little endian):
//!
//! | offset | bytes | |
//! |---:|---:|---|
//! | 0 | 64 | [`Header`] (the rest of the first block is zero) |
//! | 4096 | … | entries, each padded to a block |
//! | `toc_offset` | 80 × n | [`Entry`] records, padded to a block (the file's end) |
//!
//! - An entry is stored raw or compressed with zstd (when that saves at least 5%), and
//!   carries the hash of its raw bytes: damage is found when the entry is loaded.
//! - Ids are content hashes: the same texture used twice is stored once.
//! - Opening checks the header, the table's hash and every entry's bounds before anything
//!   is read from the entries.
//!
//! [`Pack::load`] reads entries through an I/O ring (`engine_platform::io`), many at a
//! time, and decompresses them on the job pool as they arrive.

use engine_core::hash::state_hash64;
use engine_core::jobs::ThreadPool;
use engine_platform::io::{AlignedBuffer, READ_ALIGN, ReadFile, ReadQueue};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const _: () = assert!(
    cfg!(target_endian = "little"),
    "packs are little endian and read in place"
);

/// Entries start on multiples of this (unbuffered reads).
pub const BLOCK: u64 = READ_ALIGN as u64;
/// First bytes of a pack.
pub const MAGIC: [u8; 4] = *b"OFPK";
/// Version of the layout.
pub const VERSION: u32 = 1;
/// Largest entry a pack accepts (raw bytes): 4 GiB.
pub const MAX_ENTRY_BYTES: u64 = 1 << 32;

/// The pack's header.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Header {
    /// [`MAGIC`].
    pub magic: [u8; 4],
    /// [`VERSION`].
    pub version: u32,
    /// Number of entries.
    pub entry_count: u32,
    /// Reserved, 0.
    pub flags: u32,
    /// Where the table of contents starts (a multiple of [`BLOCK`]).
    pub toc_offset: u64,
    /// Its length: 80 × `entry_count`.
    pub toc_bytes: u64,
    /// `state_hash64` of the table.
    pub toc_hash: u64,
    /// Length of the file.
    pub file_bytes: u64,
    /// Reserved, 0.
    pub reserved: [u64; 2],
}

const _: () = assert!(size_of::<Header>() == 64);

/// What an entry holds; the meaning of [`Entry::meta`] depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A texture's levels (see `crate::cook`).
    Texture,
    /// A mesh: vertices, indices, detail levels.
    Mesh,
    /// A scene: materials and instances.
    Scene,
}

impl EntryKind {
    fn code(self) -> u32 {
        match self {
            Self::Texture => 1,
            Self::Mesh => 2,
            Self::Scene => 3,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Texture),
            2 => Some(Self::Mesh),
            3 => Some(Self::Scene),
            _ => None,
        }
    }
}

/// How an entry is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// As is.
    None,
    /// zstd.
    Zstd,
}

/// An entry of the table of contents.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Entry {
    /// Content id (a 128-bit hash).
    pub id: [u64; 2],
    /// Where the stored bytes start (a multiple of [`BLOCK`]).
    pub offset: u64,
    /// Bytes stored (compressed or not).
    pub stored_bytes: u64,
    /// Bytes once decompressed.
    pub bytes: u64,
    /// `state_hash64` of the decompressed bytes.
    pub hash: u64,
    /// [`EntryKind`] code.
    pub kind: u32,
    /// 0: none, 1: zstd.
    pub compression: u32,
    /// Kind-specific description (dimensions, counts).
    pub meta: [u32; 6],
}

const _: () = assert!(size_of::<Entry>() == 80);

impl Entry {
    /// Its kind (checked when the pack was opened).
    pub fn kind(&self) -> EntryKind {
        EntryKind::from_code(self.kind).unwrap_or(EntryKind::Scene)
    }

    /// How it is stored (checked when the pack was opened).
    pub fn compression(&self) -> Compression {
        if self.compression == 1 {
            Compression::Zstd
        } else {
            Compression::None
        }
    }
}

/// Why a pack could not be written, opened or loaded.
#[derive(Debug)]
pub enum PackError {
    /// The file could not be read or written.
    Io(io::Error),
    /// The pack is not valid: not a pack, another version, damaged.
    Invalid(String),
}

impl core::fmt::Display for PackError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Invalid(why) => write!(f, "invalid pack: {why}"),
        }
    }
}

impl std::error::Error for PackError {}

impl From<io::Error> for PackError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub(crate) fn invalid(why: impl Into<String>) -> PackError {
    PackError::Invalid(why.into())
}

/// An entry ready to be written: its bytes compressed (or not) off the writing thread.
#[derive(Debug, Clone)]
pub struct PreparedEntry {
    entry: Entry,
    stored: Vec<u8>,
}

impl PreparedEntry {
    /// `data` of `kind` with `meta`, compressed with zstd at `level` (1-22) when that
    /// saves at least 5%; `level` 0 stores it as is. The id hashes the kind, the meta, the
    /// length and `state_hash64` of the data (fast on megabytes).
    pub fn new(kind: EntryKind, meta: [u32; 6], data: &[u8], level: i32) -> Self {
        let mut hasher = engine_core::hash::StableHasher::new();
        hasher.write_u64(u64::from(kind.code()));
        for m in meta {
            hasher.write_u64(u64::from(m));
        }
        hasher.write_u64(data.len() as u64);
        hasher.write_u64(state_hash64(data));
        Self::with_id(hasher.finish(), kind, meta, data, level)
    }

    /// As [`Self::new`], with an id of the caller's (for example a derived data cache
    /// key).
    pub fn with_id(id: u128, kind: EntryKind, meta: [u32; 6], data: &[u8], level: i32) -> Self {
        let compressed = (level > 0)
            .then(|| zstd::bulk::compress(data, level).ok())
            .flatten()
            .filter(|c| (c.len() as u64) * 100 <= (data.len() as u64) * 95);
        let (compression, stored) = match compressed {
            Some(c) => (1, c),
            None => (0, data.to_vec()),
        };
        Self {
            entry: Entry {
                id: [id as u64, (id >> 64) as u64],
                offset: 0,
                stored_bytes: stored.len() as u64,
                bytes: data.len() as u64,
                hash: state_hash64(data),
                kind: kind.code(),
                compression,
                meta,
            },
            stored,
        }
    }

    /// Its content id.
    pub fn id(&self) -> u128 {
        u128::from(self.entry.id[0]) | (u128::from(self.entry.id[1]) << 64)
    }

    #[cfg(test)]
    pub(crate) fn stored_for_tests(&self) -> &[u8] {
        &self.stored
    }

    #[cfg(test)]
    pub(crate) fn meta_for_tests(&self) -> [u32; 6] {
        self.entry.meta
    }
}

/// What a finished pack holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PackStats {
    /// Entries written (after removing duplicates).
    pub entries: usize,
    /// Raw bytes of the entries.
    pub bytes: u64,
    /// Bytes stored (after compression).
    pub stored_bytes: u64,
    /// Length of the file.
    pub file_bytes: u64,
}

/// Writes a pack: entries one after another, the table at [`Self::finish`]. The file is
/// written next to its path and renamed over it when finished.
#[derive(Debug)]
pub struct PackWriter {
    file: io::BufWriter<std::fs::File>,
    temporary: PathBuf,
    path: PathBuf,
    entries: Vec<Entry>,
    stats: PackStats,
    offset: u64,
}

impl PackWriter {
    /// Starts a pack at `path`.
    pub fn create(path: &Path) -> Result<Self, PackError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let mut file = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&temporary)?);
        // The header's block, filled in by `finish`.
        file.write_all(&[0; BLOCK as usize])?;
        Ok(Self {
            file,
            temporary,
            path: path.to_owned(),
            entries: Vec::new(),
            stats: PackStats::default(),
            offset: BLOCK,
        })
    }

    /// Writes an entry and returns its index; an entry with the same id as an earlier one
    /// is not written again (its index is returned).
    pub fn add(&mut self, prepared: &PreparedEntry) -> Result<u32, PackError> {
        if let Some(index) = self.entries.iter().position(|e| e.id == prepared.entry.id) {
            return Ok(index as u32);
        }
        if prepared.entry.bytes > MAX_ENTRY_BYTES {
            return Err(invalid("entry larger than 4 GiB"));
        }
        self.file.write_all(&prepared.stored)?;
        let padded = (prepared.stored.len() as u64).next_multiple_of(BLOCK);
        write_zeros(&mut self.file, padded - prepared.stored.len() as u64)?;
        self.entries.push(Entry {
            offset: self.offset,
            ..prepared.entry
        });
        self.offset += padded;
        self.stats.entries += 1;
        self.stats.bytes += prepared.entry.bytes;
        self.stats.stored_bytes += prepared.entry.stored_bytes;
        Ok((self.entries.len() - 1) as u32)
    }

    /// Writes the table and the header, and puts the pack in place.
    pub fn finish(mut self) -> Result<PackStats, PackError> {
        let toc: &[u8] = bytemuck::cast_slice(&self.entries);
        self.file.write_all(toc)?;
        let end = (self.offset + toc.len() as u64).next_multiple_of(BLOCK);
        write_zeros(&mut self.file, end - self.offset - toc.len() as u64)?;
        let header = Header {
            magic: MAGIC,
            version: VERSION,
            entry_count: self.entries.len() as u32,
            flags: 0,
            toc_offset: self.offset,
            toc_bytes: toc.len() as u64,
            toc_hash: state_hash64(toc),
            file_bytes: end,
            reserved: [0; 2],
        };
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(bytemuck::bytes_of(&header))?;
        let file = self.file.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&self.temporary, &self.path)?;
        self.stats.file_bytes = end;
        Ok(self.stats)
    }
}

fn write_zeros(out: &mut impl Write, mut count: u64) -> io::Result<()> {
    const ZEROS: [u8; BLOCK as usize] = [0; BLOCK as usize];
    while count > 0 {
        let n = count.min(BLOCK);
        out.write_all(&ZEROS[..n as usize])?;
        count -= n;
    }
    Ok(())
}

/// An open pack: its table of contents, checked.
#[derive(Debug)]
pub struct Pack {
    file: Arc<ReadFile>,
    entries: Vec<Entry>,
}

/// An entry's bytes as loaded: in the read buffer when it was stored raw (no copy), or
/// decompressed.
#[derive(Debug)]
pub enum EntryData {
    /// The read buffer; the entry is its first `len` bytes.
    Read(AlignedBuffer, usize),
    /// Decompressed.
    Decompressed(Vec<u8>),
}

impl core::ops::Deref for EntryData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Read(buffer, len) => &buffer.as_slice()[..*len],
            Self::Decompressed(bytes) => bytes,
        }
    }
}

/// What [`Pack::load`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoadStats {
    /// Bytes read from the file.
    pub read_bytes: u64,
    /// Bytes once decompressed.
    pub bytes: u64,
    /// Seconds from the first read to the last entry handed over.
    pub seconds: f64,
    /// Seconds spent decompressing and checking entries, summed over threads.
    pub decode_seconds: f64,
    /// The read queue's backend (`"io ring"` or `"synchronous"`).
    pub backend: &'static str,
}

impl Pack {
    /// Opens the pack at `path` and checks its table of contents.
    pub fn open(path: &Path) -> Result<Self, PackError> {
        let file = Arc::new(ReadFile::open(path)?);
        let mut queue = ReadQueue::new(2)?;
        let header_block = read_blocking(&mut queue, &file, 0, BLOCK as usize)?;
        let header: Header =
            bytemuck::pod_read_unaligned(&header_block.as_slice()[..size_of::<Header>()]);
        if header.magic != MAGIC {
            return Err(invalid("not a pack"));
        }
        if header.version != VERSION {
            return Err(invalid(format!(
                "version {}, this engine reads {VERSION}",
                header.version
            )));
        }
        if header.file_bytes != file.len() {
            return Err(invalid(format!(
                "{} bytes long, the header says {} (truncated?)",
                file.len(),
                header.file_bytes
            )));
        }
        let count = u64::from(header.entry_count);
        if header.toc_bytes != count * size_of::<Entry>() as u64
            || !header.toc_offset.is_multiple_of(BLOCK)
            || header.toc_offset < BLOCK
            || header.toc_offset + header.toc_bytes > header.file_bytes
        {
            return Err(invalid("table of contents out of bounds"));
        }
        let toc_block = read_blocking(
            &mut queue,
            &file,
            header.toc_offset,
            header.toc_bytes as usize,
        )?;
        let toc = &toc_block.as_slice()[..header.toc_bytes as usize];
        if state_hash64(toc) != header.toc_hash {
            return Err(invalid("table of contents damaged"));
        }
        let entries: Vec<Entry> = toc
            .chunks_exact(size_of::<Entry>())
            .map(bytemuck::pod_read_unaligned)
            .collect();
        for (i, e) in entries.iter().enumerate() {
            let fail = |why: &str| invalid(format!("entry {i}: {why}"));
            if EntryKind::from_code(e.kind).is_none() {
                return Err(fail("unknown kind"));
            }
            if e.compression > 1 || (e.compression == 0 && e.stored_bytes != e.bytes) {
                return Err(fail("bad compression"));
            }
            if e.bytes > MAX_ENTRY_BYTES || e.stored_bytes > MAX_ENTRY_BYTES {
                return Err(fail("too large"));
            }
            if !e.offset.is_multiple_of(BLOCK)
                || e.offset < BLOCK
                || e.offset + e.stored_bytes > header.toc_offset
            {
                return Err(fail("out of bounds"));
            }
        }
        Ok(Self { file, entries })
    }

    /// The table of contents.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Loads the entries at `indices`, up to `depth` reads in flight, decompressing and
    /// checking each on `jobs` and handing it to `take(index, entry, data)` there (in no
    /// particular order). Stops at the first failure, which it returns.
    pub fn load<F>(
        &self,
        indices: &[u32],
        jobs: &ThreadPool,
        depth: usize,
        take: F,
    ) -> Result<LoadStats, PackError>
    where
        F: Fn(u32, &Entry, EntryData) -> Result<(), PackError> + Sync,
    {
        engine_profiler::zone!("assets::Pack::load");
        let start = std::time::Instant::now();
        let mut stats = LoadStats::default();
        for &i in indices {
            let e = self
                .entries
                .get(i as usize)
                .ok_or_else(|| invalid(format!("no entry {i}")))?;
            stats.read_bytes += e.stored_bytes;
            stats.bytes += e.bytes;
        }
        let mut queue = ReadQueue::new(depth)?;
        stats.backend = queue.backend();
        let failure: OnceLock<PackError> = OnceLock::new();
        let decode_nanos = core::sync::atomic::AtomicU64::new(0);
        jobs.scope(|scope| {
            let mut next = indices.iter().copied();
            let mut more = true;
            while failure.get().is_none() && (more || queue.pending() > 0) {
                while queue.has_room() {
                    let Some(index) = next.next() else {
                        more = false;
                        break;
                    };
                    let e = &self.entries[index as usize];
                    let buffer = AlignedBuffer::new(e.stored_bytes as usize);
                    if let Err(refused) = queue.push(&self.file, e.offset, buffer, u64::from(index))
                    {
                        let _ = failure.set(refused.error.into());
                        return;
                    }
                }
                if let Err(e) = queue.submit(true) {
                    let _ = failure.set(e.into());
                    return;
                }
                while let Some(done) = queue.pop() {
                    let index = done.tag as u32;
                    let e = &self.entries[index as usize];
                    let (take, failure, decode_nanos) = (&take, &failure, &decode_nanos);
                    scope.spawn(move || {
                        let decoding = std::time::Instant::now();
                        let decoded = done
                            .result
                            .map_err(PackError::from)
                            .and_then(|read| decode(index, e, done.buffer, read));
                        decode_nanos.fetch_add(
                            decoding.elapsed().as_nanos() as u64,
                            core::sync::atomic::Ordering::Relaxed,
                        );
                        let result = decoded.and_then(|data| take(index, e, data));
                        if let Err(error) = result {
                            let _ = failure.set(error);
                        }
                    });
                }
            }
        });
        if let Some(error) = failure.into_inner() {
            return Err(error);
        }
        stats.seconds = start.elapsed().as_secs_f64();
        stats.decode_seconds = decode_nanos.into_inner() as f64 * 1e-9;
        Ok(stats)
    }
}

/// Reads `len` bytes at `offset` (block-aligned) and waits.
fn read_blocking(
    queue: &mut ReadQueue,
    file: &Arc<ReadFile>,
    offset: u64,
    len: usize,
) -> Result<AlignedBuffer, PackError> {
    queue
        .push(file, offset, AlignedBuffer::new(len), 0)
        .map_err(|e| e.error)?;
    loop {
        queue.submit(true)?;
        if let Some(done) = queue.pop() {
            let read = done.result?;
            if read < len {
                return Err(invalid("truncated"));
            }
            return Ok(done.buffer);
        }
    }
}

/// An entry's bytes from its read: decompressed if it was compressed, then checked.
fn decode(
    index: u32,
    e: &Entry,
    buffer: AlignedBuffer,
    read: usize,
) -> Result<EntryData, PackError> {
    let stored = e.stored_bytes as usize;
    if read < stored {
        return Err(invalid(format!("entry {index}: truncated")));
    }
    let data = match e.compression() {
        Compression::None => EntryData::Read(buffer, stored),
        Compression::Zstd => {
            let bytes = zstd::bulk::decompress(&buffer.as_slice()[..stored], e.bytes as usize)
                .map_err(|why| invalid(format!("entry {index}: {why}")))?;
            EntryData::Decompressed(bytes)
        }
    };
    if data.len() as u64 != e.bytes || state_hash64(&data) != e.hash {
        return Err(invalid(format!("entry {index}: damaged (hash mismatch)")));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "engine_assets_pack_{name}_{}.pack",
            std::process::id()
        ))
    }

    /// Entries of varied sizes and compressibility.
    fn samples() -> Vec<Vec<u8>> {
        let mut x = 7u32;
        let mut noise = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        };
        vec![
            vec![7; 10],
            (0..100_000).map(|i| (i % 7) as u8).collect(),
            (0..50_000).map(|_| noise()).collect(),
            vec![],
            (0..BLOCK as usize).map(|_| noise()).collect(),
        ]
    }

    fn write(path: &Path, level: i32) -> PackStats {
        let mut writer = PackWriter::create(path).unwrap();
        for (i, data) in samples().iter().enumerate() {
            let meta = [i as u32, 0, 0, 0, 0, 0];
            let prepared = PreparedEntry::new(EntryKind::Texture, meta, data, level);
            assert_eq!(writer.add(&prepared).unwrap(), i as u32);
        }
        // A duplicate is stored once.
        let meta = [1, 0, 0, 0, 0, 0];
        let again = PreparedEntry::new(EntryKind::Texture, meta, &samples()[1], level);
        assert_eq!(writer.add(&again).unwrap(), 1);
        writer.finish().unwrap()
    }

    fn load_all(pack: &Pack) -> Result<Vec<Vec<u8>>, PackError> {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let out = Mutex::new(vec![Vec::new(); pack.entries().len()]);
        let indices: Vec<u32> = (0..pack.entries().len() as u32).collect();
        pack.load(&indices, &jobs, 3, |i, _, data| {
            out.lock().unwrap()[i as usize] = data.to_vec();
            Ok(())
        })?;
        Ok(out.into_inner().unwrap())
    }

    #[test]
    fn round_trip_raw_and_compressed() {
        for level in [0, 3] {
            let path = temp(&format!("round_trip_{level}"));
            let stats = write(&path, level);
            assert_eq!(stats.entries, 5);
            assert!(stats.file_bytes.is_multiple_of(BLOCK));
            if level > 0 {
                assert!(stats.stored_bytes < stats.bytes);
            }
            let pack = Pack::open(&path).unwrap();
            assert_eq!(pack.entries().len(), 5);
            for (i, e) in pack.entries().iter().enumerate() {
                assert_eq!(e.meta[0], i as u32);
                assert!(e.offset.is_multiple_of(BLOCK));
            }
            // The incompressible entry is stored raw even when compressing.
            assert_eq!(pack.entries()[2].compression(), Compression::None);
            assert_eq!(load_all(&pack).unwrap(), samples());
            drop(pack);
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn damage_is_found() {
        let path = temp("damage");
        write(&path, 3);
        let good = std::fs::read(&path).unwrap();
        let reopen = |bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            Pack::open(&path)
        };
        let message = |r: Result<Pack, PackError>| match r {
            Err(PackError::Invalid(why)) => why,
            other => panic!("{other:?}"),
        };
        let mut b = good.clone();
        b[0] = b'X';
        assert!(message(reopen(&b)).contains("not a pack"));
        let mut b = good.clone();
        b[4] = 9;
        assert!(message(reopen(&b)).contains("version"));
        assert!(message(reopen(&good[..good.len() - BLOCK as usize])).contains("truncated"));
        let header: Header = bytemuck::pod_read_unaligned(&good[..64]);
        let mut b = good.clone();
        b[header.toc_offset as usize + 20] ^= 1;
        assert!(message(reopen(&b)).contains("table of contents"));
        // An entry's bytes: found when it is loaded.
        let mut b = good.clone();
        b[BLOCK as usize + 1] ^= 1;
        let pack = reopen(&b).unwrap();
        match load_all(&pack) {
            Err(PackError::Invalid(why)) => assert!(why.contains("entry 0"), "{why}"),
            other => panic!("{other:?}"),
        }
        drop(pack);
        let _ = std::fs::remove_file(path);
    }
}
