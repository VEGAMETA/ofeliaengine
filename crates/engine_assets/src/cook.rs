//! Cooked scenes (ADR 0056): a scene as the runtime uses it: textures compressed with
//! their mip levels, meshes with detail levels, materials and instances. [`cook_scene`]
//! makes one from an imported glTF scene; [`write_pack`] stores it and [`load_pack`] reads
//! it back without the sources, the derived data cache or any decoding but zstd's.
//!
//! Entries of a scene's pack (`meta` of [`crate::pack::Entry`]), in the order they are
//! written:
//!
//! - **mesh:** `[vertices, indices, detail levels, attributes (1 normals, 2 uvs, 4
//!   tangents)]`; positions, the attributes present, the bounds, the indices, then per
//!   level its index count and error, then the levels' indices. Sections padded to 16.
//! - **texture levels:** `[format code, width, height, levels, first level, level count]`
//!   (width and height of level 0); the levels, each padded to 16 bytes. A texture's
//!   levels of at most [`TAIL_SIZE`] texels share one entry (its tail, written with the
//!   meshes: the whole scene at low resolution); each larger level has its own entry,
//!   written after every tail, smallest first.
//! - **scene:** `[textures, materials, instances]`; [`TextureRecord`]s (which entry holds
//!   each level), [`MaterialRecord`]s, [`InstanceRecord`]s (meshes by entry index).

use crate::ddc::{DerivedDataCache, import_texture};
use crate::gltf_import::{AlphaMode, ImportError, ImportedScene, TextureWrap, read_texture};
use crate::lod::{Lod, LodSettings, generate_lods};
use crate::pack::{
    DEFAULT_CHUNK_SHIFT, EntryData, EntryKind, LoadStats, Pack, PackError, PackStats, PackWriter,
    PreparedEntry, invalid,
};
use crate::texture_compress::{EncodedTexture, TextureFormat};
use crate::texture_import::{ImageError, TextureUsage, build_mips, decode_image};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Quat, Vec3};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// A mesh ready for the GPU (one glTF primitive).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CookedMesh {
    /// Positions.
    pub positions: Vec<[f32; 3]>,
    /// Normals: one per position, or none.
    pub normals: Vec<[f32; 3]>,
    /// Texture coordinates: one per position, or none.
    pub uvs: Vec<[f32; 2]>,
    /// Tangents (w: handedness): one per position, or none.
    pub tangents: Vec<[f32; 4]>,
    /// Triangles.
    pub indices: Vec<u32>,
    /// Coarser detail levels over the same vertices (ADR 0053).
    pub lods: Vec<Lod>,
    /// Bounds of the positions.
    pub min: [f32; 3],
    /// Bounds of the positions.
    pub max: [f32; 3],
}

/// A texture of a material: which one and how it repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureSlot {
    /// Index into [`CookedScene::textures`].
    pub texture: u32,
    /// Wrap along u and v.
    pub wrap: [TextureWrap; 2],
}

/// A material; its textures refer to [`CookedScene::textures`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CookedMaterial {
    /// Base color factor (linear, alpha = coverage).
    pub base_color: [f32; 4],
    /// Base color texture (sRGB).
    pub base_color_texture: Option<TextureSlot>,
    /// Metallic factor.
    pub metallic: f32,
    /// Roughness factor.
    pub roughness: f32,
    /// Metallic (blue) and roughness (green).
    pub metallic_roughness_texture: Option<TextureSlot>,
    /// Normal map.
    pub normal_texture: Option<TextureSlot>,
    /// Normal map strength.
    pub normal_scale: f32,
    /// Ambient occlusion (red).
    pub occlusion_texture: Option<TextureSlot>,
    /// Occlusion strength.
    pub occlusion_strength: f32,
    /// Emission factor.
    pub emissive: [f32; 3],
    /// Emission texture (sRGB).
    pub emissive_texture: Option<TextureSlot>,
    /// Alpha mode.
    pub alpha: AlphaMode,
    /// Drawn without back-face culling.
    pub double_sided: bool,
}

/// A mesh placed in the world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CookedInstance {
    /// Index into [`CookedScene::meshes`].
    pub mesh: u32,
    /// Index into [`CookedScene::materials`], or none (the default material).
    pub material: Option<u32>,
    /// Object to world, column major.
    pub world_from_object: [f32; 16],
}

/// A scene as the runtime uses it (see the module documentation).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CookedScene {
    /// Textures, compressed, with every mip level.
    pub textures: Vec<EncodedTexture>,
    /// Meshes.
    pub meshes: Vec<CookedMesh>,
    /// Materials.
    pub materials: Vec<CookedMaterial>,
    /// Instances.
    pub instances: Vec<CookedInstance>,
}

/// How to cook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CookSettings {
    /// Make detail levels for meshes (ADR 0053).
    pub lods: bool,
    /// zstd level for pack entries (1-22; 0 stores them as they are). 19 by default: the
    /// smallest files for loading speeds that do not change (decoding runs at the same
    /// speed at every level).
    pub compression_level: i32,
    /// Block-compress textures (BC7, BC5; ADR 0041). Off: RGBA8 levels, for comparisons.
    pub block_compression: bool,
}

impl Default for CookSettings {
    fn default() -> Self {
        Self {
            lods: true,
            compression_level: 19,
            block_compression: true,
        }
    }
}

