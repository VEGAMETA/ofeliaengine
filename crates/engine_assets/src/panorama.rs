//! Panoramas cooked for the GPU (ADR 0083, phase 2): an HDR panorama
//! ([`crate::hdr::HdrImage`]) as a cube map with mips, block-compressed as BC6H (unsigned
//! half floats in 4 × 4 blocks of 16 bytes: an eighth of RGBA16F's memory), with its sun;
//! kept in the derived data cache ([`import_panorama`]) and in packs
//! ([`write_panorama_pack`], [`load_panorama_pack`]).
//!
//! - **The cube** holds the panorama in its own frame (faces in Vulkan's order: +X, −X, +Y,
//!   −Y, +Z, −Z; the renderer turns the directions it samples it with). Each texel is the
//!   mean of 2 × 2 bilinear samples of the panorama, as the renderer's `panoramaCube` makes
//!   it on the GPU; each mip the mean of the 2 × 2 texels above.
//! - **BC6H** by block_compression's port of Intel's ISPC kernels ("basic" quality), on the
//!   job system by face and rows of blocks. Values above half floats' 65,504 saturate: the
//!   sun's light, taken out by [`find_sun`], is not lost.

use crate::ddc::{CacheOutcome, DerivedDataCache};
use crate::hdr::{HdrError, HdrImage, PanoramaSun, SunSearch, decode_panorama, find_sun};
use crate::pack::{EntryKind, Pack, PackError, PackStats, PackWriter, PreparedEntry, invalid};
use block_compression::encode::compress_rgba16;
use block_compression::half::f16;
use block_compression::{BC6HSettings, CompressionVariant};
use engine_core::hash::{StableHasher, hash128};
use engine_core::jobs::ThreadPool;
use engine_core::math::Vec3;
use std::path::Path;
use std::sync::Mutex;

/// The largest face a cooked cube gets from its panorama's width
/// ([`PanoramaCookSettings::size`] 0): an 8K panorama's resolution.
pub const MAX_COOKED_SIZE: u32 = 2048;
/// Samples of the panorama per side of a texel of the cube's level 0.
const SAMPLES: u32 = 2;
/// The encoder and its settings, for cache keys.
const ENCODER: &str = "block_compression 0.10 bc6h basic";
/// Rows of texels (or of blocks) per job.
const ROWS_PER_JOB: usize = 16;

/// How [`cook_panorama`] cooks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanoramaCookSettings {
    /// Face size of the cube's level 0; 0: a quarter of the panorama's width, rounded up to
    /// a power of two, at most [`MAX_COOKED_SIZE`].
    pub size: u32,
    /// How its sun is found; `None`: none is taken out (the lighting keeps all of it).
    pub sun: Option<SunSearch>,
}

impl Default for PanoramaCookSettings {
    fn default() -> Self {
        Self {
            size: 0,
            sun: Some(SunSearch::default()),
        }
    }
}

/// A panorama cooked for the GPU (see the module documentation).
#[derive(Debug, Clone, PartialEq)]
pub struct CookedPanorama {
    /// Face size of level 0 (a power of two).
    pub size: u32,
    /// Levels from the full size down to 1 × 1: each level's six faces of BC6H blocks
    /// (unsigned), face after face (+X, −X, +Y, −Y, +Z, −Z), rows of blocks top to bottom.
    pub levels: Vec<Vec<u8>>,
    /// Its sun, in the panorama's frame.
    pub sun: Option<PanoramaSun>,
}

impl CookedPanorama {
    /// Bytes of one face of level `level` of a cube of face `size`: whole 4 × 4 blocks.
    pub fn face_bytes(size: u32, level: u32) -> usize {
        let s = (size >> level).max(1);
        (s.div_ceil(4) * s.div_ceil(4) * 16) as usize
    }

    /// Levels of a cube of face `size`, down to 1 × 1.
    pub fn level_count(size: u32) -> u32 {
        size.max(1).ilog2() + 1
    }

