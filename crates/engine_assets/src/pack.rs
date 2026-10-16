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
use engine_core::spsc;
use engine_platform::io::{AlignedBuffer, READ_ALIGN, ReadFile, ReadQueue};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const _: () = assert!(
    cfg!(target_endian = "little"),
    "packs are little endian and read in place"
);

/// Entries start on multiples of this (unbuffered reads).
pub const BLOCK: u64 = READ_ALIGN as u64;
/// First bytes of a pack.
pub const MAGIC: [u8; 4] = *b"OFPK";
/// Version of the layout: the newest this engine writes and reads. A pack is written as
/// version 1 unless it holds entries of a later version (skins and clips: version 2,
/// ADR 0068), so packs without them stay readable by engines that only know version 1;
/// those refuse a version 2 pack instead of misreading it. Version 3 (ADR 0076) adds
/// meshes stored for streaming: the coarsest detail level in the mesh entry, each finer
/// level's extra vertices and indices in a mesh level entry. Version 4 (ADR 0083) adds
/// cooked HDR panoramas (`crate::panorama`). Version 5 (ADR 0092) adds material types (their
/// compiled entry points) and the scene's materials of those types. Version 6 (ADR 0101) adds
/// colliders and the scene's bodies (`crate::colliders`). Version 7 (ADR 0102) adds a
/// locale's localized text (`crate::strings`). Version 8 (ADR 0103) adds the scene's
/// navigation mesh (`crate::navigation`).
pub const VERSION: u32 = 8;
/// Oldest version this engine reads.
pub const MIN_VERSION: u32 = 1;
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
    /// A skin: its skeleton, joints and inverse bind matrices (version 2).
    Skin,
    /// A compressed animation clip of a skin's skeleton (version 2).
    Clip,
    /// A finer detail level of a mesh stored for streaming: the vertices it adds to the
    /// coarser levels' and its indices (version 3, ADR 0076).
    MeshLevel,
    /// A cooked HDR panorama: a BC6H cube with mips and its sun (version 4, ADR 0083;
    /// `crate::panorama`).
    Panorama,
    /// A material type: its names and its fragment entry points' SPIR-V (version 5,
    /// ADR 0092).
    MaterialType,
    /// A collider: its shapes in the engine's form, and in a physics backend's binary form
    /// once baked (version 6, ADR 0101; `crate::colliders`).
    Collider,
    /// The scene's bodies: their colliders, transforms, motions and the instances they
    /// move (version 6, ADR 0101).
    Bodies,
    /// A locale's localized text: `engine_loc::Strings` as bytes (version 7, ADR 0102;
    /// `crate::strings`).
    Strings,
    /// The scene's navigation mesh: Detour's tile blobs (version 8, ADR 0103;
    /// `crate::navigation`).
    NavMesh,
}

impl EntryKind {
    fn code(self) -> u32 {
        match self {
            Self::TextureLevels => 1,
            Self::Mesh => 2,
            Self::Scene => 3,
            Self::Skin => 4,
            Self::Clip => 5,
            Self::MeshLevel => 6,
            Self::Panorama => 7,
            Self::MaterialType => 8,
            Self::Collider => 9,
            Self::Bodies => 10,
            Self::Strings => 11,
            Self::NavMesh => 12,
        }
    }