/// Why a scene could not be cooked.
#[derive(Debug)]
pub enum CookError {
    /// A source could not be read.
    Import(ImportError),
    /// A texture could not be decoded.
    Image(ImageError),
    /// The pack could not be written or read.
    Pack(PackError),
}

impl core::fmt::Display for CookError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Import(e) => write!(f, "{e}"),
            Self::Image(e) => write!(f, "{e}"),
            Self::Pack(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CookError {}

impl From<PackError> for CookError {
    fn from(e: PackError) -> Self {
        Self::Pack(e)
    }
}

/// How a material's base color texture is filtered.
fn color_usage(alpha: AlphaMode) -> TextureUsage {
    match alpha {
        AlphaMode::Mask(bits) => TextureUsage::ColorMasked {
            cutoff: f32::from_bits(bits),
        },
        AlphaMode::Opaque | AlphaMode::Blend => TextureUsage::Color,
    }
}

/// Every distinct use of a texture by the materials: `(texture, usage)`. The usage decides
/// how its mip levels are filtered and compressed.
fn texture_uses(scene: &ImportedScene) -> Vec<(usize, TextureUsage)> {
    let mut uses = Vec::new();
    let mut add = |texture: Option<usize>, usage| {
        if let Some(texture) = texture
            && !uses.contains(&(texture, usage))
        {
            uses.push((texture, usage));
        }
    };
    for m in &scene.materials {
        add(m.base_color_texture, color_usage(m.alpha));
        add(m.emissive_texture, TextureUsage::Color);
        add(m.metallic_roughness_texture, TextureUsage::Data);
        add(m.occlusion_texture, TextureUsage::Data);
        add(m.normal_texture, TextureUsage::Normal);
    }
    uses
}

/// What [`cook_scene`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CookStats {
    /// Textures taken from the derived data cache.
    pub cache_hits: usize,
    /// Seconds spent on textures.
    pub texture_seconds: f64,
    /// Seconds spent on meshes (detail levels).
    pub mesh_seconds: f64,
}

/// Cooks `scene` (imported from a glTF file in `base_dir`): its textures read, filtered
/// and compressed in parallel (from `cache` when it has them), its meshes given detail
/// levels, its node tree flattened into instances.
pub fn cook_scene(
    scene: &ImportedScene,
    base_dir: Option<&Path>,
    cache: Option<&DerivedDataCache>,
    jobs: &ThreadPool,
    settings: &CookSettings,
) -> Result<(CookedScene, CookStats), CookError> {
    engine_profiler::zone!("assets::cook_scene");
    let mut stats = CookStats::default();
    let start = std::time::Instant::now();
    let uses = texture_uses(scene);
    let textures: Vec<OnceLock<Result<(EncodedTexture, bool), CookError>>> =
        uses.iter().map(|_| OnceLock::new()).collect();
    jobs.parallel_for(uses.len(), 1, |_, range| {
        for k in range {
            let (texture, usage) = uses[k];
            let result = read_texture(&scene.textures[texture].source, base_dir)
                .map_err(CookError::Import)
                .and_then(|encoded| {
                    if settings.block_compression {
                        import_texture(&encoded, usage, cache, Some(jobs))
                            .map(|(t, outcome)| (t, outcome == crate::CacheOutcome::Hit))
                            .map_err(CookError::Image)
                    } else {
                        let mips =
                            build_mips(&decode_image(&encoded).map_err(CookError::Image)?, usage);
                        let format = if mips.srgb {
                            TextureFormat::Rgba8Srgb
                        } else {
                            TextureFormat::Rgba8Unorm
                        };
                        let texture = EncodedTexture {
                            format,
                            width: mips.width,
                            height: mips.height,
                            levels: mips.levels,
                        };
                        Ok((texture, false))
                    }
                });
            let _ = textures[k].set(result);
        }
    });
    let mut cooked = CookedScene::default();
    for texture in textures {
        let Some(result) = texture.into_inner() else {
            continue;
        };
        let (texture, hit) = result?;
        stats.cache_hits += usize::from(hit);
        cooked.textures.push(texture);
    }
    stats.texture_seconds = start.elapsed().as_secs_f64();

    // Meshes: one per primitive, with detail levels made in parallel.
    let start = std::time::Instant::now();
    let primitives: Vec<(usize, usize)> = scene
        .meshes
        .iter()
        .enumerate()
        .flat_map(|(m, mesh)| (0..mesh.primitives.len()).map(move |p| (m, p)))
        .collect();
    let meshes: Vec<OnceLock<CookedMesh>> = primitives.iter().map(|_| OnceLock::new()).collect();
    jobs.parallel_for(primitives.len(), 1, |_, range| {
        for k in range {
            let (m, p) = primitives[k];
            let p = &scene.meshes[m].primitives[p];
            let lods = if settings.lods {
                generate_lods(&p.positions, &p.indices, &LodSettings::default())
            } else {
                Vec::new()
            };
            let _ = meshes[k].set(CookedMesh {
                positions: p.positions.clone(),
                normals: p.normals.clone(),
                uvs: p.uvs.clone(),
                tangents: p.tangents.clone(),
                indices: p.indices.clone(),
                lods,
                min: p.min,
                max: p.max,
            });
        }
    });
    cooked.meshes = meshes
        .into_iter()
        .filter_map(OnceLock::into_inner)
        .collect();
    stats.mesh_seconds = start.elapsed().as_secs_f64();

    let slot = |texture: Option<usize>, usage: TextureUsage| {
        let texture = texture?;
        let k = uses.iter().position(|&u| u == (texture, usage))?;
        Some(TextureSlot {
            texture: k as u32,
            wrap: scene.textures[texture].wrap,
        })
    };
    cooked.materials = scene
        .materials
        .iter()
        .map(|m| CookedMaterial {
            base_color: m.base_color,
            base_color_texture: slot(m.base_color_texture, color_usage(m.alpha)),
            metallic: m.metallic,
            roughness: m.roughness,
            metallic_roughness_texture: slot(m.metallic_roughness_texture, TextureUsage::Data),
            normal_texture: slot(m.normal_texture, TextureUsage::Normal),
            normal_scale: m.normal_scale,
            occlusion_texture: slot(m.occlusion_texture, TextureUsage::Data),
            occlusion_strength: m.occlusion_strength,
            emissive: m.emissive,
            emissive_texture: slot(m.emissive_texture, TextureUsage::Color),
            alpha: m.alpha,
            double_sided: m.double_sided,
        })
        .collect();

    // Instances: the node tree flattened, one instance per primitive of each mesh node.
    let first_primitive: Vec<u32> = scene
        .meshes
        .iter()
        .scan(0u32, |next, mesh| {
            let first = *next;
            *next += mesh.primitives.len() as u32;
            Some(first)
        })
        .collect();
    let mut stack: Vec<(usize, Mat4)> = scene.roots.iter().map(|&r| (r, Mat4::IDENTITY)).collect();
    while let Some((index, parent)) = stack.pop() {
        let node = &scene.nodes[index];
        let world = parent
            * Mat4::from_scale_rotation_translation(
                Vec3::from(node.scale),
                Quat::from_array(node.rotation),
                Vec3::from(node.translation),
            );
        if let Some(mesh) = node.mesh {
            for (p, primitive) in scene.meshes[mesh].primitives.iter().enumerate() {
                cooked.instances.push(CookedInstance {
                    mesh: first_primitive[mesh] + p as u32,
                    material: primitive.material.map(|m| m as u32),
                    world_from_object: world.to_cols_array(),
                });
            }
        }
        stack.extend(node.children.iter().map(|&c| (c, world)));
    }
    Ok((cooked, stats))
}