    /// Bytes of every level.
    pub fn bytes(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }

    /// Serialized (the derived data cache's entries and the packs' `Panorama` entries):
    /// [`MAGIC`], face size, level count, whether it has a sun, then 12 floats of the sun
    /// (direction, illuminance, cone, sky, two zeros), then the levels.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.bytes());
        out.extend_from_slice(MAGIC);
        for v in [
            self.size,
            self.levels.len() as u32,
            u32::from(self.sun.is_some()),
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let sun = self.sun.map_or([0.0; 12], |s| {
            let (d, l, k) = (s.direction, s.illuminance, s.sky);
            [
                d.x, d.y, d.z, l.x, l.y, l.z, s.cone, k.x, k.y, k.z, 0.0, 0.0,
            ]
        });
        for v in sun {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for level in &self.levels {
            out.extend_from_slice(level);
        }
        out
    }

    /// Reads [`Self::to_bytes`]'s output; `None` if it is not a valid one.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(MAGIC)?;
        let word = |i: usize| -> Option<[u8; 4]> { rest.get(i * 4..i * 4 + 4)?.try_into().ok() };
        let (size, count, has_sun) = (
            u32::from_le_bytes(word(0)?),
            u32::from_le_bytes(word(1)?),
            u32::from_le_bytes(word(2)?),
        );
        if !size.is_power_of_two() || size > 1 << 14 || count != Self::level_count(size) {
            return None;
        }
        let float = |i: usize| -> Option<f32> { Some(f32::from_le_bytes(word(3 + i)?)) };
        let sun = match has_sun {
            0 => None,
            1 => Some(PanoramaSun {
                direction: Vec3::new(float(0)?, float(1)?, float(2)?),
                illuminance: Vec3::new(float(3)?, float(4)?, float(5)?),
                cone: float(6)?,
                sky: Vec3::new(float(7)?, float(8)?, float(9)?),
            }),
            _ => return None,
        };
        let mut at = 15 * 4;
        let mut levels = Vec::with_capacity(count as usize);
        for level in 0..count {
            let len = 6 * Self::face_bytes(size, level);
            levels.push(rest.get(at..at + len)?.to_vec());
            at += len;
        }
        (at == rest.len()).then_some(Self { size, levels, sun })
    }
}

/// First bytes of a serialized panorama; the digits are its version.
const MAGIC: &[u8; 8] = b"ENGPAN01";

/// Runs `f(first_item, items)` over `out` cut into chunks of `chunk` items, in parallel on
/// `jobs` when given (each chunk is written by one job).
fn for_chunks<T: Send>(
    jobs: Option<&ThreadPool>,
    out: &mut [T],
    chunk: usize,
    f: impl Fn(usize, &mut [T]) + Sync,
) {
    let chunk = chunk.max(1);
    match jobs {
        Some(jobs) if out.len() > chunk => {
            let pieces: Vec<Mutex<&mut [T]>> = out.chunks_mut(chunk).map(Mutex::new).collect();
            jobs.parallel_for(pieces.len(), 1, |k, _| {
                let mut piece = pieces[k].lock().unwrap_or_else(|e| e.into_inner());
                f(k * chunk, &mut piece);
            });
        }
        _ => {
            for (k, piece) in out.chunks_mut(chunk).enumerate() {
                f(k * chunk, piece);
            }
        }
    }
}

/// The direction through point `(u, v)` (−1..1, v down) of cube face `face` (Vulkan's
/// convention, as `cube_common.slang`).
fn face_direction(face: usize, u: f32, v: f32) -> Vec3 {
    match face {
        0 => Vec3::new(1.0, -v, -u),
        1 => Vec3::new(-1.0, -v, u),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, -v),
        4 => Vec3::new(u, -v, 1.0),
        _ => Vec3::new(-u, -v, -1.0),
    }
    .normalize()
}