    /// The first pack version that has this kind.
    pub fn since_version(self) -> u32 {
        match self {
            Self::TextureLevels | Self::Mesh | Self::Scene => 1,
            Self::Skin | Self::Clip => 2,
            Self::MeshLevel => 3,
            Self::Panorama => 4,
            Self::MaterialType => 5,
            Self::Collider | Self::Bodies => 6,
            Self::Strings => 7,
            Self::NavMesh => 8,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::TextureLevels),
            2 => Some(Self::Mesh),
            3 => Some(Self::Scene),
            4 => Some(Self::Skin),
            5 => Some(Self::Clip),
            6 => Some(Self::MeshLevel),
            7 => Some(Self::Panorama),
            8 => Some(Self::MaterialType),
            9 => Some(Self::Collider),
            10 => Some(Self::Bodies),
            11 => Some(Self::Strings),
            12 => Some(Self::NavMesh),
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
    /// The version the entries so far need.
    version: u32,
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
            version: MIN_VERSION,
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
        if let Some(kind) = EntryKind::from_code(prepared.entry.kind) {
            self.version = self.version.max(kind.since_version());
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
            version: self.version,
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
        if !(MIN_VERSION..=VERSION).contains(&header.version) {
            return Err(invalid(format!(
                "version {}, this engine reads {MIN_VERSION} to {VERSION}",
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
            match EntryKind::from_code(e.kind) {
                None => return Err(fail("unknown kind")),
                Some(kind) if kind.since_version() > header.version => {
                    return Err(fail("a kind its version does not have"));
                }
                Some(_) => {}
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
                    match (done.result, done.buffer) {
                        (Ok(read), Some(buffer)) if read as u64 >= e.stored_bytes => {
                            decode(scope, &shared, index, e, self.chunks(e), buffer);
                        }
                        (Ok(_), _) => shared.fail(invalid(format!("entry {index}: truncated"))),
                        (Err(error), _) => shared.fail(error.into()),
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
            let target = (!raw).then(|| {
                // SAFETY: the output holds `e.bytes` bytes and lives as above; this job
                // alone writes `raw_start..raw_start + raw_len`, and nobody reads it before
                // every chunk is done.
                unsafe {
                    core::slice::from_raw_parts_mut(assembly.output.0.add(raw_start), raw_len)
                }
            });
            let checked = chunk_checked(index, k, chunk, stored, target);
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

/// Chunk `k` of entry `index` from its `stored` bytes: decompressed (or copied) into
/// `target` when given, then its raw bytes checked against its hash; without a target the
/// entry is raw and `stored` is checked where it is.
fn chunk_checked(
    index: u32,
    k: usize,
    chunk: &Chunk,
    stored: &[u8],
    target: Option<&mut [u8]>,
) -> Result<(), PackError> {
    let bytes: &[u8] = match target {
        None => stored,
        Some(target) if chunk.is_raw() => {
            if target.len() != stored.len() {
                return Err(invalid(format!(
                    "entry {index}: chunk {k} has the wrong size"
                )));
            }
            target.copy_from_slice(stored);
            target
        }
        Some(target) => {
            let len = target.len();
            match zstd::bulk::decompress_to_buffer(stored, &mut *target) {
                Ok(n) if n == len => target,
                Ok(_) => return Err(invalid(format!("entry {index}: chunk {k} too short"))),
                Err(why) => return Err(invalid(format!("entry {index}: chunk {k}: {why}"))),
            }
        }
    };
    if state_hash64(bytes) == chunk.hash {
        Ok(())
    } else {
        Err(invalid(format!(
            "entry {index}: chunk {k} damaged (hash mismatch)"
        )))
    }
}

/// Streams entries of a pack into memory of the caller's (staging memory the GPU copies
/// from), without blocking a frame on the drive (ADR 0057). Each [`Self::update`] starts
/// the reads requested since the last, and checks and decompresses the entries whose reads
/// have finished, their chunks in parallel on the job pool.
///
/// - Raw entries are read straight into their target and checked there.
/// - Compressed entries are read into a buffer of the streamer's, then decompressed into
///   their target.
pub struct PackStreamer {
    pack: Pack,
    queue: ReadQueue,
    reads: Vec<Option<StreamRead>>,
    free: Vec<usize>,
    /// Read buffers of compressed entries, kept for the next reads.
    buffers: Vec<AlignedBuffer>,
    /// Scratch of `update`, kept to avoid allocating per frame.
    arrived: Vec<(usize, io::Result<usize>, Option<AlignedBuffer>)>,
    work: Vec<(usize, usize)>,
}

impl core::fmt::Debug for PackStreamer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PackStreamer")
            .field("entries", &self.pack.entries.len())
            .field("reading", &self.pending())
            .finish_non_exhaustive()
    }
}

/// An entry being streamed.
struct StreamRead {
    index: u32,
    tag: u64,
    target: Address,
}

/// A finished streamed entry: its tag and whether it is in its target, checked.
#[derive(Debug)]
pub struct Streamed {
    /// The tag given to [`PackStreamer::request`].
    pub tag: u64,
    /// The entry's index.
    pub index: u32,
    /// Ok when the target holds the entry's raw bytes, checked.
    pub result: Result<(), PackError>,
}

impl PackStreamer {
    /// Streams from `pack`, up to `depth` entries at a time.
    pub fn new(pack: Pack, depth: usize) -> Result<Self, PackError> {
        let depth = depth.clamp(1, 1024);
        Ok(Self {
            pack,
            queue: ReadQueue::new(depth)?,
            reads: (0..depth).map(|_| None).collect(),
            free: (0..depth).rev().collect(),
            buffers: Vec::new(),
            arrived: Vec::with_capacity(depth),
            work: Vec::new(),
        })
    }

    /// The pack.
    pub fn pack(&self) -> &Pack {
        &self.pack
    }

    /// Entries requested and not yet finished.
    pub fn pending(&self) -> usize {
        self.reads.len() - self.free.len()
    }

    /// Whether another entry can be requested now.
    pub fn has_room(&self) -> bool {
        !self.free.is_empty()
    }

    /// Bytes an entry's target needs: its raw bytes, and for raw entries (read straight
    /// into it) their stored bytes rounded up to a block.
    pub fn target_bytes(entry: &Entry) -> usize {
        if entry.is_raw() {
            entry.stored_bytes.next_multiple_of(BLOCK) as usize
        } else {
            entry.bytes as usize
        }
    }

    /// Requests entry `index` into `target..target + len`, tagged `tag`; it arrives in a
    /// later [`Self::update`]. `target` is a multiple of [`BLOCK`] and `len` at least
    /// [`Self::target_bytes`].
    ///
    /// # Safety
    ///
    /// The target memory is writable, and nothing else reads, writes or frees it until the
    /// entry has come out of [`Self::update`] (or the streamer is dropped).
    pub unsafe fn request(
        &mut self,
        index: u32,
        target: *mut u8,
        len: usize,
        tag: u64,
    ) -> Result<(), PackError> {
        let e = *self
            .pack
            .entries
            .get(index as usize)
            .ok_or_else(|| invalid(format!("no entry {index}")))?;
        if !(target as usize).is_multiple_of(BLOCK as usize) || len < Self::target_bytes(&e) {
            return Err(invalid("stream target misaligned or too small"));
        }
        let Some(slot) = self.free.pop() else {
            return Err(PackError::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "streamer full",
            )));
        };
        let pushed = if e.is_raw() {
            let padded = e.stored_bytes.next_multiple_of(BLOCK) as usize;
            if padded == 0 {
                // An empty entry: nothing to read.
                Ok(())
            } else {
                // SAFETY: the caller's contract covers `target..target + len`, and
                // `padded <= len`.
                unsafe {
                    self.queue
                        .push_into(&self.pack.file, e.offset, target, padded, slot as u64)
                }
                .map_err(PackError::from)
            }
        } else {
            let buffer = match self
                .buffers
                .iter()
                .position(|b| b.len() as u64 >= e.stored_bytes)
            {
                Some(k) => self.buffers.swap_remove(k),
                None => AlignedBuffer::new(e.stored_bytes as usize),
            };
            self.queue
                .push(&self.pack.file, e.offset, buffer, slot as u64)
                .map_err(|refused| {
                    if let Some(buffer) = refused.buffer {
                        self.buffers.push(buffer);
                    }
                    PackError::from(refused.error)
                })
        };
        if let Err(error) = pushed {
            self.free.push(slot);
            return Err(error);
        }
        self.reads[slot] = Some(StreamRead {
            index,
            tag,
            target: Address(target),
        });
        if e.is_raw() && e.stored_bytes == 0 {
            // Nothing to read: finished at the next update.
            self.arrived.push((slot, Ok(0), None));
        }
        Ok(())
    }

    /// Starts requested reads, then checks and decompresses every entry whose read has
    /// finished (on `jobs`, blocking until that work is done) and appends them to
    /// `finished`. With `wait`, first waits for at least one read if none has finished.
    pub fn update(
        &mut self,
        jobs: &ThreadPool,
        wait: bool,
        finished: &mut Vec<Streamed>,
    ) -> Result<(), PackError> {
        engine_profiler::zone!("assets::PackStreamer::update");
        self.queue.submit(wait && self.arrived.is_empty())?;
        while let Some(done) = self.queue.pop() {
            self.arrived
                .push((done.tag as usize, done.result, done.buffer));
        }
        if self.arrived.is_empty() {
            return Ok(());
        }
        // One job per chunk of every arrived entry.
        let chunk_bytes = 1usize << self.pack.chunk_shift;
        self.work.clear();
        let mut failures: Vec<Option<PackError>> = Vec::with_capacity(self.arrived.len());
        for (a, (slot, result, _)) in self.arrived.iter_mut().enumerate() {
            let read = self.reads[*slot].as_ref();
            let failure = match (read, result) {
                (None, _) => Some(invalid("a read finished that was not requested")),
                (Some(_), Err(error)) => Some(PackError::Io(core::mem::replace(
                    error,
                    io::Error::other("taken"),
                ))),
                (Some(r), Ok(n)) => {
                    let e = &self.pack.entries[r.index as usize];
                    if (*n as u64) < e.stored_bytes {
                        Some(invalid(format!("entry {}: truncated", r.index)))
                    } else {
                        self.work
                            .extend((0..e.chunk_count as usize).map(|k| (a, k)));
                        None
                    }
                }
            };
            failures.push(failure);
        }
        let errors: Vec<OnceLock<PackError>> =
            (0..self.arrived.len()).map(|_| OnceLock::new()).collect();
        {
            let (pack, reads, arrived, work) = (&self.pack, &self.reads, &self.arrived, &self.work);
            jobs.parallel_for(work.len(), 1, |_, range| {
                for w in range {
                    let (a, k) = work[w];
                    let (slot, _, buffer) = &arrived[a];
                    let Some(r) = reads[*slot].as_ref() else {
                        continue;
                    };
                    let e = &pack.entries[r.index as usize];
                    let chunk = &pack.chunks(e)[k];
                    let raw_start = k * chunk_bytes;
                    let raw_len = (e.bytes as usize - raw_start).min(chunk_bytes);
                    let result = if e.is_raw() {
                        // SAFETY: the target holds the read (the request's contract, and
                        // the read has finished); this job only reads it.
                        let stored = unsafe {
                            core::slice::from_raw_parts(
                                r.target.0.add(chunk.offset as usize),
                                chunk.stored_bytes() as usize,
                            )
                        };
                        chunk_checked(r.index, k, chunk, stored, None)
                    } else {
                        let stored = buffer.as_ref().map(|b| {
                            &b.as_slice()[chunk.offset as usize..][..chunk.stored_bytes() as usize]
                        });
                        // SAFETY: `raw_start + raw_len <= e.bytes <= target_len` (checked
                        // at the request); this job alone writes that range of the target.
                        let target = unsafe {
                            core::slice::from_raw_parts_mut(r.target.0.add(raw_start), raw_len)
                        };
                        match stored {
                            Some(stored) => chunk_checked(r.index, k, chunk, stored, Some(target)),
                            None => Err(invalid("compressed entry without its read buffer")),
                        }
                    };
                    if let Err(error) = result {
                        let _ = errors[a].set(error);
                    }
                }
            });
        }
        for ((slot, _, buffer), (failure, error)) in
            self.arrived.drain(..).zip(failures.into_iter().zip(errors))
        {
            if let Some(buffer) = buffer {
                self.buffers.push(buffer);
            }
            let Some(read) = self.reads[slot].take() else {
                continue;
            };
            self.free.push(slot);
            finished.push(Streamed {
                tag: read.tag,
                index: read.index,
                result: match failure.or(error.into_inner()) {
                    Some(error) => Err(error),
                    None => Ok(()),
                },
            });
        }
        Ok(())
    }
}

/// Work run on the streaming thread on each entry that arrived ([`StreamThread::spawn_with`]):
/// the entry and its whole target, which it may rewrite.
pub type PostProcess = Box<dyn Fn(&Entry, &mut [u8]) -> Result<(), PackError> + Send>;

/// A request to [`StreamThread`].
struct ThreadRequest {
    index: u32,
    target: Address,
    len: usize,
    tag: u64,
}

/// A [`PackStreamer`] on a thread of its own (ADR 0057): the frame's thread only queues
/// requests and collects finished entries, through lock-free queues and without
/// allocating. The streaming thread waits for the drive and decompresses on a job pool of
/// its own, at below-normal priority, so a frame never waits for either (measured on
/// NVMe: decompressing inside the frame's update took up to 6 ms of a frame).
pub struct StreamThread {
    entries: Vec<Entry>,
    requests: spsc::Producer<ThreadRequest>,
    finished: spsc::Consumer<Streamed>,
    /// Requested and not yet collected.
    in_flight: usize,
    capacity: usize,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl core::fmt::Debug for StreamThread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamThread")
            .field("entries", &self.entries.len())
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl StreamThread {
    /// Starts streaming from `pack`: up to `depth` reads in flight, up to `capacity`
    /// entries requested at a time, decompression on `workers` threads (below-normal
    /// priority).
    pub fn spawn(
        pack: Pack,
        depth: usize,
        capacity: usize,
        workers: usize,
    ) -> Result<Self, PackError> {
        Self::spawn_with(pack, depth, capacity, workers, None)
    }

    /// [`Self::spawn`], with `post` run on the streaming thread on every entry that
    /// arrived checked, over its whole target (the entry's raw bytes first), before it is
    /// handed back: work the frame's thread should not do (decoding into the form the GPU
    /// takes, ADR 0076). An error from it fails the entry.
    pub fn spawn_with(
        pack: Pack,
        depth: usize,
        capacity: usize,
        workers: usize,
        post: Option<PostProcess>,
    ) -> Result<Self, PackError> {
        let capacity = capacity.max(1);
        let entries = pack.entries().to_vec();
        let mut streamer = PackStreamer::new(pack, depth)?;
        let jobs = ThreadPool::builder()
            .workers(workers.max(1))
            .thread_name("engine-stream")
            .on_thread_start(|_| lower_priority())
            .build()
            .map_err(|e| PackError::Io(io::Error::other(format!("{e:?}"))))?;
        let (requests, mut incoming) = spsc::channel::<ThreadRequest>(capacity);
        let (mut outgoing, finished) = spsc::channel::<Streamed>(capacity);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name(String::from("engine-stream"))
            .spawn(move || {
                lower_priority();
                let mut done = Vec::with_capacity(capacity);
                let mut waiting: Option<ThreadRequest> = None;
                // Targets by tag, for `post`.
                let mut targets: Vec<(u64, Address, usize)> = Vec::with_capacity(capacity);
                loop {
                    // New requests, as many as the streamer takes.
                    while streamer.has_room() {
                        let Some(r) = waiting.take().or_else(|| incoming.pop()) else {
                            break;
                        };
                        // SAFETY: the frame's thread promised the target under
                        // `StreamThread::request`'s contract, which is the streamer's.
                        let requested =
                            unsafe { streamer.request(r.index, r.target.0, r.len, r.tag) };
                        if requested.is_ok() && post.is_some() {
                            targets.push((r.tag, Address(r.target.0), r.len));
                        }
                        if let Err(error) = requested {
                            done.push(Streamed {
                                tag: r.tag,
                                index: r.index,
                                result: Err(error),
                            });
                        }
                    }
                    if streamer.pending() == 0 && done.is_empty() {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        // Nothing to do until a request arrives (`request` unparks).
                        match incoming.pop() {
                            Some(r) => waiting = Some(r),
                            None => std::thread::park(),
                        }
                        continue;
                    }
                    if streamer.pending() > 0
                        && let Err(error) = streamer.update(&jobs, true, &mut done)
                    {
                        // The queue itself failed: every entry in flight fails with it.
                        done.push(Streamed {
                            tag: u64::MAX,
                            index: u32::MAX,
                            result: Err(error),
                        });
                        break;
                    }
                    for streamed in done.drain(..) {
                        let mut streamed = streamed;
                        if let Some(post) = post.as_ref()
                            && let Some(k) = targets.iter().position(|t| t.0 == streamed.tag)
                        {
                            let (_, target, len) = targets.swap_remove(k);
                            if streamed.result.is_ok()
                                && let Some(entry) =
                                    streamer.pack().entries.get(streamed.index as usize)
                            {
                                // SAFETY: the target is this entry's until it is handed
                                // back below (`request`'s contract), and holds `len` bytes.
                                let bytes =
                                    unsafe { core::slice::from_raw_parts_mut(target.0, len) };
                                streamed.result = post(entry, bytes);
                            }
                        }
                        // Never full: at most `capacity` entries are in flight.
                        while let Err(back) = outgoing.push(streamed) {
                            streamed = back;
                            std::thread::yield_now();
                        }
                    }
                }
            })
            .map_err(PackError::Io)?;
        Ok(Self {
            entries,
            requests,
            finished,
            in_flight: 0,
            capacity,
            stop,
            thread: Some(thread),
        })
    }

    /// The pack's table of contents.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Entries requested and not yet collected.
    pub fn pending(&self) -> usize {
        self.in_flight
    }

    /// Whether another entry can be requested now.
    pub fn has_room(&self) -> bool {
        self.in_flight < self.capacity
    }

    /// Requests entry `index` into `target..target + len`, tagged `tag`, as
    /// [`PackStreamer::request`] (a failure comes back as a finished entry).
    ///
    /// # Safety
    ///
    /// The target memory is writable, and nothing else reads, writes or frees it until the
    /// entry has come out of [`Self::poll`] (or this is dropped).
    pub unsafe fn request(
        &mut self,
        index: u32,
        target: *mut u8,
        len: usize,
        tag: u64,
    ) -> Result<(), PackError> {
        if !self.has_room() {
            return Err(PackError::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "streaming thread full",
            )));
        }
        let request = ThreadRequest {
            index,
            target: Address(target),
            len,
            tag,
        };
        if self.requests.push(request).is_err() {
            return Err(PackError::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "streaming thread full",
            )));
        }
        self.in_flight += 1;
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
        Ok(())
    }

    /// Appends the entries finished since the last call to `finished`. An entry tagged
    /// `u64::MAX` (index `u32::MAX`) means the streaming thread's read queue failed:
    /// nothing more will finish.
    pub fn poll(&mut self, finished: &mut Vec<Streamed>) {
        while let Some(streamed) = self.finished.pop() {
            self.in_flight = self.in_flight.saturating_sub(1);
            finished.push(streamed);
        }
    }
}

impl Drop for StreamThread {
    fn drop(&mut self) {
        // The thread finishes what is in flight (targets stay valid until then, by
        // `request`'s contract) and leaves.
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// Streaming works below normal priority: frames come first.
fn lower_priority() {
    #[cfg(windows)]
    let _ = engine_platform::thread::set_current_thread_priority(
        engine_platform::thread::ThreadPriority::BelowNormal,
    );
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
            return match done.buffer {
                Some(buffer) if read >= len => Ok(buffer),
                _ => Err(invalid("truncated")),
            };
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
    fn entries_stream_into_memory_of_the_callers() {
        for level in [0, 3] {
            let path = temp(&format!("stream_{level}"));
            write(&path, level);
            let jobs = ThreadPool::builder().workers(3).build().unwrap();
            let mut streamer = PackStreamer::new(Pack::open(&path).unwrap(), 2).unwrap();
            let entries = streamer.pack().entries().to_vec();
            let mut targets: Vec<AlignedBuffer> = entries
                .iter()
                .map(|e| AlignedBuffer::new(PackStreamer::target_bytes(e)))
                .collect();
            let mut next = 0;
            let mut finished = Vec::new();
            while next < entries.len() || streamer.pending() > 0 {
                while next < entries.len() && streamer.has_room() {
                    let target = &mut targets[next];
                    // SAFETY: each target is used by its own entry only and outlives the
                    // streamer's use of it (finished below before the targets are read).
                    unsafe {
                        streamer
                            .request(
                                next as u32,
                                target.as_mut_slice().as_mut_ptr(),
                                target.len(),
                                100 + next as u64,
                            )
                            .unwrap();
                    }
                    next += 1;
                }
                streamer.update(&jobs, true, &mut finished).unwrap();
            }
            assert_eq!(finished.len(), entries.len());
            for f in &finished {
                assert!(f.result.is_ok(), "{:?}", f.result);
                assert_eq!(f.tag, 100 + u64::from(f.index));
            }
            for (i, sample) in samples().iter().enumerate() {
                assert_eq!(
                    targets[i].as_slice()[..sample.len()],
                    sample[..],
                    "entry {i}"
                );
            }
            drop(streamer);
            // A damaged chunk fails its entry only.
            let e = Pack::open(&path).unwrap().entries()[1];
            let mut bytes = std::fs::read(&path).unwrap();
            bytes[(e.offset + 5) as usize] ^= 1;
            std::fs::write(&path, bytes).unwrap();
            let mut streamer = PackStreamer::new(Pack::open(&path).unwrap(), 4).unwrap();
            let mut finished = Vec::new();
            for i in [0u32, 1] {
                let target = &mut targets[i as usize];
                // SAFETY: as above.
                unsafe {
                    streamer
                        .request(i, target.as_mut_slice().as_mut_ptr(), target.len(), 0)
                        .unwrap();
                }
            }
            while streamer.pending() > 0 {
                streamer.update(&jobs, true, &mut finished).unwrap();
            }
            finished.sort_by_key(|f| f.index);
            assert!(finished[0].result.is_ok());
            assert!(finished[1].result.is_err());
            drop(streamer);
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn a_streaming_thread_delivers_every_entry() {
        let path = temp("thread");
        write(&path, 3);
        let pack = Pack::open(&path).unwrap();
        let entries = pack.entries().to_vec();
        let mut targets: Vec<AlignedBuffer> = entries
            .iter()
            .map(|e| AlignedBuffer::new(PackStreamer::target_bytes(e)))
            .collect();
        let mut thread = StreamThread::spawn(pack, 2, 3, 2).unwrap();
        let (mut next, mut finished) = (0, Vec::new());
        while next < entries.len() || thread.pending() > 0 {
            while next < entries.len() && thread.has_room() {
                let target = &mut targets[next];
                // SAFETY: each target is used by its own entry only and outlives the
                // thread's use of it.
                unsafe {
                    thread
                        .request(
                            next as u32,
                            target.as_mut_slice().as_mut_ptr(),
                            target.len(),
                            next as u64,
                        )
                        .unwrap();
                }
                next += 1;
            }
            thread.poll(&mut finished);
            std::thread::yield_now();
        }
        assert_eq!(finished.len(), entries.len());
        assert!(
            finished
                .iter()
                .all(|f| f.result.is_ok() && f.tag == u64::from(f.index))
        );
        for (i, sample) in samples().iter().enumerate() {
            assert_eq!(
                targets[i].as_slice()[..sample.len()],
                sample[..],
                "entry {i}"
            );
        }
        // A misaligned target fails that entry; dropping waits for what is in flight.
        let mut odd = AlignedBuffer::new(8192);
        // SAFETY: as above.
        unsafe {
            let address = odd.as_mut_slice().as_mut_ptr().add(1);
            thread.request(1, address, 4096, 7).unwrap();
        }
        let mut failed = Vec::new();
        while failed.is_empty() {
            thread.poll(&mut failed);
            std::thread::yield_now();
        }
        assert!(failed[0].result.is_err());
        let first = &mut targets[0];
        // SAFETY: as above.
        unsafe {
            thread
                .request(0, first.as_mut_slice().as_mut_ptr(), first.len(), 0)
                .unwrap();
        }
        drop(thread);
        let _ = std::fs::remove_file(path);
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