/// A texture in a scene entry: its size and which entry holds each of its levels.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TextureRecord {
    /// [`TextureFormat::code`].
    pub format: u32,
    /// Width of level 0.
    pub width: u32,
    /// Height of level 0.
    pub height: u32,
    /// Number of levels.
    pub levels: u32,
    /// The entry holding each level (`u32::MAX` past the last): one per large level, one
    /// for the small ones at the end (the tail).
    pub entries: [u32; 16],
}

const _: () = assert!(size_of::<TextureRecord>() == 80);

/// A material in a scene entry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MaterialRecord {
    /// Base color factor.
    pub base_color: [f32; 4],
    /// Emission factor.
    pub emissive: [f32; 3],
    /// Metallic factor.
    pub metallic: f32,
    /// Roughness factor.
    pub roughness: f32,
    /// Normal map strength.
    pub normal_scale: f32,
    /// Occlusion strength.
    pub occlusion_strength: f32,
    /// 0 opaque, 1 masked, 2 blended.
    pub alpha_mode: u32,
    /// Alpha cutoff bits (masked).
    pub alpha_cutoff: u32,
    /// 1: double-sided.
    pub double_sided: u32,
    /// Textures (base color, metallic-roughness, normal, occlusion, emission) as indices of
    /// the scene's [`TextureRecord`]s, or `u32::MAX`.
    pub textures: [u32; 5],
    /// Wrap modes of the textures: u | v << 8 (0 repeat, 1 mirrored, 2 clamp).
    pub wraps: [u32; 5],
}

const _: () = assert!(size_of::<MaterialRecord>() == 96);

/// An instance in a scene entry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceRecord {
    /// Mesh entry.
    pub mesh: u32,
    /// Material index in the scene, or `u32::MAX`.
    pub material: u32,
    /// Reserved, 0.
    pub reserved: [u32; 2],
    /// Object to world, column major.
    pub world_from_object: [f32; 16],
}

const _: () = assert!(size_of::<InstanceRecord>() == 80);

const NONE: u32 = u32::MAX;

/// Levels of at most this many texels on their longer side share one entry (the tail): the
/// whole texture at low resolution, loaded first (22 KiB of BC7 at 128²).
pub const TAIL_SIZE: u32 = 128;

/// The first level of a `width × height` texture that belongs to its tail.
fn first_tail_level(width: u32, height: u32, levels: u32) -> u32 {
    (0..levels)
        .find(|&k| (width >> k).max(height >> k).max(1) <= TAIL_SIZE)
        .unwrap_or(levels.saturating_sub(1))
}

fn wrap_code(w: TextureWrap) -> u32 {
    match w {
        TextureWrap::Repeat => 0,
        TextureWrap::MirroredRepeat => 1,
        TextureWrap::ClampToEdge => 2,
    }
}

fn wrap_from(code: u32) -> Result<TextureWrap, PackError> {
    Ok(match code {
        0 => TextureWrap::Repeat,
        1 => TextureWrap::MirroredRepeat,
        2 => TextureWrap::ClampToEdge,
        _ => return Err(invalid("bad wrap mode")),
    })
}

