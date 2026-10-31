//! The derived data cache (CLAUDE.md §13, ADR 0041): results of expensive import steps
//! (texture compression first) stored under a key that hashes everything they derive
//! from: the source bytes, the settings and the encoder's version. A later import of the
//! same source with the same settings loads the result instead of computing it.
//!
//! Entries are files named by their key in one directory. Each is written to a temporary
//! name and renamed, so a crash never leaves a torn entry and several processes (later the
//! team's shared cache) can use one directory. The entry format is internal to the cache,
//! versioned by the key: it is not the cooked runtime format.
//!
//! A built game carries the entries its assets need as one read-only file, `derived.pack`
//! (ADR 0126's G3a): [`DerivedDataCache::open_pack`] reads it as a cache that has them and
//! stores nothing; a build records which entries its imports used
//! ([`DerivedDataCache::recording`]) and writes them there ([`DerivedDataCache::write_pack`]).
//!
//! ```text
//! "OFDD"  u32 version  u32 entries, each: u128 key, u8 kind bytes, kind, u64 offset,
//!         u64 bytes;  then the entries' bytes
//! ```

use crate::texture_compress::{ENCODER, EncodedTexture, TextureFormat, compress};
use crate::texture_import::{ImageError, TextureUsage, build_mips, decode_image};
use engine_core::hash::{StableHasher, content_hash128, to_hex};
use engine_core::jobs::ThreadPool;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A built game's read-only cache file in its `data/` folder.
pub const DERIVED_PACK: &str = "derived.pack";

const PACK_MAGIC: &[u8; 4] = b"OFDD";
const PACK_VERSION: u32 = 1;

/// An entry's key and kind.
pub type EntryName = (u128, String);

/// Whether a result came from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Loaded from the cache.
    Hit,
    /// Computed (and stored, if the cache could write).
    Miss,
}

/// A derived data cache: a directory, or a built game's read-only pack.
#[derive(Debug, Clone)]
pub struct DerivedDataCache {
    dir: PathBuf,
    /// A pack's entries (the cache is then read-only; `dir` is the pack's path).
    pack: Option<Arc<Pack>>,
    /// The entries read or stored through it, when it records them (a build).
    used: Option<Arc<Mutex<BTreeSet<EntryName>>>>,
}

/// A read-only pack of entries: its file and where each entry is in it.
#[derive(Debug)]
struct Pack {
    file: fs::File,
    entries: HashMap<EntryName, (u64, u64)>,
}

