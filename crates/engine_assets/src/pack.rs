//! Cooked packs (ADR 0056): the runtime's asset files. A pack is a header, entries (blobs
//! of cooked data, each starting on a [`BLOCK`] boundary so it is read unbuffered straight
//! into memory) and, at the end, a table of contents.
//!
//! Layout (little endian):
//!
//! | offset | bytes | |
//! |---:|---:|---|
//! | 0 | 64 | [`Header`] (the rest of the first block is zero) |
//! | 4096 | … | entries, each padded to a block |
//! | `toc_offset` | 80 × entries + 16 × chunks | [`Entry`] records, then [`Chunk`] records, padded to a block (the file's end) |
//!
//! - An entry is cut into chunks of `1 << chunk_shift` raw bytes (1 MiB by default), each
//!   compressed with zstd when that saves at least 5%, otherwise stored raw. An entry
//!   whose chunks are all raw is stored as its raw bytes, so it can be read straight to
//!   where it is used.
//! - Chunks decompress in parallel and each carries the hash of its raw bytes: damage is
//!   found when an entry is loaded, down to the chunk.
//! - Ids are content hashes: the same data stored twice is written once.
//! - Opening checks the header, the table's hash and every entry's and chunk's bounds
//!   before anything is read from the entries.
//!
//! [`Pack::load`] reads entries through an I/O ring (`engine_platform::io`), many at a
//! time, and decompresses their chunks on the job pool as they arrive.

use engine_core::hash::state_hash64;
use engine_core::jobs::{Scope, ThreadPool};
use engine_platform::io::{AlignedBuffer, READ_ALIGN, ReadFile, ReadQueue};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
/// Chunks of 1 MiB: measured on BC7/BC5 textures and meshes, they compress within
/// 0.6-2.3 points of whole entries (65.7% against 65.1% for textures), and a 4K texture's
/// top level decompresses on 16 threads.
pub const DEFAULT_CHUNK_SHIFT: u32 = 20;
/// Marks a raw chunk in [`Chunk::stored`].
const RAW_CHUNK: u32 = 1 << 31;

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
    /// Number of chunks.
    pub chunk_count: u32,
    /// Chunks hold `1 << chunk_shift` raw bytes (the last of an entry fewer).
    pub chunk_shift: u32,
    /// Reserved, 0.
    pub reserved: u32,
    /// Where the table of contents starts (a multiple of [`BLOCK`]).
    pub toc_offset: u64,
    /// Its length: 80 × entries + 16 × chunks.
    pub toc_bytes: u64,
    /// `state_hash64` of the table.
    pub toc_hash: u64,
    /// Length of the file.
    pub file_bytes: u64,
    /// Reserved, 0.
    pub reserved2: u64,
}

const _: () = assert!(size_of::<Header>() == 64);

/// What an entry holds; the meaning of [`Entry::meta`] depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// Mip levels of a texture (see `crate::cook`).
    TextureLevels,
    /// A mesh: vertices, indices, detail levels.
    Mesh,
    /// A scene: textures, materials and instances.
    Scene,
}

impl EntryKind {
    fn code(self) -> u32 {
        match self {
            Self::TextureLevels => 1,
            Self::Mesh => 2,
            Self::Scene => 3,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::TextureLevels),
            2 => Some(Self::Mesh),
            3 => Some(Self::Scene),
            _ => None,
        }
    }
}

/// An entry of the table of contents.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Entry {
    /// Content id (a 128-bit hash).
    pub id: [u64; 2],
    /// Where the stored bytes start (a multiple of [`BLOCK`]).
    pub offset: u64,
    /// Bytes stored (its chunks, one after another).
    pub stored_bytes: u64,
    /// Raw bytes.
    pub bytes: u64,
    /// Its first chunk in the table.
    pub first_chunk: u32,
    /// Its number of chunks.
    pub chunk_count: u32,
    /// [`EntryKind`] code.
    pub kind: u32,
    /// Kind-specific description (dimensions, counts).
    pub meta: [u32; 7],
}