/// The panorama's radiance along `p` (its own frame), its texels filtered bilinearly,
/// wrapping around in u and clamped at the poles (as `panorama.slang`).
fn sample(image: &HdrImage, scale: &[f32; 256], p: Vec3) -> Vec3 {
    let [u, v] = crate::hdr::panorama_uv(p);
    let (w, h) = (image.width as i64, image.height as i64);
    let (x, y) = (u * w as f32 - 0.5, v * h as f32 - 0.5);
    let (bx, by) = (x.floor(), y.floor());
    let (fx, fy) = (x - bx, y - by);
    let (x0, y0) = (bx as i64, by as i64);
    let column = |x: i64| x.rem_euclid(w) as usize;
    let row = |y: i64| y.clamp(0, h - 1) as usize * w as usize;
    let texel = |x: i64, y: i64| {
        let p = image.pixels[row(y) + column(x)];
        let s = scale[usize::from(p[3])];
        Vec3::new(
            (f32::from(p[0]) + 0.5) * s,
            (f32::from(p[1]) + 0.5) * s,
            (f32::from(p[2]) + 0.5) * s,
        )
    };
    let top = texel(x0, y0).lerp(texel(x0 + 1, y0), fx);
    let bottom = texel(x0, y0 + 1).lerp(texel(x0 + 1, y0 + 1), fx);
    top.lerp(bottom, fy)
}

/// Cooks a panorama: its sun found ([`PanoramaCookSettings::sun`]), its cube made with
/// every mip and compressed as BC6H, on `jobs` when given.
///
/// # Panics
///
/// If `image.pixels` does not hold `width × height` pixels, or the image is empty.
pub fn cook_panorama(
    image: &HdrImage,
    settings: &PanoramaCookSettings,
    jobs: Option<&ThreadPool>,
) -> CookedPanorama {
    engine_profiler::zone!("assets::cook_panorama");
    assert!(
        image.width > 0
            && image.height > 0
            && image.pixels.len() == image.width as usize * image.height as usize,
        "a panorama of {} × {} pixels with {} of them",
        image.width,
        image.height,
        image.pixels.len()
    );
    let size = match settings.size {
        0 => image
            .width
            .div_ceil(4)
            .next_power_of_two()
            .clamp(4, MAX_COOKED_SIZE),
        size => size.next_power_of_two(),
    };
    let sun = settings.sun.and_then(|search| find_sun(image, &search));
    let mut level = cube(image, size, jobs);
    let mut levels = Vec::with_capacity(CookedPanorama::level_count(size) as usize);
    let mut s = size;
    loop {
        levels.push(compress_level(&level, s, jobs));
        if s == 1 {
            break;
        }
        level = next_level(&level, s);
        s /= 2;
    }
    CookedPanorama { size, levels, sun }
}

/// Level 0 of the cube (six faces of `size²` texels, rows top to bottom): SAMPLES²
/// bilinear samples of the panorama per texel.
fn cube(image: &HdrImage, size: u32, jobs: Option<&ThreadPool>) -> Vec<Vec3> {
    let scale: [f32; 256] = core::array::from_fn(|e| {
        if e == 0 {
            0.0
        } else {
            f32::powi(2.0, e as i32 - 136)
        }
    });
    let face = (size * size) as usize;
    let mut level = vec![Vec3::ZERO; 6 * face];
    for_chunks(
        jobs,
        &mut level,
        ROWS_PER_JOB * size as usize,
        |first, texels| {
            for (i, texel) in texels.iter_mut().enumerate() {
                let at = first + i;
                let (f, rest) = (at / face, at % face);
                let (x, y) = ((rest % size as usize) as f32, (rest / size as usize) as f32);
                let mut sum = Vec3::ZERO;
                for k in 0..SAMPLES * SAMPLES {
                    let ox = ((k % SAMPLES) as f32 + 0.5) / SAMPLES as f32;
                    let oy = ((k / SAMPLES) as f32 + 0.5) / SAMPLES as f32;
                    let u = (x + ox) / size as f32 * 2.0 - 1.0;
                    let v = (y + oy) / size as f32 * 2.0 - 1.0;
                    sum += sample(image, &scale, face_direction(f, u, v));
                }
                *texel = sum / (SAMPLES * SAMPLES) as f32;
            }
        },
    );
    level
}