impl DerivedDataCache {
    /// Opens (creates) a cache in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            pack: None,
            used: None,
        })
    }

    /// The pack at `path` as a read-only cache (a built game's `derived.pack`).
    ///
    /// # Errors
    ///
    /// A file that cannot be read, or is not a pack of this version.
    pub fn open_pack(path: impl Into<PathBuf>) -> io::Result<Self> {
        use std::io::Read as _;
        let path = path.into();
        let mut file = fs::File::open(&path)?;
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        let mut head = [0u8; 12];
        file.read_exact(&mut head)?;
        if &head[..4] != PACK_MAGIC {
            return Err(bad("not a derived data pack"));
        }
        let version = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if version != PACK_VERSION {
            return Err(bad("a derived data pack of another version"));
        }
        let count = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
        let mut index = io::BufReader::new(&mut file);
        let mut entries = HashMap::with_capacity(count);
        for _ in 0..count {
            let mut key = [0u8; 17];
            index.read_exact(&mut key)?;
            let mut kind = vec![0u8; usize::from(key[16])];
            index.read_exact(&mut kind)?;
            let mut place = [0u8; 16];
            index.read_exact(&mut place)?;
            let key = u128::from_le_bytes(key[..16].try_into().unwrap_or_default());
            let kind = String::from_utf8(kind).map_err(|_| bad("a kind not in UTF-8"))?;
            let offset = u64::from_le_bytes(place[..8].try_into().unwrap_or_default());
            let bytes = u64::from_le_bytes(place[8..].try_into().unwrap_or_default());
            entries.insert((key, kind), (offset, bytes));
        }
        drop(index);
        Ok(Self {
            dir: path,
            pack: Some(Arc::new(Pack { file, entries })),
            used: None,
        })
    }

    /// The same cache, recording every entry read from it or stored in it: what a build
    /// packs ([`Self::used`], [`Self::write_pack`]).
    pub fn recording(&self) -> Self {
        Self {
            used: Some(Arc::new(Mutex::new(BTreeSet::new()))),
            ..self.clone()
        }
    }

    /// The entries read or stored so far through a recording cache, sorted.
    pub fn used(&self) -> Vec<EntryName> {
        self.used
            .as_ref()
            .and_then(|u| u.lock().ok().map(|u| u.iter().cloned().collect()))
            .unwrap_or_default()
    }

    fn note(&self, key: u128, kind: &str) {
        if let Some(used) = &self.used
            && let Ok(mut used) = used.lock()
        {
            used.insert((key, kind.to_owned()));
        }
    }

    /// Whether it stores nothing (a pack).
    pub fn read_only(&self) -> bool {
        self.pack.is_some()
    }

    /// Entries `entries` of this cache written as a pack at `path` (read-only, one file);
    /// the bytes written. An entry it no longer has is left out.
    ///
    /// # Errors
    ///
    /// The pack not written.
    pub fn write_pack(&self, entries: &[EntryName], path: &Path) -> io::Result<u64> {
        let mut found = Vec::with_capacity(entries.len());
        for (key, kind) in entries {
            if let Some(bytes) = self.get(*key, kind) {
                found.push((*key, kind.as_str(), bytes));
            }
        }
        let index: usize = found.iter().map(|(_, kind, _)| 17 + kind.len() + 16).sum();
        let mut offset = (12 + index) as u64;
        let mut out = Vec::with_capacity(12 + index);
        out.extend_from_slice(PACK_MAGIC);
        out.extend_from_slice(&PACK_VERSION.to_le_bytes());
        out.extend_from_slice(&(found.len() as u32).to_le_bytes());
        for (key, kind, bytes) in &found {
            out.extend_from_slice(&key.to_le_bytes());
            out.push(kind.len() as u8);
            out.extend_from_slice(kind.as_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            offset += bytes.len() as u64;
        }
        for (_, _, bytes) in &found {
            out.extend_from_slice(bytes);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &out)?;
        Ok(out.len() as u64)
    }

    /// The names of the entries in a pack at `path` (a build compares them with what it
    /// would write); `None` without a readable pack there.
    pub fn pack_entries(path: &Path) -> Option<Vec<EntryName>> {
        let pack = Self::open_pack(path).ok()?.pack?;
        let mut names: Vec<EntryName> = pack.entries.keys().cloned().collect();
        names.sort();
        Some(names)
    }

    /// The cache directory (a pack's path for a pack).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: u128, kind: &str) -> PathBuf {
        self.dir.join(format!("{}.{kind}", to_hex(key)))
    }

    /// The entry of `key`, if cached.
    pub fn get(&self, key: u128, kind: &str) -> Option<Vec<u8>> {
        let bytes = match &self.pack {
            Some(pack) => {
                use std::os::windows::fs::FileExt as _;
                let &(offset, len) = pack.entries.get(&(key, kind.to_owned()))?;
                let mut bytes = vec![0u8; usize::try_from(len).ok()?];
                let mut done = 0;
                while done < bytes.len() {
                    let n = pack
                        .file
                        .seek_read(&mut bytes[done..], offset + done as u64)
                        .ok()?;
                    if n == 0 {
                        return None;
                    }
                    done += n;
                }
                bytes
            }
            None => fs::read(self.path(key, kind)).ok()?,
        };
        self.note(key, kind);
        Some(bytes)
    }

    /// Stores an entry (atomically: readers see the old entry or the new one). Writers of
    /// the same entry at once (threads of one process, or processes) each write a
    /// temporary file of their own: the last rename wins, whole. A pack stores nothing.
    pub fn put(&self, key: u128, kind: &str, bytes: &[u8]) -> io::Result<()> {
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if self.pack.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::ReadOnlyFilesystem,
                "a derived data pack is read-only",
            ));
        }
        self.note(key, kind);
        let path = self.path(key, kind);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = path.with_extension(format!("{kind}.tmp{}-{n}", std::process::id()));
        fs::write(&temp, bytes)?;
        fs::rename(&temp, &path)
    }
}