/// Appends `bytes` and pads to 16.
fn section(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes);
    out.resize(out.len().next_multiple_of(16), 0);
}

/// An entry of a texture: `level_count` levels from `first_level`.
fn texture_levels_entry(
    t: &EncodedTexture,
    first_level: u32,
    level_count: u32,
    level: i32,
) -> PreparedEntry {
    let mut data = Vec::new();
    for l in &t.levels[first_level as usize..][..level_count as usize] {
        section(&mut data, l);
    }
    let meta = [
        t.format.code(),
        t.width,
        t.height,
        t.levels.len() as u32,
        first_level,
        level_count,
        0,
    ];
    PreparedEntry::new(
        EntryKind::TextureLevels,
        meta,
        &data,
        level,
        DEFAULT_CHUNK_SHIFT,
    )
}

fn mesh_entry(m: &CookedMesh, level: i32) -> PreparedEntry {
    let mut data = Vec::new();
    let mut attributes = 0;
    section(&mut data, bytemuck::cast_slice(&m.positions));
    if !m.normals.is_empty() {
        attributes |= 1;
        section(&mut data, bytemuck::cast_slice(&m.normals));
    }
    if !m.uvs.is_empty() {
        attributes |= 2;
        section(&mut data, bytemuck::cast_slice(&m.uvs));
    }
    if !m.tangents.is_empty() {
        attributes |= 4;
        section(&mut data, bytemuck::cast_slice(&m.tangents));
    }
    section(&mut data, bytemuck::cast_slice(&[m.min, m.max]));
    section(&mut data, bytemuck::cast_slice(&m.indices));
    let table: Vec<u32> = m
        .lods
        .iter()
        .flat_map(|l| [l.indices.len() as u32, l.error.to_bits()])
        .collect();
    section(&mut data, bytemuck::cast_slice(&table));
    for l in &m.lods {
        section(&mut data, bytemuck::cast_slice(&l.indices));
    }
    let meta = [
        m.positions.len() as u32,
        m.indices.len() as u32,
        m.lods.len() as u32,
        attributes,
        0,
        0,
        0,
    ];
    PreparedEntry::new(EntryKind::Mesh, meta, &data, level, DEFAULT_CHUNK_SHIFT)
}

/// A part of the scene stored as one entry.
#[derive(Debug, Clone, Copy)]
enum Piece {
    Mesh(usize),
    /// Texture, first level, level count.
    Levels(usize, u32, u32),
}

/// Writes `scene` to a pack at `path`, compressing entries on `jobs`. Entries are in the
/// order a scene loads best from a hard disk: meshes and the textures' tails first (the
/// whole scene at low resolution), then the textures' large levels from the smallest.
pub fn write_pack(
    path: &Path,
    scene: &CookedScene,
    jobs: &ThreadPool,
    level: i32,
) -> Result<PackStats, PackError> {
    engine_profiler::zone!("assets::write_pack");
    let mut pieces: Vec<Piece> = (0..scene.meshes.len()).map(Piece::Mesh).collect();
    let mut large = Vec::new();
    for (t, texture) in scene.textures.iter().enumerate() {
        let levels = texture.levels.len() as u32;
        let tail = first_tail_level(texture.width, texture.height, levels);
        pieces.push(Piece::Levels(t, tail, levels - tail));
        large.extend((0..tail).map(|k| Piece::Levels(t, k, 1)));
    }
    let bytes = |p: &Piece| match *p {
        Piece::Levels(t, k, _) => scene.textures[t].levels[k as usize].len(),
        Piece::Mesh(_) => 0,
    };
    large.sort_by_key(bytes);
    pieces.extend(large);

    // Compressing is the slow part: every entry in parallel.
    let prepared: Vec<OnceLock<PreparedEntry>> = pieces.iter().map(|_| OnceLock::new()).collect();
    jobs.parallel_for(pieces.len(), 1, |_, range| {
        for k in range {
            let entry = match pieces[k] {
                Piece::Mesh(m) => mesh_entry(&scene.meshes[m], level),
                Piece::Levels(t, first, count) => {
                    texture_levels_entry(&scene.textures[t], first, count, level)
                }
            };
            let _ = prepared[k].set(entry);
        }
    });
    let mut writer = PackWriter::create(path, DEFAULT_CHUNK_SHIFT)?;
    let mut mesh_entries = vec![NONE; scene.meshes.len()];
    let mut records: Vec<TextureRecord> = scene
        .textures
        .iter()
        .map(|t| TextureRecord {
            format: t.format.code(),
            width: t.width,
            height: t.height,
            levels: t.levels.len() as u32,
            entries: [NONE; 16],
        })
        .collect();
    for (piece, entry) in pieces.iter().zip(prepared) {
        let Some(entry) = entry.into_inner() else {
            return Err(invalid("an entry was not prepared"));
        };
        let index = writer.add(&entry)?;
        match *piece {
            Piece::Mesh(m) => mesh_entries[m] = index,
            Piece::Levels(t, first, count) => {
                for k in first..first + count {
                    let slot = records[t]
                        .entries
                        .get_mut(k as usize)
                        .ok_or_else(|| invalid("more than 16 mip levels"))?;
                    *slot = index;
                }
            }
        }
    }
    let texture = |slot: Option<TextureSlot>| {
        slot.map_or((NONE, 0), |s| {
            (s.texture, wrap_code(s.wrap[0]) | wrap_code(s.wrap[1]) << 8)
        })
    };
    let materials: Vec<MaterialRecord> = scene
        .materials
        .iter()
        .map(|m| {
            let slots = [
                texture(m.base_color_texture),
                texture(m.metallic_roughness_texture),
                texture(m.normal_texture),
                texture(m.occlusion_texture),
                texture(m.emissive_texture),
            ];
            let (alpha_mode, alpha_cutoff) = match m.alpha {
                AlphaMode::Opaque => (0, 0),
                AlphaMode::Mask(bits) => (1, bits),
                AlphaMode::Blend => (2, 0),
            };
            MaterialRecord {
                base_color: m.base_color,
                emissive: m.emissive,
                metallic: m.metallic,
                roughness: m.roughness,
                normal_scale: m.normal_scale,
                occlusion_strength: m.occlusion_strength,
                alpha_mode,
                alpha_cutoff,
                double_sided: u32::from(m.double_sided),
                textures: slots.map(|s| s.0),
                wraps: slots.map(|s| s.1),
            }
        })
        .collect();
    let instances: Vec<InstanceRecord> = scene
        .instances
        .iter()
        .map(|i| InstanceRecord {
            mesh: mesh_entries[i.mesh as usize],
            material: i.material.unwrap_or(NONE),
            reserved: [0; 2],
            world_from_object: i.world_from_object,
        })
        .collect();
    let mut data = Vec::new();
    section(&mut data, bytemuck::cast_slice(&records));
    section(&mut data, bytemuck::cast_slice(&materials));
    section(&mut data, bytemuck::cast_slice(&instances));
    let meta = [
        records.len() as u32,
        materials.len() as u32,
        instances.len() as u32,
        0,
        0,
        0,
        0,
    ];
    writer.add(&PreparedEntry::new(
        EntryKind::Scene,
        meta,
        &data,
        level,
        DEFAULT_CHUNK_SHIFT,
    ))?;
    writer.finish()
}