/// The level below `level` (faces of `size²` texels): the means of 2 × 2 texels.
fn next_level(level: &[Vec3], size: u32) -> Vec<Vec3> {
    let half = (size / 2) as usize;
    let size = size as usize;
    let mut next = vec![Vec3::ZERO; 6 * half * half];
    for (i, texel) in next.iter_mut().enumerate() {
        let (f, rest) = (i / (half * half), i % (half * half));
        let (x, y) = (rest % half * 2, rest / half * 2);
        let at = |x: usize, y: usize| level[f * size * size + y * size + x];
        *texel = (at(x, y) + at(x + 1, y) + at(x, y + 1) + at(x + 1, y + 1)) * 0.25;
    }
    next
}

/// One level's six faces (`size²` texels each) as BC6H blocks, faces padded to whole
/// blocks by repeating their edge texels.
fn compress_level(level: &[Vec3], size: u32, jobs: Option<&ThreadPool>) -> Vec<u8> {
    let padded = size.max(4);
    let face_bytes = CookedPanorama::face_bytes(size, 0);
    let row_bytes = (padded / 4 * 16) as usize;
    let variant = CompressionVariant::BC6H(BC6HSettings::basic());
    let mut out = vec![0u8; 6 * face_bytes];
    // One job per face's rows of blocks: chunks of ROWS_PER_JOB block rows.
    let rows_per_face = (padded / 4) as usize;
    let chunk = row_bytes * ROWS_PER_JOB.min(rows_per_face);
    for_chunks(jobs, &mut out, chunk, |first, blocks| {
        let (f, row) = (first / face_bytes, first % face_bytes / row_bytes);
        let rows = blocks.len() / row_bytes;
        let mut texels = vec![f16::ZERO; (padded * 4) as usize * 4 * rows];
        for (i, texel) in texels.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % padded as usize, row * 4 + i / padded as usize);
            let c = level[f * (size * size) as usize
                + y.min(size as usize - 1) * size as usize
                + x.min(size as usize - 1)];
            texel[0] = f16::from_f32(c.x.min(65_504.0));
            texel[1] = f16::from_f32(c.y.min(65_504.0));
            texel[2] = f16::from_f32(c.z.min(65_504.0));
            texel[3] = f16::ONE;
        }
        compress_rgba16(
            variant,
            &texels,
            blocks,
            padded,
            (rows * 4) as u32,
            padded * 4,
        );
    });
    out
}

/// The cache key of a panorama file's bytes (`.hdr` or OpenEXR) cooked with `settings`.
pub fn panorama_key(encoded: &[u8], settings: &PanoramaCookSettings) -> u128 {
    let mut h = StableHasher::new();
    h.write_str("engine panorama 1");
    h.write_str(ENCODER);
    h.write_str("2 x 2 bilinear samples; box mips");
    h.write_u64(u64::from(settings.size));
    match settings.sun {
        None => h.write_str("no sun"),
        Some(s) => {
            h.write_str("find_sun 1");
            for v in [s.min_contrast, s.falloff, s.max_cone] {
                h.write_u64(u64::from(v.to_bits()));
            }
        }
    }
    h.write(&hash128(encoded).to_le_bytes());
    h.finish()
}

/// Entry kind of cooked panoramas in the derived data cache.
const PANORAMA_KIND: &str = "panorama";

