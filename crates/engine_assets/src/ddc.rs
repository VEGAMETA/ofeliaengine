//! The derived data cache (CLAUDE.md §13, ADR 0041): results of expensive import steps
//! (texture compression first) stored under a key that hashes everything they derive
//! from: the source bytes, the settings and the encoder's version. A later import of the
//! same source with the same settings loads the result instead of computing it.
//!
//! Entries are files named by their key in one directory. Each is written to a temporary
//! name and renamed, so a crash never leaves a torn entry and several processes (later the
//! team's shared cache) can use one directory. The entry format is internal to the cache,
//! versioned by the key: it is not the cooked runtime format.

use crate::texture_compress::{ENCODER, EncodedTexture, TextureFormat, compress};
use crate::texture_import::{ImageError, TextureUsage, build_mips, decode_image};
use engine_core::hash::{StableHasher, hash128, to_hex};
use engine_core::jobs::ThreadPool;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Whether a result came from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Loaded from the cache.
    Hit,
    /// Computed (and stored, if the cache could write).
    Miss,
}

/// A derived data cache directory.
#[derive(Debug, Clone)]
pub struct DerivedDataCache {
    dir: PathBuf,
}

impl DerivedDataCache {
    /// Opens (creates) a cache in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: u128, kind: &str) -> PathBuf {
        self.dir.join(format!("{}.{kind}", to_hex(key)))
    }

    /// The entry of `key`, if cached.
    pub fn get(&self, key: u128, kind: &str) -> Option<Vec<u8>> {
        fs::read(self.path(key, kind)).ok()
    }

    /// Stores an entry (atomically: readers see the old entry or the new one).
    pub fn put(&self, key: u128, kind: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(key, kind);
        let temp = path.with_extension(format!("{kind}.tmp{}", std::process::id()));
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
    h.write_str("engine texture 1");
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
    h.write(&hash128(encoded).to_le_bytes());
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