/// Entry kind of encoded textures.
const TEXTURE_KIND: &str = "texture";
/// First bytes of a texture entry; the digit is the entry version.
const TEXTURE_MAGIC: &[u8; 8] = b"ENGTEX01";

/// The cache key of `encoded` (an image file) imported for `usage`.
pub fn texture_key(encoded: &[u8], usage: TextureUsage) -> u128 {
    let mut h = StableHasher::new();
    // 2: the file's bytes by `content_hash128` (ADR 0113).
    h.write_str("engine texture 2");
    h.write_str(ENCODER);
    // Mip filtering is part of the result too.
    h.write_str("kaiser 3 4; linear light; coverage; renormalized");
    match usage {
        TextureUsage::Color => h.write_str("color"),
        TextureUsage::ColorMasked { cutoff } => {
            h.write_str("masked");
            h.write_u64(u64::from(cutoff.to_bits()));
        }
        TextureUsage::Data => h.write_str("data"),
        TextureUsage::Normal => h.write_str("normal"),
    }
    h.write(&content_hash128(encoded).to_le_bytes());
    h.finish()
}

/// Serializes an encoded texture as a cache entry.
pub fn texture_to_bytes(texture: &EncodedTexture) -> Vec<u8> {
    let total: usize = texture.levels.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(24 + 8 * texture.levels.len() + total);
    out.extend_from_slice(TEXTURE_MAGIC);
    for v in [
        texture.format.code(),
        texture.width,
        texture.height,
        texture.levels.len() as u32,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for level in &texture.levels {
        out.extend_from_slice(&(level.len() as u64).to_le_bytes());
    }
    for level in &texture.levels {
        out.extend_from_slice(level);
    }
    out
}

/// Reads a cache entry; `None` if it is not a valid one.
pub fn texture_from_bytes(bytes: &[u8]) -> Option<EncodedTexture> {
    let rest = bytes.strip_prefix(TEXTURE_MAGIC)?;
    let word = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            rest.get(i * 4..i * 4 + 4)?.try_into().ok()?,
        ))
    };
    let format = TextureFormat::from_code(word(0)?)?;
    let (width, height, count) = (word(1)?, word(2)?, word(3)? as usize);
    if width == 0 || height == 0 || count == 0 || count > 32 {
        return None;
    }
    let mut offset = 16 + count * 8;
    let mut levels = Vec::with_capacity(count);
    let (mut w, mut h) = (width, height);
    for k in 0..count {
        let at = 16 + k * 8;
        let len = u64::from_le_bytes(rest.get(at..at + 8)?.try_into().ok()?) as usize;
        if len != format.level_bytes(w, h) {
            return None;
        }
        levels.push(rest.get(offset..offset + len)?.to_vec());
        offset += len;
        (w, h) = ((w / 2).max(1), (h / 2).max(1));
    }
    (offset == rest.len()).then_some(EncodedTexture {
        format,
        width,
        height,
        levels,
    })
}

/// The compressed texture an earlier import of `encoded` (an image file) for `usage` stored
/// in `cache`, if any: [`import_texture`]'s lookup alone, nothing imported on a miss.
pub fn cached_texture(
    cache: &DerivedDataCache,
    encoded: &[u8],
    usage: TextureUsage,
) -> Option<EncodedTexture> {
    cache
        .get(texture_key(encoded, usage), TEXTURE_KIND)
        .and_then(|bytes| texture_from_bytes(&bytes))
}