/// Imports a panorama file (`.hdr` or OpenEXR, [`decode_panorama`]) as a cooked panorama:
/// decoded and cooked, or loaded from `cache` when an earlier import of the same bytes with
/// the same settings stored it.
///
/// # Errors
///
/// If the bytes are not a panorama this crate reads.
pub fn import_panorama(
    encoded: &[u8],
    settings: &PanoramaCookSettings,
    cache: Option<&DerivedDataCache>,
    jobs: Option<&ThreadPool>,
) -> Result<(CookedPanorama, CacheOutcome), HdrError> {
    engine_profiler::zone!("assets::import_panorama");
    let key = cache.map(|_| panorama_key(encoded, settings));
    if let (Some(cache), Some(key)) = (cache, key)
        && let Some(panorama) = cache
            .get(key, PANORAMA_KIND)
            .and_then(|bytes| CookedPanorama::from_bytes(&bytes))
    {
        return Ok((panorama, CacheOutcome::Hit));
    }
    let panorama = cook_panorama(&decode_panorama(encoded)?, settings, jobs);
    if let (Some(cache), Some(key)) = (cache, key) {
        // A cache that cannot write only costs the next import its time.
        let _ = cache.put(key, PANORAMA_KIND, &panorama.to_bytes());
    }
    Ok((panorama, CacheOutcome::Miss))
}

/// Writes `panorama` to a pack of its own at `path`: one `Panorama` entry (pack version 4),
/// compressed with zstd at `level` where that saves space.
///
/// # Errors
///
/// If the file cannot be written.
pub fn write_panorama_pack(
    path: &Path,
    panorama: &CookedPanorama,
    level: i32,
) -> Result<PackStats, PackError> {
    engine_profiler::zone!("assets::write_panorama_pack");
    let meta = [
        panorama.size,
        panorama.levels.len() as u32,
        u32::from(panorama.sun.is_some()),
        0,
        0,
        0,
        0,
    ];
    let shift = crate::pack::DEFAULT_CHUNK_SHIFT;
    let entry = PreparedEntry::new(
        EntryKind::Panorama,
        meta,
        &panorama.to_bytes(),
        level,
        shift,
    );
    let mut writer = PackWriter::create(path, shift)?;
    writer.add(&entry)?;
    writer.finish()
}