/// Reads sections off an entry's bytes, checking bounds.
struct Sections<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Sections<'a> {
    fn next<T: bytemuck::Pod>(&mut self, count: usize) -> Result<Vec<T>, PackError> {
        let bytes = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| invalid("section too large"))?;
        Ok(self
            .slice(bytes)?
            .chunks_exact(size_of::<T>())
            .map(bytemuck::pod_read_unaligned)
            .collect())
    }

    fn slice(&mut self, len: usize) -> Result<&'a [u8], PackError> {
        let end = self.at.checked_add(len).filter(|&e| e <= self.data.len());
        let Some(end) = end else {
            return Err(invalid("section out of bounds"));
        };
        let out = &self.data[self.at..end];
        self.at = end.next_multiple_of(16).min(self.data.len());
        Ok(out)
    }
}

/// The dimensions a texture entry or record describes, checked.
fn texture_shape(
    format: u32,
    width: u32,
    height: u32,
    levels: u32,
) -> Result<TextureFormat, PackError> {
    let format =
        TextureFormat::from_code(format).ok_or_else(|| invalid("unknown texture format"))?;
    let full = 32 - width.max(height).max(1).leading_zeros();
    if width == 0
        || height == 0
        || width > 1 << 15
        || height > 1 << 15
        || levels == 0
        || levels > full
    {
        return Err(invalid("bad texture dimensions"));
    }
    Ok(format)
}

/// Levels of a texture entry: `(meta, levels)`.
fn read_levels_entry(meta: [u32; 7], data: &[u8]) -> Result<Vec<Vec<u8>>, PackError> {
    let [code, width, height, levels, first, count, _] = meta;
    let format = texture_shape(code, width, height, levels)?;
    if count == 0 || first.checked_add(count).is_none_or(|end| end > levels) {
        return Err(invalid("bad texture levels"));
    }
    let mut sections = Sections { data, at: 0 };
    (first..first + count)
        .map(|k| {
            let bytes = format.level_bytes((width >> k).max(1), (height >> k).max(1));
            sections.slice(bytes).map(<[u8]>::to_vec)
        })
        .collect()
}

fn read_mesh_entry(meta: [u32; 7], data: &[u8]) -> Result<CookedMesh, PackError> {
    let [vertices, index_count, lod_count, attributes, ..] = meta.map(|m| m as usize);
    let mut s = Sections { data, at: 0 };
    let positions = s.next::<[f32; 3]>(vertices)?;
    let normals = if attributes & 1 != 0 {
        s.next(vertices)?
    } else {
        Vec::new()
    };
    let uvs = if attributes & 2 != 0 {
        s.next(vertices)?
    } else {
        Vec::new()
    };
    let tangents = if attributes & 4 != 0 {
        s.next(vertices)?
    } else {
        Vec::new()
    };
    let bounds = s.next::<[f32; 3]>(2)?;
    let indices = s.next::<u32>(index_count)?;
    let table = s.next::<[u32; 2]>(lod_count)?;
    let lods = table
        .iter()
        .map(|&[count, error]| {
            Ok(Lod {
                indices: s.next::<u32>(count as usize)?,
                error: f32::from_bits(error),
            })
        })
        .collect::<Result<Vec<_>, PackError>>()?;
    let out_of_range = |list: &[u32]| list.iter().any(|&i| i as usize >= vertices);
    if out_of_range(&indices) || lods.iter().any(|l| out_of_range(&l.indices)) {
        return Err(invalid("mesh index out of range"));
    }
    Ok(CookedMesh {
        positions,
        normals,
        uvs,
        tangents,
        indices,
        lods,
        min: bounds[0],
        max: bounds[1],
    })
}