const _: () = assert!(size_of::<Entry>() == 80);

impl Entry {
    /// Its kind (checked when the pack was opened).
    pub fn kind(&self) -> EntryKind {
        EntryKind::from_code(self.kind).unwrap_or(EntryKind::Scene)
    }

    /// Whether it is stored as its raw bytes (every chunk raw).
    pub fn is_raw(&self) -> bool {
        self.stored_bytes == self.bytes
    }
}

/// A chunk of an entry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Chunk {
    /// Where its stored bytes start, from the entry's start.
    pub offset: u32,
    /// Bytes stored; the high bit marks a raw chunk.
    pub stored: u32,
    /// `state_hash64` of its raw bytes.
    pub hash: u64,
}

const _: () = assert!(size_of::<Chunk>() == 16);

impl Chunk {
    /// Bytes stored.
    pub fn stored_bytes(&self) -> u32 {
        self.stored & !RAW_CHUNK
    }

    /// Whether it is stored raw.
    pub fn is_raw(&self) -> bool {
        self.stored & RAW_CHUNK != 0
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

/// An entry ready to be written: its chunks compressed (or not) off the writing thread.
#[derive(Debug, Clone)]
pub struct PreparedEntry {
    entry: Entry,
    chunks: Vec<Chunk>,
    stored: Vec<u8>,
}

impl PreparedEntry {
    /// `data` of `kind` with `meta`, cut into chunks of `1 << chunk_shift` bytes, each
    /// compressed with zstd at `level` (1-22) when that saves at least 5%; `level` 0
    /// stores every chunk raw. The id hashes the kind, the meta, the length and
    /// `state_hash64` of the data.
    pub fn new(kind: EntryKind, meta: [u32; 7], data: &[u8], level: i32, chunk_shift: u32) -> Self {
        let mut hasher = engine_core::hash::StableHasher::new();
        hasher.write_u64(u64::from(kind.code()));
        for m in meta {
            hasher.write_u64(u64::from(m));
        }
        hasher.write_u64(data.len() as u64);
        hasher.write_u64(state_hash64(data));
        let id = hasher.finish();
        let mut chunks = Vec::new();
        let mut stored = Vec::new();
        for raw in data.chunks(1 << chunk_shift.min(30)) {
            let compressed = (level > 0)
                .then(|| zstd::bulk::compress(raw, level).ok())
                .flatten()
                .filter(|c| (c.len() as u64) * 100 <= (raw.len() as u64) * 95);
            let offset = stored.len() as u32;
            let flag = match compressed {
                Some(c) => {
                    stored.extend_from_slice(&c);
                    0
                }
                None => {
                    stored.extend_from_slice(raw);
                    RAW_CHUNK
                }
            };
            chunks.push(Chunk {
                offset,
                stored: (stored.len() as u32 - offset) | flag,
                hash: state_hash64(raw),
            });
        }
        Self {
            entry: Entry {
                id: [id as u64, (id >> 64) as u64],
                offset: 0,
                stored_bytes: stored.len() as u64,
                bytes: data.len() as u64,
                first_chunk: 0,
                chunk_count: chunks.len() as u32,
                kind: kind.code(),
                meta,
            },
            chunks,
            stored,
        }
    }

    /// Its content id.
    pub fn id(&self) -> u128 {
        u128::from(self.entry.id[0]) | (u128::from(self.entry.id[1]) << 64)
    }

    /// Its raw size.
    pub fn bytes(&self) -> u64 {
        self.entry.bytes
    }

    #[cfg(test)]
    pub(crate) fn raw_for_tests(&self) -> Vec<u8> {
        self.chunks
            .iter()
            .flat_map(|c| {
                let s = &self.stored[c.offset as usize..][..c.stored_bytes() as usize];
                if c.is_raw() {
                    s.to_vec()
                } else {
                    zstd::bulk::decompress(s, 1 << 30).unwrap_or_default()
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn meta_for_tests(&self) -> [u32; 7] {
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

/// Writes a pack: entries one after another, in the order they are added (put what loads
/// first at the front: a hard disk reads the file in order), the table at
/// [`Self::finish`]. The file is written next to its path and renamed over it when
/// finished.
#[derive(Debug)]
pub struct PackWriter {
    file: io::BufWriter<std::fs::File>,
    temporary: PathBuf,
    path: PathBuf,
    entries: Vec<Entry>,
    chunks: Vec<Chunk>,
    chunk_shift: u32,
    stats: PackStats,
    offset: u64,
}

impl PackWriter {
    /// Starts a pack at `path` whose entries are prepared with `chunk_shift` (12: 4 KiB
    /// to 26: 64 MiB).
    pub fn create(path: &Path, chunk_shift: u32) -> Result<Self, PackError> {
        if !(12..=26).contains(&chunk_shift) {
            return Err(invalid("chunks from 4 KiB to 64 MiB"));
        }
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
            chunks: Vec::new(),
            chunk_shift,
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
        let expected = prepared.entry.bytes.div_ceil(1 << self.chunk_shift);
        if u64::from(prepared.entry.chunk_count) != expected {
            return Err(invalid("entry prepared with another chunk size"));
        }
        self.file.write_all(&prepared.stored)?;
        let padded = (prepared.stored.len() as u64).next_multiple_of(BLOCK);
        write_zeros(&mut self.file, padded - prepared.stored.len() as u64)?;
        self.entries.push(Entry {
            offset: self.offset,
            first_chunk: self.chunks.len() as u32,
            ..prepared.entry
        });
        self.chunks.extend_from_slice(&prepared.chunks);
        self.offset += padded;
        self.stats.entries += 1;
        self.stats.bytes += prepared.entry.bytes;
        self.stats.stored_bytes += prepared.entry.stored_bytes;
        Ok((self.entries.len() - 1) as u32)
    }

    /// Writes the table and the header, and puts the pack in place.
    pub fn finish(mut self) -> Result<PackStats, PackError> {
        let mut toc: Vec<u8> = bytemuck::cast_slice(&self.entries).to_vec();
        toc.extend_from_slice(bytemuck::cast_slice(&self.chunks));
        self.file.write_all(&toc)?;
        let end = (self.offset + toc.len() as u64).next_multiple_of(BLOCK);
        write_zeros(&mut self.file, end - self.offset - toc.len() as u64)?;
        let header = Header {
            magic: MAGIC,
            version: VERSION,
            entry_count: self.entries.len() as u32,
            chunk_count: self.chunks.len() as u32,
            chunk_shift: self.chunk_shift,
            reserved: 0,
            toc_offset: self.offset,
            toc_bytes: toc.len() as u64,
            toc_hash: state_hash64(&toc),
            file_bytes: end,
            reserved2: 0,
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
    chunks: Vec<Chunk>,
    chunk_shift: u32,
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
    /// Raw bytes of the entries.
    pub bytes: u64,
    /// Seconds from the first read to the last entry handed over.
    pub seconds: f64,
    /// Seconds spent decompressing and checking chunks, summed over threads.
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
        let entries_bytes = u64::from(header.entry_count) * size_of::<Entry>() as u64;
        let chunks_bytes = u64::from(header.chunk_count) * size_of::<Chunk>() as u64;
        if !(12..=26).contains(&header.chunk_shift)
            || header.toc_bytes != entries_bytes + chunks_bytes
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
        let (entry_bytes, chunk_bytes) = toc.split_at(entries_bytes as usize);
        let entries: Vec<Entry> = entry_bytes
            .chunks_exact(size_of::<Entry>())
            .map(bytemuck::pod_read_unaligned)
            .collect();
        let chunks: Vec<Chunk> = chunk_bytes
            .chunks_exact(size_of::<Chunk>())
            .map(bytemuck::pod_read_unaligned)
            .collect();
        let chunk_bytes = 1u64 << header.chunk_shift;
        for (i, e) in entries.iter().enumerate() {
            let fail = |why: &str| invalid(format!("entry {i}: {why}"));
            if EntryKind::from_code(e.kind).is_none() {
                return Err(fail("unknown kind"));
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
            let first = e.first_chunk as usize;
            let Some(own) = chunks.get(first..first + e.chunk_count as usize) else {
                return Err(fail("chunks out of bounds"));
            };
            if u64::from(e.chunk_count) != e.bytes.div_ceil(chunk_bytes) {
                return Err(fail("wrong number of chunks"));
            }
            let mut next = 0u64;
            for (k, c) in own.iter().enumerate() {
                let raw = (e.bytes - k as u64 * chunk_bytes).min(chunk_bytes);
                let stored = u64::from(c.stored_bytes());
                if u64::from(c.offset) != next
                    || (c.is_raw() && stored != raw)
                    || (!c.is_raw() && stored >= raw)
                {
                    return Err(fail("bad chunk"));
                }
                next += stored;
            }
            if next != e.stored_bytes {
                return Err(fail("chunks do not fill it"));
            }
        }
        Ok(Self {
            file,
            entries,
            chunks,
            chunk_shift: header.chunk_shift,
        })
    }

    /// The table of contents.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The chunks of `entry`.
    pub fn chunks(&self, entry: &Entry) -> &[Chunk] {
        &self.chunks[entry.first_chunk as usize..][..entry.chunk_count as usize]
    }

    /// Loads the entries at `indices` (in that order: put what is needed first first), up
    /// to `depth` reads in flight, decompressing and checking their chunks in parallel on
    /// `jobs` and handing each entry to `take(index, entry, data)` there, in no particular
    /// order. Stops at the first failure, which it returns.
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
        let shared = Shared {
            take: &take,
            failure: OnceLock::new(),
            decode_nanos: AtomicU64::new(0),
            chunk_shift: self.chunk_shift,
        };
        jobs.scope(|scope| {
            let mut next = indices.iter().copied();
            let mut more = true;
            while shared.failure.get().is_none() && (more || queue.pending() > 0) {
                while queue.has_room() {
                    let Some(index) = next.next() else {
                        more = false;
                        break;
                    };
                    let e = &self.entries[index as usize];
                    let buffer = AlignedBuffer::new(e.stored_bytes as usize);
                    if let Err(refused) = queue.push(&self.file, e.offset, buffer, u64::from(index))
                    {
                        shared.fail(refused.error.into());
                        return;
                    }
                }
                if let Err(e) = queue.submit(true) {
                    shared.fail(e.into());
                    return;
                }
                while let Some(done) = queue.pop() {
                    let index = done.tag as u32;
                    let e = &self.entries[index as usize];
                    match done.result {
                        Ok(read) if read as u64 >= e.stored_bytes => {
                            decode(scope, &shared, index, e, self.chunks(e), done.buffer);
                        }
                        Ok(_) => shared.fail(invalid(format!("entry {index}: truncated"))),
                        Err(error) => shared.fail(error.into()),
                    }
                }
            }
        });
        if let Some(error) = shared.failure.into_inner() {
            return Err(error);
        }
        stats.seconds = start.elapsed().as_secs_f64();
        stats.decode_seconds = shared.decode_nanos.into_inner() as f64 * 1e-9;
        Ok(stats)
    }
}

/// What the jobs of [`Pack::load`] share.
struct Shared<'a, F> {
    take: &'a F,
    failure: OnceLock<PackError>,
    decode_nanos: AtomicU64,
    chunk_shift: u32,
}

impl<F> Shared<'_, F> {
    fn fail(&self, error: PackError) {
        let _ = self.failure.set(error);
    }
}

/// An address inside an entry's read buffer or output, used by its chunk jobs.
#[derive(Clone, Copy)]
struct Address(*mut u8);

// SAFETY: chunk jobs read the read buffer and write disjoint ranges of the output; both
// outlive them (see `Assembly`).
unsafe impl Send for Address {}
// SAFETY: as above.
unsafe impl Sync for Address {}

/// An entry being checked and decompressed: its read buffer and its output (which its
/// chunk jobs read and fill through `input` and `output`), and how many chunks are left.
/// The job that finishes last takes them and hands the entry over.
struct Assembly {
    buffers: Mutex<Option<(AlignedBuffer, Vec<u8>)>>,
    input: Address,
    output: Address,
    remaining: AtomicUsize,
}

/// Checks and decompresses entry `index` from its read `buffer`: one job per chunk; the
/// job that finishes last hands the entry to `take`.
fn decode<'scope, 'env, F>(
    scope: &'scope Scope<'scope, 'env>,
    shared: &'scope Shared<'scope, F>,
    index: u32,
    e: &'scope Entry,
    chunks: &'scope [Chunk],
    mut buffer: AlignedBuffer,
) where
    F: Fn(u32, &Entry, EntryData) -> Result<(), PackError> + Sync,
{
    let chunk_bytes = 1usize << shared.chunk_shift;
    let raw = e.is_raw();
    if chunks.is_empty() {
        if let Err(error) = (shared.take)(index, e, EntryData::Read(buffer, 0)) {
            shared.fail(error);
        }
        return;
    }
    let mut output = if raw {
        Vec::new()
    } else {
        vec![0u8; e.bytes as usize]
    };
    // Heap addresses: they stay put when the buffers move into the assembly.
    let assembly = Arc::new(Assembly {
        input: Address(buffer.as_mut_slice().as_mut_ptr()),
        output: Address(output.as_mut_ptr()),
        buffers: Mutex::new(Some((buffer, output))),
        remaining: AtomicUsize::new(chunks.len()),
    });
    for (k, chunk) in chunks.iter().enumerate() {
        let assembly = Arc::clone(&assembly);
        scope.spawn(move || {
            let start = std::time::Instant::now();
            let raw_start = k * chunk_bytes;
            let raw_len = (e.bytes as usize - raw_start).min(chunk_bytes);
            // SAFETY: the read buffer holds at least `e.stored_bytes` bytes (checked when
            // the read finished) and chunk bounds were checked when the pack was opened;
            // the assembly keeps the buffer alive and unmoved until the last chunk is
            // done, and nothing writes it meanwhile.
            let stored = unsafe {
                core::slice::from_raw_parts(
                    assembly.input.0.add(chunk.offset as usize),
                    chunk.stored_bytes() as usize,
                )
            };
            let result = if raw {
                // Raw entries stay in the read buffer: only the check.
                Ok(stored)
            } else {
                // SAFETY: the output holds `e.bytes` bytes and lives as above; this job
                // alone writes `raw_start..raw_start + raw_len`, and nobody reads it before
                // every chunk is done.
                let target = unsafe {
                    core::slice::from_raw_parts_mut(assembly.output.0.add(raw_start), raw_len)
                };
                if chunk.is_raw() {
                    target.copy_from_slice(stored);
                    Ok(&*target)
                } else {
                    match zstd::bulk::decompress_to_buffer(stored, target) {
                        Ok(n) if n == raw_len => Ok(&*target),
                        Ok(_) => Err(invalid(format!("entry {index}: chunk {k} too short"))),
                        Err(why) => Err(invalid(format!("entry {index}: chunk {k}: {why}"))),
                    }
                }
            };
            let checked = result.and_then(|bytes| {
                if state_hash64(bytes) == chunk.hash {
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "entry {index}: chunk {k} damaged (hash mismatch)"
                    )))
                }
            });
            shared
                .decode_nanos
                .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if let Err(error) = checked {
                shared.fail(error);
            }
            // The last chunk hands the entry over: every other chunk's writes happen
            // before its decrement.
            if assembly.remaining.fetch_sub(1, Ordering::AcqRel) != 1
                || shared.failure.get().is_some()
            {
                return;
            }
            let Some((buffer, output)) = assembly.buffers.lock().ok().and_then(|mut b| b.take())
            else {
                return;
            };
            let data = if raw {
                EntryData::Read(buffer, e.bytes as usize)
            } else {
                EntryData::Decompressed(output)
            };
            if let Err(error) = (shared.take)(index, e, data) {
                shared.fail(error);
            }
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Small chunks, so the samples span several.
    const SHIFT: u32 = 12;

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
        let mut mixed: Vec<u8> = (0..10_000).map(|i| (i % 7) as u8).collect();
        mixed.extend((0..10_000).map(|_| noise()));
        vec![
            vec![7; 10],
            (0..100_000).map(|i| (i % 7) as u8).collect(),
            (0..50_000).map(|_| noise()).collect(),
            vec![],
            (0..BLOCK as usize).map(|_| noise()).collect(),
            mixed,
        ]
    }

    fn write(path: &Path, level: i32) -> PackStats {
        let mut writer = PackWriter::create(path, SHIFT).unwrap();
        for (i, data) in samples().iter().enumerate() {
            let meta = [i as u32, 0, 0, 0, 0, 0, 0];
            let prepared = PreparedEntry::new(EntryKind::TextureLevels, meta, data, level, SHIFT);
            assert_eq!(prepared.raw_for_tests(), *data);
            assert_eq!(writer.add(&prepared).unwrap(), i as u32);
        }
        // A duplicate is stored once.
        let meta = [1, 0, 0, 0, 0, 0, 0];
        let again = PreparedEntry::new(EntryKind::TextureLevels, meta, &samples()[1], level, SHIFT);
        assert_eq!(writer.add(&again).unwrap(), 1);
        // Another chunk size is refused.
        let other = PreparedEntry::new(EntryKind::Mesh, meta, &samples()[1], level, 14);
        assert!(writer.add(&other).is_err());
        writer.finish().unwrap()
    }

    fn load_all(pack: &Pack) -> Result<Vec<Vec<u8>>, PackError> {
        let jobs = ThreadPool::builder().workers(3).build().unwrap();
        let out = Mutex::new(vec![Vec::new(); pack.entries().len()]);
        let indices: Vec<u32> = (0..pack.entries().len() as u32).rev().collect();
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
            assert_eq!(stats.entries, 6);
            assert!(stats.file_bytes.is_multiple_of(BLOCK));
            let pack = Pack::open(&path).unwrap();
            assert_eq!(pack.entries().len(), 6);
            for (i, e) in pack.entries().iter().enumerate() {
                assert_eq!(e.meta[0], i as u32);
                assert!(e.offset.is_multiple_of(BLOCK));
                assert_eq!(
                    e.chunk_count as usize,
                    samples()[i].len().div_ceil(1 << SHIFT)
                );
            }
            // Noise stays raw even when compressing; the mixed entry has chunks of both.
            assert!(pack.entries()[2].is_raw());
            let mixed = pack.chunks(&pack.entries()[5]);
            if level > 0 {
                assert!(stats.stored_bytes < stats.bytes);
                assert!(mixed.iter().any(Chunk::is_raw) && !mixed.iter().all(Chunk::is_raw));
            } else {
                assert!(pack.entries().iter().all(Entry::is_raw));
            }
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
        // An entry's bytes: found when it is loaded, in the chunk hit (entry 1 is
        // compressed, entry 2 raw).
        for entry in [1usize, 2] {
            let e = reopen(&good).unwrap().entries()[entry];
            let mut b = good.clone();
            b[(e.offset + 5) as usize] ^= 1;
            let pack = reopen(&b).unwrap();
            match load_all(&pack) {
                Err(PackError::Invalid(why)) => {
                    assert!(why.contains(&format!("entry {entry}: chunk 0")), "{why}");
                }
                other => panic!("{other:?}"),
            }
        }
        let _ = std::fs::remove_file(path);
    }
}