/// Reads the panorama of the pack at `path` (its first `Panorama` entry), decompressing
/// on `jobs`.
///
/// # Errors
///
/// If the file cannot be read, or holds no valid panorama.
pub fn load_panorama_pack(path: &Path, jobs: &ThreadPool) -> Result<CookedPanorama, PackError> {
    engine_profiler::zone!("assets::load_panorama_pack");
    let pack = Pack::open(path)?;
    let index = pack
        .entries()
        .iter()
        .position(|e| e.kind() == EntryKind::Panorama)
        .ok_or_else(|| invalid("no panorama in the pack"))?;
    let found = Mutex::new(None);
    pack.load(&[index as u32], jobs, 1, |_, _, data| {
        let panorama =
            CookedPanorama::from_bytes(&data).ok_or_else(|| invalid("damaged panorama entry"))?;
        *found.lock().unwrap_or_else(|e| e.into_inner()) = Some(panorama);
        Ok(())
    })?;
    found
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .ok_or_else(|| invalid("the panorama entry was not loaded"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hdr::{linear_to_rgbe, panorama_direction};

    /// A `width × height` panorama of `radiance(direction)`.
    fn panorama(width: u32, height: u32, radiance: impl Fn(Vec3) -> Vec3) -> HdrImage {
        let pixels = (0..width * height)
            .map(|i| {
                let uv = [
                    ((i % width) as f32 + 0.5) / width as f32,
                    ((i / width) as f32 + 0.5) / height as f32,
                ];
                linear_to_rgbe(radiance(panorama_direction(uv)).to_array())
            })
            .collect();
        HdrImage {
            width,
            height,
            pixels,
        }
    }

    #[test]
    fn faces_face_their_axes_and_mips_average() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let image = panorama(512, 256, |d| d * 0.5 + 0.5);
        let settings = PanoramaCookSettings {
            size: 64,
            sun: None,
        };
        let cooked = cook_panorama(&image, &settings, Some(&jobs));
        assert_eq!(cooked.size, 64);
        assert_eq!(cooked.levels.len(), 7);
        assert_eq!(cooked.sun, None);
        for (level, bytes) in cooked.levels.iter().enumerate() {
            assert_eq!(
                bytes.len(),
                6 * CookedPanorama::face_bytes(64, level as u32)
            );
        }
        // The middle of each face shows its axis (before compression: the compressed
        // cube is checked on the GPU, block_compression's BC6H decoder to floats being
        // broken in 0.10), and the level below averages it.
        let level = cube(&image, 64, Some(&jobs));
        let below = next_level(&level, 64);
        let axes = [
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Y,
            Vec3::NEG_Y,
            Vec3::Z,
            Vec3::NEG_Z,
        ];
        for (face, axis) in axes.iter().enumerate() {
            let middle = level[face * 64 * 64 + 32 * 64 + 32];
            let expected = *axis * 0.5 + 0.5;
            assert!(
                (middle - expected).abs().max_element() < 0.02,
                "face {face}: {middle} vs {expected}"
            );
            let at = |x: usize, y: usize| level[face * 64 * 64 + y * 64 + x];
            let mean = (at(10, 20) + at(11, 20) + at(10, 21) + at(11, 21)) * 0.25;
            assert!((below[face * 32 * 32 + 10 * 32 + 5] - mean).length() < 1e-6);
        }
        // Serialized, and its garbage refused.
        let bytes = cooked.to_bytes();
        assert_eq!(CookedPanorama::from_bytes(&bytes), Some(cooked.clone()));
        assert_eq!(CookedPanorama::from_bytes(&bytes[..bytes.len() - 1]), None);
        assert_eq!(CookedPanorama::from_bytes(b"ENGPAN01"), None);
    }

    #[test]
    fn suns_cache_entries_and_packs_round_trip() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let toward = Vec3::new(0.3, 0.8, -0.5).normalize();
        let image = panorama(1024, 512, |d| {
            Vec3::splat(if d.dot(toward) > 0.995 { 20_000.0 } else { 0.5 })
        });
        let encoded = crate::hdr::encode_hdr(&image);
        let dir = std::env::temp_dir().join(format!("engine_panorama_test_{}", std::process::id()));
        let cache = DerivedDataCache::new(&dir).unwrap();
        let settings = PanoramaCookSettings::default();
        let (first, outcome) =
            import_panorama(&encoded, &settings, Some(&cache), Some(&jobs)).unwrap();
        assert_eq!(outcome, CacheOutcome::Miss);
        assert_eq!(first.size, 256);
        let sun = first.sun.unwrap();
        assert!(sun.direction.dot(toward) > 0.9999, "{sun:?}");
        let (second, outcome) =
            import_panorama(&encoded, &settings, Some(&cache), Some(&jobs)).unwrap();
        assert_eq!(outcome, CacheOutcome::Hit);
        assert_eq!(second, first);
        // Another setting is another entry.
        let other = PanoramaCookSettings {
            sun: None,
            ..settings
        };
        let (third, outcome) =
            import_panorama(&encoded, &other, Some(&cache), Some(&jobs)).unwrap();
        assert_eq!((outcome, third.sun), (CacheOutcome::Miss, None));
        // A pack of its own, version 4.
        let path = dir.join("sky.pack");
        write_panorama_pack(&path, &first, 3).unwrap();
        let file = std::fs::read(&path).unwrap();
        let header: crate::pack::Header = bytemuck::pod_read_unaligned(&file[..64]);
        assert_eq!(header.version, 4);
        assert_eq!(load_panorama_pack(&path, &jobs).unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