/// Where some of a texture's levels are in its pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureEntry {
    /// The entry.
    pub entry: u32,
    /// The texture (index into [`SceneLayout::textures`]).
    pub texture: u32,
    /// Its first level.
    pub first_level: u32,
    /// How many levels it holds, from `first_level` on.
    pub level_count: u32,
}

impl TextureEntry {
    /// Whether it holds the texture's smallest levels (its tail): the texture is visible
    /// once they are uploaded.
    pub fn is_tail(&self, layout: &SceneLayout) -> bool {
        layout
            .textures
            .get(self.texture as usize)
            .is_some_and(|t| self.first_level + self.level_count == t.levels)
    }
}

/// A cooked scene without its textures' levels: what streaming starts from (ADR 0057).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneLayout {
    /// The textures: format, size, levels, and the entries holding them.
    pub textures: Vec<TextureRecord>,
    /// Meshes, materials and instances; `textures` stays empty and the materials' texture
    /// slots index [`Self::textures`].
    pub scene: CookedScene,
    /// Every texture entry, in file order: the textures' tails first, then their larger
    /// levels from the smallest (the order to stream them in).
    pub texture_entries: Vec<TextureEntry>,
}

/// An entry decoded by kind.
enum Decoded {
    Mesh(CookedMesh),
    Scene(Vec<TextureRecord>, Vec<MaterialRecord>, Vec<InstanceRecord>),
}

fn add_stats(total: &mut LoadStats, more: LoadStats) {
    total.read_bytes += more.read_bytes;
    total.bytes += more.bytes;
    total.seconds += more.seconds;
    total.decode_seconds += more.decode_seconds;
    total.backend = more.backend;
}

/// Opens the pack at `path` and loads everything but the textures' levels: the scene
/// record and the meshes (up to `depth` reads in flight, decoded on `jobs`). The textures
/// are described, with the entries holding their levels, for [`load_pack`] or for
/// streaming them with a [`crate::pack::PackStreamer`] over the returned pack.
pub fn open_scene(
    path: &Path,
    jobs: &ThreadPool,
    depth: usize,
) -> Result<(Pack, SceneLayout, LoadStats), PackError> {
    engine_profiler::zone!("assets::open_scene");
    let pack = Pack::open(path)?;
    let entries = pack.entries();
    let decoded: Vec<Mutex<Option<Decoded>>> = entries.iter().map(|_| Mutex::new(None)).collect();
    let indices: Vec<u32> = (0..entries.len() as u32)
        .filter(|&i| entries[i as usize].kind() != EntryKind::TextureLevels)
        .collect();
    let stats = pack.load(&indices, jobs, depth, |index, e, data: EntryData| {
        let value = match e.kind() {
            EntryKind::Mesh => Decoded::Mesh(read_mesh_entry(e.meta, &data)?),
            EntryKind::Scene => {
                let mut s = Sections { data: &data, at: 0 };
                let textures = s.next::<TextureRecord>(e.meta[0] as usize)?;
                let materials = s.next::<MaterialRecord>(e.meta[1] as usize)?;
                let instances = s.next::<InstanceRecord>(e.meta[2] as usize)?;
                Decoded::Scene(textures, materials, instances)
            }
            EntryKind::TextureLevels => return Err(invalid("texture levels loaded early")),
        };
        if let Ok(mut slot) = decoded[index as usize].lock() {
            *slot = Some(value);
        }
        Ok(())
    })?;
    let mut layout = SceneLayout::default();
    let mut mesh_of = vec![NONE; entries.len()];
    let mut records = None;
    for (i, slot) in decoded.into_iter().enumerate() {
        match slot.into_inner().ok().flatten() {
            Some(Decoded::Mesh(m)) => {
                mesh_of[i] = layout.scene.meshes.len() as u32;
                layout.scene.meshes.push(m);
            }
            Some(Decoded::Scene(t, m, inst)) => {
                if records.replace((t, m, inst)).is_some() {
                    return Err(invalid("more than one scene"));
                }
            }
            None if entries[i].kind() == EntryKind::TextureLevels => {}
            None => return Err(invalid(format!("entry {i} not loaded"))),
        }
    }
    let Some((textures, materials, instances)) = records else {
        return Err(invalid("no scene"));
    };
    // Every texture's levels are in texture entries that match it.
    for (t, record) in textures.iter().enumerate() {
        texture_shape(record.format, record.width, record.height, record.levels)?;
        if record.levels as usize > record.entries.len() {
            return Err(invalid("texture with too many levels"));
        }
        let mut k = 0;
        while k < record.levels {
            let index = record.entries[k as usize];
            let e = entries
                .get(index as usize)
                .filter(|e| e.kind() == EntryKind::TextureLevels)
                .ok_or_else(|| invalid("texture level in a missing entry"))?;
            let [code, width, height, count, first, level_count, _] = e.meta;
            if [code, width, height, count]
                != [record.format, record.width, record.height, record.levels]
                || first != k
                || level_count == 0
                || first + level_count > record.levels
                || (first..first + level_count).any(|l| record.entries[l as usize] != index)
            {
                return Err(invalid("texture level entry does not match its texture"));
            }
            layout.texture_entries.push(TextureEntry {
                entry: index,
                texture: t as u32,
                first_level: first,
                level_count,
            });
            k += level_count;
        }
    }
    layout.texture_entries.sort_by_key(|t| (t.entry, t.texture));
    let slot = |texture: u32, wrap: u32| -> Result<Option<TextureSlot>, PackError> {
        if texture == NONE {
            return Ok(None);
        }
        if texture as usize >= textures.len() {
            return Err(invalid("material refers to a missing texture"));
        }
        Ok(Some(TextureSlot {
            texture,
            wrap: [wrap_from(wrap & 0xff)?, wrap_from(wrap >> 8)?],
        }))
    };
    for m in &materials {
        let t = |k: usize| slot(m.textures[k], m.wraps[k]);
        layout.scene.materials.push(CookedMaterial {
            base_color: m.base_color,
            base_color_texture: t(0)?,
            metallic: m.metallic,
            roughness: m.roughness,
            metallic_roughness_texture: t(1)?,
            normal_texture: t(2)?,
            normal_scale: m.normal_scale,
            occlusion_texture: t(3)?,
            occlusion_strength: m.occlusion_strength,
            emissive: m.emissive,
            emissive_texture: t(4)?,
            alpha: match m.alpha_mode {
                0 => AlphaMode::Opaque,
                1 => AlphaMode::Mask(m.alpha_cutoff),
                2 => AlphaMode::Blend,
                _ => return Err(invalid("bad alpha mode")),
            },
            double_sided: m.double_sided != 0,
        });
    }
    for i in &instances {
        let mesh = *mesh_of
            .get(i.mesh as usize)
            .filter(|&&m| m != NONE)
            .ok_or_else(|| invalid("instance refers to a missing mesh"))?;
        let material = (i.material != NONE).then_some(i.material);
        if material.is_some_and(|m| m as usize >= layout.scene.materials.len()) {
            return Err(invalid("instance refers to a missing material"));
        }
        layout.scene.instances.push(CookedInstance {
            mesh,
            material,
            world_from_object: i.world_from_object,
        });
    }
    layout.textures = textures;
    Ok((pack, layout, stats))
}