/// Imports an image file (PNG or JPEG) as a compressed texture for `usage`: decoded, given
/// its mip chain and block-compressed, or loaded from `cache` when an earlier import of the
/// same bytes with the same settings stored it. Work runs on `jobs` when given.
pub fn import_texture(
    encoded: &[u8],
    usage: TextureUsage,
    cache: Option<&DerivedDataCache>,
    jobs: Option<&ThreadPool>,
) -> Result<(EncodedTexture, CacheOutcome), ImageError> {
    engine_profiler::zone!("assets::import_texture");
    let key = cache.map(|_| texture_key(encoded, usage));
    if let (Some(cache), Some(key)) = (cache, key)
        && let Some(texture) = cache
            .get(key, TEXTURE_KIND)
            .and_then(|bytes| texture_from_bytes(&bytes))
    {
        return Ok((texture, CacheOutcome::Hit));
    }
    let image = decode_image(encoded)?;
    let texture = compress(&build_mips(&image, usage), usage, jobs);
    if let (Some(cache), Some(key)) = (cache, key) {
        // A cache that cannot write only costs the next import its time.
        let _ = cache.put(key, TEXTURE_KIND, &texture_to_bytes(&texture));
    }
    Ok((texture, CacheOutcome::Miss))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = (0..width * height)
            .flat_map(|i| [(i * 7) as u8, (i * 3) as u8, (i / 5) as u8, 255])
            .collect();
        writer.write_image_data(&data).unwrap();
        drop(writer);
        bytes
    }

    #[test]
    fn entries_round_trip_and_reject_garbage() {
        let texture = EncodedTexture {
            format: TextureFormat::Bc5Unorm,
            width: 8,
            height: 4,
            levels: vec![vec![1; 32], vec![2; 16], vec![3; 16], vec![4; 16]],
        };
        let bytes = texture_to_bytes(&texture);
        assert_eq!(texture_from_bytes(&bytes), Some(texture));
        assert_eq!(
            texture_from_bytes(&bytes[..bytes.len() - 1]),
            None,
            "truncated"
        );
        assert_eq!(texture_from_bytes(b"ENGTEX02"), None);
        let mut wrong = bytes.clone();
        wrong[8] = 99;
        assert_eq!(texture_from_bytes(&wrong), None, "unknown format");
    }

    #[test]
    fn writers_of_one_entry_at_once_leave_it_whole() {
        // Threads of one process caching the same texture (parallel tests of one scene):
        // each entry read back is one writer's bytes, never a mix.
        let dir = std::env::temp_dir().join(format!("engine_ddc_race_{}", std::process::id()));
        let cache = DerivedDataCache::new(&dir).unwrap();
        std::thread::scope(|scope| {
            for t in 0..8u8 {
                let cache = &cache;
                scope.spawn(move || {
                    for _ in 0..32 {
                        cache.put(7, "texture", &vec![t; 1 << 16]).unwrap();
                        if let Some(bytes) = cache.get(7, "texture") {
                            assert_eq!(bytes.len(), 1 << 16);
                            assert!(bytes.iter().all(|&b| b == bytes[0]), "a mixed entry");
                        }
                    }
                });
            }
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn imports_are_cached_by_content_and_settings() {
        let dir = std::env::temp_dir().join(format!("engine_ddc_test_{}", std::process::id()));
        let cache = DerivedDataCache::new(&dir).unwrap();
        let source = png(16, 16);
        let (first, outcome) =
            import_texture(&source, TextureUsage::Color, Some(&cache), None).unwrap();
        assert_eq!(outcome, CacheOutcome::Miss);
        let (second, outcome) =
            import_texture(&source, TextureUsage::Color, Some(&cache), None).unwrap();
        assert_eq!((outcome, &second), (CacheOutcome::Hit, &first));
        // Other settings, other bytes: other entries.
        let (_, outcome) = import_texture(&source, TextureUsage::Data, Some(&cache), None).unwrap();
        assert_eq!(outcome, CacheOutcome::Miss);
        // The lookup alone finds what an import stored, and nothing else.
        assert_eq!(
            cached_texture(&cache, &source, TextureUsage::Color).as_ref(),
            Some(&first)
        );
        assert_eq!(cached_texture(&cache, &source, TextureUsage::Normal), None);
        let (_, outcome) =
            import_texture(&png(16, 8), TextureUsage::Color, Some(&cache), None).unwrap();
        assert_eq!(outcome, CacheOutcome::Miss);
        assert_ne!(
            texture_key(&source, TextureUsage::ColorMasked { cutoff: 0.5 }),
            texture_key(&source, TextureUsage::ColorMasked { cutoff: 0.25 })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