/// Loads the whole scene of the pack at `path`: [`open_scene`], then every texture entry
/// in file order, up to `depth` reads in flight, decoded on `jobs`.
pub fn load_pack(
    path: &Path,
    jobs: &ThreadPool,
    depth: usize,
) -> Result<(CookedScene, LoadStats), PackError> {
    engine_profiler::zone!("assets::load_pack");
    let (pack, layout, mut stats) = open_scene(path, jobs, depth)?;
    let mut indices: Vec<u32> = layout.texture_entries.iter().map(|t| t.entry).collect();
    indices.dedup();
    let levels: Vec<Mutex<Vec<Vec<u8>>>> = pack
        .entries()
        .iter()
        .map(|_| Mutex::new(Vec::new()))
        .collect();
    let more = pack.load(&indices, jobs, depth, |index, e, data: EntryData| {
        let held = read_levels_entry(e.meta, &data)?;
        if let Ok(mut slot) = levels[index as usize].lock() {
            *slot = held;
        }
        Ok(())
    })?;
    add_stats(&mut stats, more);
    let levels: Vec<Vec<Vec<u8>>> = levels
        .into_iter()
        .map(|l| l.into_inner().unwrap_or_default())
        .collect();
    let SceneLayout {
        textures,
        mut scene,
        ..
    } = layout;
    for record in &textures {
        let format = texture_shape(record.format, record.width, record.height, record.levels)?;
        let mut out = Vec::with_capacity(record.levels as usize);
        for k in 0..record.levels {
            let index = record.entries[k as usize] as usize;
            let first = pack.entries()[index].meta[4];
            let level = levels[index]
                .get((k - first) as usize)
                .ok_or_else(|| invalid("texture level missing from its entry"))?;
            out.push(level.clone());
        }
        scene.textures.push(EncodedTexture {
            format,
            width: record.width,
            height: record.height,
            levels: out,
        });
    }
    Ok((scene, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Levels of a `size`² BC7 texture, each filled with its level number.
    fn bc7(size: u32) -> EncodedTexture {
        let levels = 32 - size.leading_zeros();
        EncodedTexture {
            format: TextureFormat::Bc7Srgb,
            width: size,
            height: size,
            levels: (0..levels)
                .map(|k| {
                    let s = (size >> k).max(1);
                    vec![k as u8 + 1; TextureFormat::Bc7Srgb.level_bytes(s, s)]
                })
                .collect(),
        }
    }

    fn sample() -> CookedScene {
        let quad = CookedMesh {
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2, 0, 2, 3],
            lods: vec![Lod {
                indices: vec![0, 1, 2],
                error: 0.25,
            }],
            min: [0.0; 3],
            max: [1.0, 1.0, 0.0],
        };
        let data_texture = EncodedTexture {
            format: TextureFormat::Rgba8Unorm,
            width: 2,
            height: 2,
            levels: vec![vec![9; 16], vec![8; 4]],
        };
        let material = CookedMaterial {
            base_color: [1.0, 0.5, 0.25, 1.0],
            base_color_texture: Some(TextureSlot {
                texture: 0,
                wrap: [TextureWrap::Repeat, TextureWrap::ClampToEdge],
            }),
            metallic: 0.0,
            roughness: 0.7,
            metallic_roughness_texture: Some(TextureSlot {
                texture: 1,
                wrap: [TextureWrap::MirroredRepeat; 2],
            }),
            normal_texture: Some(TextureSlot {
                texture: 2,
                wrap: [TextureWrap::Repeat; 2],
            }),
            normal_scale: 1.0,
            occlusion_texture: Some(TextureSlot {
                texture: 1,
                wrap: [TextureWrap::MirroredRepeat; 2],
            }),
            occlusion_strength: 0.5,
            emissive: [0.0; 3],
            emissive_texture: None,
            alpha: AlphaMode::Mask(0.5f32.to_bits()),
            double_sided: true,
        };
        CookedScene {
            // A texture with large levels and a tail, one that is all tail, and a
            // non-square one.
            textures: vec![bc7(512), data_texture, {
                let mut t = bc7(256);
                t.width = 256;
                t.height = 64;
                t.levels = (0..9u32)
                    .map(|k| {
                        let (w, h) = ((256 >> k).max(1), (64 >> k).max(1));
                        vec![k as u8 + 7; TextureFormat::Bc7Srgb.level_bytes(w, h)]
                    })
                    .collect();
                t
            }],
            meshes: vec![
                quad.clone(),
                CookedMesh {
                    lods: Vec::new(),
                    ..quad
                },
            ],
            materials: vec![material],
            instances: vec![
                CookedInstance {
                    mesh: 0,
                    material: Some(0),
                    world_from_object: Mat4::from_translation(Vec3::X).to_cols_array(),
                },
                CookedInstance {
                    mesh: 1,
                    material: None,
                    world_from_object: Mat4::IDENTITY.to_cols_array(),
                },
            ],
        }
    }

    #[test]
    fn a_scene_survives_its_pack() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let path =
            std::env::temp_dir().join(format!("engine_assets_cook_{}.pack", std::process::id()));
        for level in [0, 19] {
            let scene = sample();
            let stats = write_pack(&path, &scene, &jobs, level).unwrap();
            // Meshes 2; textures: 512² has 2 large levels + a tail, 2×2 a tail, 256×64
            // 1 large level + a tail; the scene.
            assert_eq!(stats.entries, 2 + 3 + 1 + 2 + 1);
            let (loaded, load) = load_pack(&path, &jobs, 4).unwrap();
            assert_eq!(loaded, scene);
            assert_eq!(load.read_bytes, stats.stored_bytes);
            // File order: meshes, tails, then large levels from the smallest.
            let pack = Pack::open(&path).unwrap();
            let kinds: Vec<(EntryKind, u32)> = pack
                .entries()
                .iter()
                .map(|e| (e.kind(), e.meta[4]))
                .collect();
            assert_eq!(kinds[..2], [(EntryKind::Mesh, 0), (EntryKind::Mesh, 0)]);
            let large: Vec<u64> = pack.entries()[5..8].iter().map(|e| e.bytes).collect();
            assert!(large.is_sorted(), "{large:?}");
            assert_eq!(kinds[8].0, EntryKind::Scene);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_layout_lists_texture_entries_in_file_order() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let path =
            std::env::temp_dir().join(format!("engine_assets_layout_{}.pack", std::process::id()));
        let scene = sample();
        write_pack(&path, &scene, &jobs, 3).unwrap();
        let (_, layout, stats) = open_scene(&path, &jobs, 4).unwrap();
        assert!(stats.bytes > 0);
        assert_eq!(layout.scene.meshes, scene.meshes);
        assert_eq!(layout.scene.materials, scene.materials);
        assert_eq!(layout.scene.instances, scene.instances);
        assert!(layout.scene.textures.is_empty());
        assert_eq!(layout.textures.len(), 3);
        // Three tails, then three large levels (512², its 256², 256×64) smallest first.
        let tails: Vec<bool> = layout
            .texture_entries
            .iter()
            .map(|t| t.is_tail(&layout))
            .collect();
        assert_eq!(tails, [true, true, true, false, false, false]);
        assert!(
            layout
                .texture_entries
                .windows(2)
                .all(|w| w[0].entry < w[1].entry)
        );
        let large: Vec<(u32, u32)> = layout.texture_entries[3..]
            .iter()
            .map(|t| (t.texture, t.first_level))
            .collect();
        assert_eq!(large, [(2, 0), (0, 1), (0, 0)]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bad_entries_are_refused() {
        // Indices past the vertices.
        let mut mesh = sample().meshes[0].clone();
        mesh.indices[2] = 99;
        let entry = mesh_entry(&mesh, 0);
        assert!(read_mesh_entry(entry.meta_for_tests(), &entry.raw_for_tests()).is_err());
        // A texture entry shorter than its levels, or claiming more levels than fit.
        let texture = &sample().textures[0];
        let entry = texture_levels_entry(texture, 2, 8, 0);
        let data = entry.raw_for_tests();
        assert!(read_levels_entry(entry.meta_for_tests(), &data[..40]).is_err());
        assert_eq!(
            read_levels_entry(entry.meta_for_tests(), &data).unwrap(),
            texture.levels[2..]
        );
        let mut meta = entry.meta_for_tests();
        meta[3] = 11;
        assert!(read_levels_entry(meta, &data).is_err());
    }
}
