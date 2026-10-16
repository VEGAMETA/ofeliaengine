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
//! - **mesh stored for streaming** (version 3, ADR 0076): a mesh with detail levels whose
//!   vertices are ordered coarsest level first ([`order_for_streaming`]), so each level
//!   uses a prefix of them. Its mesh entry has `meta[6] = 1` and `meta[5]` the vertices of
//!   the coarsest level: positions and attributes of those, the bounds, a table of
//!   `[index count, error bits, vertices used]` for every level from the full mesh to the
//!   coarsest, then the coarsest level's indices. Each finer level is a **mesh level**
//!   entry, `[level, first vertex, vertices, indices, attributes, meshlets]`: the vertices
//!   it adds (positions, then attributes) and its indices. The scene entry's mesh records
//!   (`meta[3]`) name each mesh's entries.
//! - **texture levels:** `[format code, width, height, levels, first level, level count]`
//!   (width and height of level 0); the levels, each padded to 16 bytes. A texture's
//!   levels of at most [`TAIL_SIZE`] texels share one entry (its tail, written with the
//!   meshes: the whole scene at low resolution); each larger level has its own entry,
//!   written after every tail, smallest first.
//! - **scene:** `[textures, materials, instances]`; [`TextureRecord`]s (which entry holds
//!   each level), [`MaterialRecord`]s, [`InstanceRecord`]s (meshes by entry index; a
//!   skinned instance's skin + 1 in `reserved[0]`).
//!
//! Skinned scenes (pack version 2, ADR 0068) add, after the meshes:
//!
//! - **skin:** `[joints, skeleton bytes, clips]`; the `engine_anim` skeleton
//!   ([`Skeleton::to_bytes`]), the skeleton joint of each skin joint (`u16`), the inverse
//!   bind matrices (16 floats each). Skins are numbered in file order.
//! - **clip:** `[skin, bytes]`; a [`CompressedClip::to_bytes`] of that skin's skeleton.
//!
//! and a skinned mesh entry has attribute bit 8 (influences) with its sets of four in
//! `meta[4]`: after the tangents, `[u16; 4]` joints then `[f32; 4]` weights, `vertices ×
//! sets` each.
//!
//! Meshlets (ADR 0085) need no new version. A mesh or mesh level entry with attribute
//! bit 16 ends, after its other sections, with a section of the meshlet count of each level
//! it holds the indices of (a mesh's full level then its detail levels; the coarsest level
//! of a mesh stored for streaming; a mesh level's own), then one of those levels'
//! [`Meshlet`]s, level after level. A level's meshlets are consecutive ranges of its
//! indices, in order (a level without meshlets counts 0). Engines from before meshlets
//! read the same triangles, reordered meshlet by meshlet, and never reach the tables.

use crate::animation::{raw_clip, shell_distances, skin_skeleton};
use crate::ddc::{DerivedDataCache, import_texture};
use crate::gltf_import::{
    AlphaMode, ImportError, ImportedScene, MaterialTypeData, TextureWrap, read_texture,
};
use crate::lod::{Lod, LodSettings, generate_lods};
use crate::material_file::TextureEncoding;
use crate::meshlet::{Meshlet, build_meshlets, meshlets_cover};
use crate::pack::{
    DEFAULT_CHUNK_SHIFT, EntryData, EntryKind, LoadStats, Pack, PackError, PackStats, PackWriter,
    PreparedEntry, invalid,
};
use crate::texture_compress::{EncodedTexture, TextureFormat};
use crate::texture_import::{ImageError, TextureUsage, build_mips, decode_image};
use engine_anim::{CompressSettings, CompressedClip, CompressionStats, Skeleton, compress};
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
    /// Meshlets of the full mesh (ADR 0085): consecutive ranges of `indices`, in order;
    /// none for a skinned mesh or one cooked without them.
    pub meshlets: Vec<Meshlet>,
    /// Coarser detail levels over the same vertices (ADR 0053).
    pub lods: Vec<Lod>,
    /// Bounds of the positions.
    pub min: [f32; 3],
    /// Bounds of the positions.
    pub max: [f32; 3],
    /// Joint influences of a skinned mesh: joints of its skin, four per set, a vertex's
    /// sets consecutive (`vertices × sets`); empty otherwise.
    pub joints: Vec<[u16; 4]>,
    /// Their weights, normalized per vertex.
    pub weights: Vec<[f32; 4]>,
}

/// A skin with its skeleton and clips (ADR 0068).
#[derive(Debug, Clone, PartialEq)]
pub struct CookedSkin {
    /// The skeleton: the skin's joints and their ancestors; its model space is the
    /// scene's world space.
    pub skeleton: Skeleton,
    /// The skeleton joint of each skin joint (the meshes' joint indices index this).
    pub joints: Vec<u16>,
    /// Inverse bind matrices, per skin joint, column major.
    pub inverse_bind: Vec<[f32; 16]>,
    /// The scene's animations that move this skeleton, compressed.
    pub clips: Vec<CompressedClip>,
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
    /// Object to world, column major (identity for skinned instances: their skeleton
    /// places them).
    pub world_from_object: [f32; 16],
    /// Index into [`CookedScene::skins`] for a skinned mesh.
    pub skin: Option<u32>,
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
    /// Skins, with their skeletons and clips.
    pub skins: Vec<CookedSkin>,
    /// Material types (ADR 0092), with their compiled entry points.
    pub material_types: Vec<MaterialTypeData>,
    /// Materials of those types.
    pub typed_materials: Vec<CookedTypedMaterial>,
    /// Colliders, shared by the bodies (ADR 0101; `crate::colliders`).
    pub colliders: Vec<crate::colliders::CookedCollider>,
    /// Bodies: static level geometry, props, triggers.
    pub bodies: Vec<crate::colliders::CookedBody>,
}

/// A material of a type (ADR 0092): its glTF record in [`CookedScene::materials`] is its
/// fallback.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CookedTypedMaterial {
    /// Index into [`CookedScene::materials`].
    pub material: u32,
    /// Index into [`CookedScene::material_types`].
    pub ty: u32,
    /// The parameter bytes (texture fields hold `u32::MAX` until the material table writes
    /// their bindless references).
    pub params: Vec<u8>,
    /// Texture parameters: byte offset and texture.
    pub textures: Vec<(u32, TextureSlot)>,
}

/// How a texture parameter's image is filtered and compressed.
fn encoding_usage(encoding: TextureEncoding) -> TextureUsage {
    match encoding {
        TextureEncoding::Color => TextureUsage::Color,
        TextureEncoding::Data => TextureUsage::Data,
        TextureEncoding::Normal => TextureUsage::Normal,
    }
}

/// How to cook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CookSettings {
    /// Make detail levels for meshes (ADR 0053).
    pub lods: bool,
    /// Make meshlets for meshes (ADR 0085): each level's triangles reordered meshlet by
    /// meshlet, with a table of their bounds and normal cones. Skinned meshes get none.
    pub meshlets: bool,
    /// zstd level for pack entries (1-22; 0 stores them as they are). 19 by default: the
    /// smallest files for loading speeds that do not change (decoding runs at the same
    /// speed at every level).
    pub compression_level: i32,
    /// Block-compress textures (BC7, BC5; ADR 0041). Off: RGBA8 levels, for comparisons.
    pub block_compression: bool,
    /// Largest error of a compressed clip, in scene units, measured on virtual vertices at
    /// each joint's farthest skinned vertex (ADR 0068; 0.1 mm for meters).
    pub animation_error: f32,
    /// Make colliders and bodies (ADR 0101) by the scene's [`ImportedScene::collider_rules`]
    /// and its glTF physics.
    pub colliders: bool,
}

impl Default for CookSettings {
    fn default() -> Self {
        Self {
            lods: true,
            meshlets: true,
            compression_level: 19,
            block_compression: true,
            animation_error: 1e-4,
            colliders: true,
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
    for t in &scene.typed_materials {
        for &(_, texture, encoding) in &t.textures {
            add(Some(texture), encoding_usage(encoding));
        }
    }
    uses
}

/// Weights scaled to sum to 1 per vertex (all on the first joint when they sum to 0).
fn normalized_weights(weights: &[[f32; 4]], sets: usize) -> Vec<[f32; 4]> {
    if sets == 0 {
        return Vec::new();
    }
    let mut out = weights.to_vec();
    for vertex in out.chunks_mut(sets) {
        let total: f32 = vertex.iter().flatten().map(|w| w.max(0.0)).sum();
        for (k, set) in vertex.iter_mut().enumerate() {
            for (i, w) in set.iter_mut().enumerate() {
                *w = if total > 0.0 {
                    w.max(0.0) / total
                } else {
                    f32::from(u8::from(k == 0 && i == 0))
                };
            }
        }
    }
    out
}

/// What [`cook_scene`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CookStats {
    /// Textures taken from the derived data cache.
    pub cache_hits: usize,
    /// Seconds spent on textures.
    pub texture_seconds: f64,
    /// Seconds spent on meshes (detail levels and meshlets).
    pub mesh_seconds: f64,
    /// Meshlets made, over every mesh and level.
    pub meshlets: usize,
    /// Seconds spent compressing clips.
    pub animation_seconds: f64,
    /// Clips compressed: source and compressed bytes, largest error.
    pub clips: usize,
    /// Bytes of the clips' source keys.
    pub clip_source_bytes: usize,
    /// Bytes of the compressed clips.
    pub clip_bytes: usize,
    /// Largest error of any clip.
    pub clip_error: f32,
    /// Seconds spent on colliders (ADR 0101).
    pub collider_seconds: f64,
    /// Meshes split into convex parts, and how many of them came from the cache.
    pub colliders: crate::colliders::ColliderStats,
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
            let mut mesh = CookedMesh {
                positions: p.positions.clone(),
                normals: p.normals.clone(),
                uvs: p.uvs.clone(),
                tangents: p.tangents.clone(),
                indices: p.indices.clone(),
                meshlets: Vec::new(),
                lods,
                min: p.min,
                max: p.max,
                joints: p.joints.clone(),
                weights: normalized_weights(&p.weights, p.influence_sets()),
            };
            // Meshlets per level (ADR 0085), before the vertices are ordered for streaming
            // so that their first use follows the meshlets. Skinned meshes get none: their
            // bounds move.
            if settings.meshlets && mesh.joints.is_empty() {
                mesh.meshlets = build_meshlets(&mesh.positions, &mut mesh.indices);
                for l in &mut mesh.lods {
                    l.meshlets = build_meshlets(&mesh.positions, &mut l.indices);
                }
            }
            // Levels stream from the coarsest up (ADR 0076): each uses a prefix of the
            // vertices. Skinned meshes are not streamed.
            if !mesh.lods.is_empty() && mesh.joints.is_empty() {
                order_for_streaming(&mut mesh);
            }
            let _ = meshes[k].set(mesh);
        }
    });
    cooked.meshes = meshes
        .into_iter()
        .filter_map(OnceLock::into_inner)
        .collect();
    stats.mesh_seconds = start.elapsed().as_secs_f64();
    stats.meshlets = cooked
        .meshes
        .iter()
        .map(|m| m.meshlets.len() + m.lods.iter().map(|l| l.meshlets.len()).sum::<usize>())
        .sum();

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
    cooked.material_types = scene.material_types.clone();
    cooked.typed_materials = scene
        .typed_materials
        .iter()
        .map(|t| CookedTypedMaterial {
            material: t.material as u32,
            ty: t.ty as u32,
            params: t.params.clone(),
            textures: t
                .textures
                .iter()
                .filter_map(|&(offset, texture, encoding)| {
                    Some((offset, slot(Some(texture), encoding_usage(encoding))?))
                })
                .collect(),
        })
        .collect();

    // Skins: skeletons, and every animation that moves one, compressed in parallel.
    let start = std::time::Instant::now();
    let skeletons = (0..scene.skins.len())
        .map(|k| skin_skeleton(scene, k))
        .collect::<Result<Vec<_>, _>>()
        .map_err(CookError::Import)?;
    let jobs_list: Vec<(usize, usize)> = (0..skeletons.len())
        .flat_map(|k| (0..scene.animations.len()).map(move |a| (k, a)))
        .collect();
    let compressed: Vec<OnceLock<Option<(CompressedClip, CompressionStats)>>> =
        jobs_list.iter().map(|_| OnceLock::new()).collect();
    let shells: Vec<Vec<f32>> = skeletons
        .iter()
        .enumerate()
        .map(|(k, s)| shell_distances(scene, k, s))
        .collect();
    jobs.parallel_for(jobs_list.len(), 1, |_, range| {
        for i in range {
            let (k, a) = jobs_list[i];
            let clip = raw_clip(scene, a, &skeletons[k]).map(|raw| {
                let clip_settings = CompressSettings {
                    error_threshold: settings.animation_error,
                    joint_shell_distances: shells[k].clone(),
                    ..CompressSettings::default()
                };
                compress(&raw, &skeletons[k].skeleton, &clip_settings)
            });
            let _ = compressed[i].set(clip);
        }
    });
    cooked.skins = skeletons
        .into_iter()
        .map(|s| CookedSkin {
            skeleton: s.skeleton,
            joints: s.skin_joints,
            inverse_bind: s
                .inverse_bind
                .iter()
                .map(|m| Mat4::from(*m).to_cols_array())
                .collect(),
            clips: Vec::new(),
        })
        .collect();
    for ((k, _), clip) in jobs_list.iter().zip(compressed) {
        if let Some((clip, clip_stats)) = clip.into_inner().flatten() {
            stats.clips += 1;
            stats.clip_source_bytes += clip_stats.source_bytes;
            stats.clip_bytes += clip_stats.compressed_bytes;
            stats.clip_error = stats.clip_error.max(clip_stats.max_error);
            cooked.skins[*k].clips.push(clip);
        }
    }
    stats.animation_seconds = start.elapsed().as_secs_f64();

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
    // Each node's instances (first, count), for the bodies that move them.
    let mut node_instances = vec![(0u32, 0u32); scene.nodes.len()];
    while let Some((index, parent)) = stack.pop() {
        let node = &scene.nodes[index];
        let world = parent
            * Mat4::from_scale_rotation_translation(
                Vec3::from(node.scale),
                Quat::from_array(node.rotation),
                Vec3::from(node.translation),
            );
        let first = cooked.instances.len() as u32;
        // Authored collision meshes (`UCX_` and kin) are not drawn.
        if let Some(mesh) = node
            .mesh
            .filter(|_| !crate::colliders::is_collision_node(&node.name))
        {
            // A skinned mesh is placed by its skeleton (glTF ignores its node's
            // transform), when it has influences.
            let skin = node.skin.filter(|&k| k < cooked.skins.len());
            for (p, primitive) in scene.meshes[mesh].primitives.iter().enumerate() {
                let skinned = skin.filter(|_| !primitive.joints.is_empty());
                cooked.instances.push(CookedInstance {
                    mesh: first_primitive[mesh] + p as u32,
                    material: primitive.material.map(|m| m as u32),
                    world_from_object: if skinned.is_some() {
                        Mat4::IDENTITY.to_cols_array()
                    } else {
                        world.to_cols_array()
                    },
                    skin: skinned.map(|k| k as u32),
                });
            }
        }
        node_instances[index] = (first, cooked.instances.len() as u32 - first);
        stack.extend(node.children.iter().map(|&c| (c, world)));
    }
    if settings.colliders {
        let start = std::time::Instant::now();
        (cooked.colliders, cooked.bodies, stats.colliders) = crate::colliders::cook_colliders(
            scene,
            &node_instances,
            &scene.collider_rules,
            cache,
            Some(jobs),
        );
        stats.collider_seconds = start.elapsed().as_secs_f64();
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
    /// `reserved[0]`: the skin + 1 of a skinned instance (0: none; version 2); the rest 0.
    pub reserved: [u32; 2],
    /// Object to world, column major.
    pub world_from_object: [f32; 16],
}

const _: () = assert!(size_of::<InstanceRecord>() == 80);

/// A material of a type in a scene entry (version 5, ADR 0092).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct TypedMaterialRecord {
    /// Material index in the scene (its glTF record, the fallback).
    material: u32,
    /// Its type: the `ty`th material type entry.
    ty: u32,
    /// Its parameter bytes in the scene entry's parameter section.
    params_offset: u32,
    params_bytes: u32,
    /// Its texture parameters in the scene entry's [`ParamTextureRecord`]s.
    first_texture: u32,
    texture_count: u32,
    reserved: [u32; 2],
}

const _: () = assert!(size_of::<TypedMaterialRecord>() == 32);

/// A texture parameter of a material of a type (version 5, ADR 0092).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct ParamTextureRecord {
    /// Byte offset in the parameters.
    offset: u32,
    /// Index of the scene's texture records.
    texture: u32,
    /// Wrap modes: u | v << 8.
    wrap: u32,
    reserved: u32,
}

const _: () = assert!(size_of::<ParamTextureRecord>() == 16);

/// A material type entry: `meta = [entries, module bytes, type name bytes]`; the module's
/// and the type's names (each padded to 4 bytes), then per entry point `[name bytes,
/// SPIR-V words]`, its name padded to 4, its SPIR-V.
fn material_type_entry(ty: &MaterialTypeData) -> ([u32; 7], Vec<u8>) {
    fn string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(s.as_bytes());
        out.resize(out.len().next_multiple_of(4), 0);
    }
    let mut data = Vec::new();
    string(&mut data, &ty.module);
    string(&mut data, &ty.type_name);
    for (name, spirv) in &ty.entries {
        data.extend_from_slice(&(name.len() as u32).to_le_bytes());
        data.extend_from_slice(&(spirv.len() as u32).to_le_bytes());
        string(&mut data, name);
        data.extend_from_slice(bytemuck::cast_slice(spirv));
    }
    let meta = [
        ty.entries.len() as u32,
        ty.module.len() as u32,
        ty.type_name.len() as u32,
        0,
        0,
        0,
        0,
    ];
    (meta, data)
}

/// Reads a material type entry ([`material_type_entry`]).
fn read_material_type_entry(meta: [u32; 7], data: &[u8]) -> Result<MaterialTypeData, PackError> {
    let mut at = 0usize;
    let mut take = |len: usize| -> Result<&[u8], PackError> {
        let bytes = data
            .get(
                at..at
                    .checked_add(len)
                    .ok_or_else(|| invalid("material type entry"))?,
            )
            .ok_or_else(|| invalid("material type entry truncated"))?;
        at += len.next_multiple_of(4);
        Ok(bytes)
    };
    let text = |b: &[u8]| {
        String::from_utf8(b.to_vec()).map_err(|_| invalid("material type name is not UTF-8"))
    };
    let module = text(take(meta[1] as usize)?)?;
    let type_name = text(take(meta[2] as usize)?)?;
    let mut entries = Vec::new();
    for _ in 0..meta[0] {
        let counts = take(8)?;
        let name_len = u32::from_le_bytes([counts[0], counts[1], counts[2], counts[3]]) as usize;
        let words = u32::from_le_bytes([counts[4], counts[5], counts[6], counts[7]]) as usize;
        let name = text(take(name_len)?)?;
        let bytes = take(
            words
                .checked_mul(4)
                .ok_or_else(|| invalid("material type entry"))?,
        )?;
        let spirv = bytes
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        entries.push((name, spirv));
    }
    Ok(MaterialTypeData {
        module,
        type_name,
        entries,
    })
}

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
    let sets = m.joints.len() / m.positions.len().max(1);
    if sets > 0 {
        attributes |= 8;
        section(&mut data, bytemuck::cast_slice(&m.joints));
        section(&mut data, bytemuck::cast_slice(&m.weights));
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
    let levels: Vec<&[Meshlet]> = core::iter::once(m.meshlets.as_slice())
        .chain(m.lods.iter().map(|l| l.meshlets.as_slice()))
        .collect();
    attributes |= meshlet_sections(&mut data, &levels);
    let meta = [
        m.positions.len() as u32,
        m.indices.len() as u32,
        m.lods.len() as u32,
        attributes,
        sets as u32,
        0,
        0,
    ];
    PreparedEntry::new(EntryKind::Mesh, meta, &data, level, DEFAULT_CHUNK_SHIFT)
}

/// The attribute bit of mesh and mesh level entries that end with meshlet tables.
const MESHLETS: u32 = 16;

/// Appends the meshlet tables of `levels` when any has meshlets (see the module
/// documentation); returns the attribute bit.
fn meshlet_sections(data: &mut Vec<u8>, levels: &[&[Meshlet]]) -> u32 {
    if levels.iter().all(|l| l.is_empty()) {
        return 0;
    }
    let counts: Vec<u32> = levels.iter().map(|l| l.len() as u32).collect();
    section(data, bytemuck::cast_slice(&counts));
    let meshlets: Vec<Meshlet> = levels.iter().flat_map(|l| l.iter().copied()).collect();
    section(data, bytemuck::cast_slice(&meshlets));
    MESHLETS
}

/// Reads the meshlet tables of levels with `index_counts` indices when the entry has them
/// (`present`), each checked to cover its level; otherwise none per level.
fn read_meshlets(
    s: &mut Sections<'_>,
    present: bool,
    index_counts: &[usize],
) -> Result<Vec<Vec<Meshlet>>, PackError> {
    if !present {
        return Ok(vec![Vec::new(); index_counts.len()]);
    }
    let counts = s.next::<u32>(index_counts.len())?;
    let total = counts
        .iter()
        .try_fold(0usize, |sum, &c| sum.checked_add(c as usize))
        .ok_or_else(|| invalid("too many meshlets"))?;
    let meshlets = s.next::<Meshlet>(total)?;
    let mut levels = Vec::with_capacity(counts.len());
    let mut at = 0;
    for (&count, &index_count) in counts.iter().zip(index_counts) {
        let level = &meshlets[at..at + count as usize];
        at += count as usize;
        if !level.is_empty() && !meshlets_cover(level, index_count) {
            return Err(invalid("meshlets do not cover their level"));
        }
        levels.push(level.to_vec());
    }
    Ok(levels)
}

/// Orders a mesh's vertices for streaming its detail levels (ADR 0076): by first use in
/// the levels from the coarsest to the full mesh (meshoptimizer's advice for levels that
/// share a vertex buffer), so every level uses a prefix of the vertices and a coarser
/// level a shorter one. The triangles are the same; only the vertices' numbering moves.
/// Vertices no level uses go last.
pub fn order_for_streaming(mesh: &mut CookedMesh) {
    let n = mesh.positions.len();
    let mut remap = vec![u32::MAX; n];
    let mut next = 0u32;
    let levels = mesh
        .lods
        .iter()
        .rev()
        .map(|l| l.indices.as_slice())
        .chain(core::iter::once(mesh.indices.as_slice()));
    for indices in levels {
        for &i in indices {
            if let Some(r) = remap.get_mut(i as usize)
                && *r == u32::MAX
            {
                *r = next;
                next += 1;
            }
        }
    }
    for r in &mut remap {
        if *r == u32::MAX {
            *r = next;
            next += 1;
        }
    }
    fn permute<T: Copy + Default>(values: &mut Vec<T>, remap: &[u32], per_vertex: usize) {
        if values.is_empty() {
            return;
        }
        let mut out = vec![T::default(); values.len()];
        for (old, &new) in remap.iter().enumerate() {
            for k in 0..per_vertex {
                if let (Some(&v), Some(slot)) = (
                    values.get(old * per_vertex + k),
                    out.get_mut(new as usize * per_vertex + k),
                ) {
                    *slot = v;
                }
            }
        }
        *values = out;
    }
    let sets = mesh.joints.len() / n.max(1);
    permute(&mut mesh.positions, &remap, 1);
    permute(&mut mesh.normals, &remap, 1);
    permute(&mut mesh.uvs, &remap, 1);
    permute(&mut mesh.tangents, &remap, 1);
    permute(&mut mesh.joints, &remap, sets.max(1));
    permute(&mut mesh.weights, &remap, sets.max(1));
    for i in mesh
        .indices
        .iter_mut()
        .chain(mesh.lods.iter_mut().flat_map(|l| l.indices.iter_mut()))
    {
        *i = remap.get(*i as usize).copied().unwrap_or(*i);
    }
}

/// The vertices each level of a mesh uses, from the full mesh to the coarsest: one more
/// than the largest index of that level and the coarser ones, when the vertices are
/// ordered for streaming (each level then uses exactly that prefix); `None` when they are
/// not, or the mesh has no levels or is skinned (skinned meshes are not streamed).
pub fn streamed_vertex_counts(mesh: &CookedMesh) -> Option<Vec<u32>> {
    if mesh.lods.is_empty() || !mesh.joints.is_empty() {
        return None;
    }
    let levels: Vec<&[u32]> = core::iter::once(mesh.indices.as_slice())
        .chain(mesh.lods.iter().map(|l| l.indices.as_slice()))
        .collect();
    let mut counts = vec![0u32; levels.len()];
    let mut seen = vec![false; mesh.positions.len()];
    let (mut largest, mut distinct) = (0u32, 0u32);
    for k in (0..levels.len()).rev() {
        for &i in levels[k] {
            let slot = seen.get_mut(i as usize)?;
            if !*slot {
                *slot = true;
                distinct += 1;
            }
            largest = largest.max(i + 1);
        }
        // A prefix: every vertex below the largest is used.
        if largest != distinct {
            return None;
        }
        counts[k] = largest;
    }
    Some(counts)
}

/// Appends the attribute sections of vertices `range` of `m` (positions, then the
/// attributes present); returns the attribute bits.
fn vertex_sections(data: &mut Vec<u8>, m: &CookedMesh, range: core::ops::Range<usize>) -> u32 {
    let mut attributes = 0;
    section(data, bytemuck::cast_slice(&m.positions[range.clone()]));
    if !m.normals.is_empty() {
        attributes |= 1;
        section(data, bytemuck::cast_slice(&m.normals[range.clone()]));
    }
    if !m.uvs.is_empty() {
        attributes |= 2;
        section(data, bytemuck::cast_slice(&m.uvs[range.clone()]));
    }
    if !m.tangents.is_empty() {
        attributes |= 4;
        section(data, bytemuck::cast_slice(&m.tangents[range]));
    }
    attributes
}

/// The entry of a mesh stored for streaming: its coarsest level (see the module
/// documentation).
fn streamed_mesh_entry(m: &CookedMesh, counts: &[u32], level: i32) -> PreparedEntry {
    let coarsest = counts.len() - 1;
    let vertices = counts[coarsest] as usize;
    let mut data = Vec::new();
    let mut attributes = vertex_sections(&mut data, m, 0..vertices);
    section(&mut data, bytemuck::cast_slice(&[m.min, m.max]));
    let table: Vec<u32> = core::iter::once((m.indices.len(), 0.0f32))
        .chain(m.lods.iter().map(|l| (l.indices.len(), l.error)))
        .zip(counts)
        .flat_map(|((count, error), &v)| [count as u32, error.to_bits(), v])
        .collect();
    section(&mut data, bytemuck::cast_slice(&table));
    section(
        &mut data,
        bytemuck::cast_slice(&m.lods[coarsest - 1].indices),
    );
    attributes |= meshlet_sections(&mut data, &[&m.lods[coarsest - 1].meshlets]);
    let meta = [
        m.positions.len() as u32,
        m.indices.len() as u32,
        m.lods.len() as u32,
        attributes,
        0,
        vertices as u32,
        1,
    ];
    PreparedEntry::new(EntryKind::Mesh, meta, &data, level, DEFAULT_CHUNK_SHIFT)
}

/// The entry of a finer level `k` (0: the full mesh) of a mesh stored for streaming.
fn mesh_level_entry(m: &CookedMesh, counts: &[u32], k: usize, level: i32) -> PreparedEntry {
    let (first, end) = (counts[k + 1] as usize, counts[k] as usize);
    let (indices, meshlets) = if k == 0 {
        (&m.indices, &m.meshlets)
    } else {
        (&m.lods[k - 1].indices, &m.lods[k - 1].meshlets)
    };
    let mut data = Vec::new();
    let mut attributes = vertex_sections(&mut data, m, first..end);
    section(&mut data, bytemuck::cast_slice(indices));
    attributes |= meshlet_sections(&mut data, &[meshlets]);
    let meta = [
        k as u32,
        first as u32,
        (end - first) as u32,
        indices.len() as u32,
        attributes,
        meshlets.len() as u32,
        0,
    ];
    PreparedEntry::new(
        EntryKind::MeshLevel,
        meta,
        &data,
        level,
        DEFAULT_CHUNK_SHIFT,
    )
}

/// A finer detail level of a mesh stored for streaming, as its entry holds it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MeshLevelData {
    /// The level (0: the full mesh).
    pub level: u32,
    /// Its first new vertex: the coarser levels use the vertices before.
    pub first_vertex: u32,
    /// Positions of the new vertices.
    pub positions: Vec<[f32; 3]>,
    /// Their normals (or none).
    pub normals: Vec<[f32; 3]>,
    /// Their texture coordinates (or none).
    pub uvs: Vec<[f32; 2]>,
    /// Their tangents (or none).
    pub tangents: Vec<[f32; 4]>,
    /// The level's triangles, over every vertex up to the new ones.
    pub indices: Vec<u32>,
    /// The level's meshlets (ADR 0085): consecutive ranges of `indices`, or none.
    pub meshlets: Vec<Meshlet>,
}

/// Reads a mesh level entry ([`EntryKind::MeshLevel`]) from its meta and raw bytes.
pub fn read_mesh_level_entry(meta: [u32; 7], data: &[u8]) -> Result<MeshLevelData, PackError> {
    let [level, first, vertices, index_count, attributes, ..] = meta;
    let n = vertices as usize;
    let mut s = Sections { data, at: 0 };
    let positions = s.next::<[f32; 3]>(n)?;
    let normals = if attributes & 1 != 0 {
        s.next(n)?
    } else {
        Vec::new()
    };
    let uvs = if attributes & 2 != 0 {
        s.next(n)?
    } else {
        Vec::new()
    };
    let tangents = if attributes & 4 != 0 {
        s.next(n)?
    } else {
        Vec::new()
    };
    let indices = s.next::<u32>(index_count as usize)?;
    let end = first
        .checked_add(vertices)
        .ok_or_else(|| invalid("mesh level vertices out of range"))?;
    if indices.iter().any(|&i| i >= end) {
        return Err(invalid("mesh level index out of range"));
    }
    let meshlets = read_meshlets(&mut s, attributes & MESHLETS != 0, &[indices.len()])?
        .into_iter()
        .next()
        .unwrap_or_default();
    // The count is in the meta too: what a reader sizes the level's decoded form by.
    if meta[5] as usize != meshlets.len() {
        return Err(invalid("mesh level meshlets differ from its meta"));
    }
    Ok(MeshLevelData {
        level,
        first_vertex: first,
        positions,
        normals,
        uvs,
        tangents,
        indices,
        meshlets,
    })
}

/// Where a mesh stored for streaming keeps each of its levels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshLevelRecord {
    /// The entry holding the level's new vertices and indices (the coarsest level is in
    /// the mesh's own entry).
    pub entry: u32,
    /// Its indices.
    pub index_count: u32,
    /// Its error in object units (0 for the full mesh).
    pub error: f32,
    /// The vertices it uses (a prefix).
    pub vertices: u32,
}

/// A mesh stored for streaming (ADR 0076): its levels from the full mesh to the coarsest.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamedMesh {
    /// Every level, the full mesh first; the last is the coarsest, held by the mesh
    /// itself in [`SceneLayout::scene`].
    pub levels: Vec<MeshLevelRecord>,
}

/// Records of a scene's meshes in its scene entry: each mesh's entry and the entries of
/// its finer levels when stored for streaming (`u32::MAX`: none).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MeshRecord {
    /// The mesh entry.
    pub entry: u32,
    /// The entries of levels 0..3 (the full mesh first) stored apart.
    pub levels: [u32; 4],
    /// 0.
    pub reserved: [u32; 3],
}

const _: () = assert!(size_of::<MeshRecord>() == 32);

fn skin_entry(skin: &CookedSkin, level: i32) -> PreparedEntry {
    let skeleton = skin.skeleton.to_bytes();
    let mut data = Vec::new();
    section(&mut data, bytemuck::cast_slice(&skin.joints));
    section(&mut data, &skeleton);
    section(&mut data, bytemuck::cast_slice(&skin.inverse_bind));
    let meta = [
        skin.joints.len() as u32,
        skeleton.len() as u32,
        skin.clips.len() as u32,
        0,
        0,
        0,
        0,
    ];
    PreparedEntry::new(EntryKind::Skin, meta, &data, level, DEFAULT_CHUNK_SHIFT)
}

fn clip_entry(skin: u32, clip: &CompressedClip, level: i32) -> PreparedEntry {
    let bytes = clip.to_bytes();
    let meta = [skin, bytes.len() as u32, 0, 0, 0, 0, 0];
    PreparedEntry::new(EntryKind::Clip, meta, &bytes, level, DEFAULT_CHUNK_SHIFT)
}

/// A part of the scene stored as one entry.
#[derive(Debug, Clone, Copy)]
enum Piece {
    Mesh(usize),
    /// Mesh, finer level (stored for streaming).
    MeshLevel(usize, u32),
    Skin(usize),
    /// Skin, clip.
    Clip(usize, usize),
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
    // Meshes whose vertices are ordered for streaming are stored by level (version 3).
    let streamed: Vec<Option<Vec<u32>>> = scene.meshes.iter().map(streamed_vertex_counts).collect();
    let mut pieces: Vec<Piece> = (0..scene.meshes.len()).map(Piece::Mesh).collect();
    for (k, skin) in scene.skins.iter().enumerate() {
        pieces.push(Piece::Skin(k));
        pieces.extend((0..skin.clips.len()).map(|c| Piece::Clip(k, c)));
    }
    let mut large = Vec::new();
    for (t, texture) in scene.textures.iter().enumerate() {
        let levels = texture.levels.len() as u32;
        let tail = first_tail_level(texture.width, texture.height, levels);
        pieces.push(Piece::Levels(t, tail, levels - tail));
        large.extend((0..tail).map(|k| Piece::Levels(t, k, 1)));
    }
    for (m, counts) in streamed.iter().enumerate() {
        if let Some(counts) = counts {
            large.extend((0..counts.len() as u32 - 1).map(|k| Piece::MeshLevel(m, k)));
        }
    }
    let bytes = |p: &Piece| match *p {
        Piece::Levels(t, k, _) => scene.textures[t].levels[k as usize].len(),
        Piece::MeshLevel(m, k) => streamed[m].as_ref().map_or(0, |c| {
            let indices = if k == 0 {
                scene.meshes[m].indices.len()
            } else {
                scene.meshes[m].lods[k as usize - 1].indices.len()
            };
            (c[k as usize] - c[k as usize + 1]) as usize * 60 + indices * 4
        }),
        Piece::Mesh(_) | Piece::Skin(_) | Piece::Clip(..) => 0,
    };
    large.sort_by_key(bytes);
    pieces.extend(large);

    // Compressing is the slow part: every entry in parallel.
    let prepared: Vec<OnceLock<PreparedEntry>> = pieces.iter().map(|_| OnceLock::new()).collect();
    jobs.parallel_for(pieces.len(), 1, |_, range| {
        for k in range {
            let entry = match pieces[k] {
                Piece::Mesh(m) => match &streamed[m] {
                    Some(counts) => streamed_mesh_entry(&scene.meshes[m], counts, level),
                    None => mesh_entry(&scene.meshes[m], level),
                },
                Piece::MeshLevel(m, k) => match &streamed[m] {
                    Some(counts) => mesh_level_entry(&scene.meshes[m], counts, k as usize, level),
                    None => mesh_entry(&scene.meshes[m], level),
                },
                Piece::Skin(k) => skin_entry(&scene.skins[k], level),
                Piece::Clip(k, c) => clip_entry(k as u32, &scene.skins[k].clips[c], level),
                Piece::Levels(t, first, count) => {
                    texture_levels_entry(&scene.textures[t], first, count, level)
                }
            };
            let _ = prepared[k].set(entry);
        }
    });
    let mut writer = PackWriter::create(path, DEFAULT_CHUNK_SHIFT)?;
    let mut mesh_entries = vec![NONE; scene.meshes.len()];
    let mut mesh_levels = vec![[NONE; 4]; scene.meshes.len()];
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
            Piece::MeshLevel(m, k) => {
                if let Some(slot) = mesh_levels[m].get_mut(k as usize) {
                    *slot = index;
                }
            }
            Piece::Skin(_) | Piece::Clip(..) => {}
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
            reserved: [i.skin.map_or(0, |k| k + 1), 0],
            world_from_object: i.world_from_object,
        })
        .collect();
    // Mesh records, when a mesh is stored for streaming (version 3).
    let mesh_records: Vec<MeshRecord> = if streamed.iter().any(Option::is_some) {
        mesh_entries
            .iter()
            .zip(&mesh_levels)
            .map(|(&entry, &levels)| MeshRecord {
                entry,
                levels,
                reserved: [0; 3],
            })
            .collect()
    } else {
        Vec::new()
    };
    // Material types (version 5, ADR 0092): an entry each, in order.
    for ty in &scene.material_types {
        let (meta, data) = material_type_entry(ty);
        writer.add(&PreparedEntry::new(
            EntryKind::MaterialType,
            meta,
            &data,
            level,
            DEFAULT_CHUNK_SHIFT,
        ))?;
    }
    let mut typed = Vec::new();
    let mut param_textures = Vec::new();
    let mut param_bytes = Vec::new();
    for t in &scene.typed_materials {
        typed.push(TypedMaterialRecord {
            material: t.material,
            ty: t.ty,
            params_offset: param_bytes.len() as u32,
            params_bytes: t.params.len() as u32,
            first_texture: param_textures.len() as u32,
            texture_count: t.textures.len() as u32,
            reserved: [0; 2],
        });
        param_bytes.extend_from_slice(&t.params);
        param_bytes.resize(param_bytes.len().next_multiple_of(4), 0);
        param_textures.extend(t.textures.iter().map(|&(offset, s)| ParamTextureRecord {
            offset,
            texture: s.texture,
            wrap: wrap_code(s.wrap[0]) | wrap_code(s.wrap[1]) << 8,
            reserved: 0,
        }));
    }
    // Colliders and bodies (version 6, ADR 0101): an entry per collider, in order, then one
    // for the bodies.
    for c in &scene.colliders {
        let (meta, data) = crate::colliders::collider_entry(c);
        writer.add(&PreparedEntry::new(
            EntryKind::Collider,
            meta,
            &data,
            level,
            DEFAULT_CHUNK_SHIFT,
        ))?;
    }
    if !scene.bodies.is_empty() {
        let (meta, data) = crate::colliders::bodies_entry(&scene.bodies);
        writer.add(&PreparedEntry::new(
            EntryKind::Bodies,
            meta,
            &data,
            level,
            DEFAULT_CHUNK_SHIFT,
        ))?;
    }
    let mut data = Vec::new();
    section(&mut data, bytemuck::cast_slice(&records));
    section(&mut data, bytemuck::cast_slice(&materials));
    section(&mut data, bytemuck::cast_slice(&instances));
    section(&mut data, bytemuck::cast_slice(&mesh_records));
    section(&mut data, bytemuck::cast_slice(&typed));
    section(&mut data, bytemuck::cast_slice(&param_textures));
    section(&mut data, &param_bytes);
    let meta = [
        records.len() as u32,
        materials.len() as u32,
        instances.len() as u32,
        mesh_records.len() as u32,
        typed.len() as u32,
        param_textures.len() as u32,
        param_bytes.len() as u32,
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

/// A mesh entry: the whole mesh, or for a mesh stored for streaming its coarsest level
/// (the mesh's vertices that level uses, its indices and meshlets, no `lods`) with every
/// level's record (entries not filled in).
fn read_mesh_entry_parts(
    meta: [u32; 7],
    data: &[u8],
) -> Result<(CookedMesh, Option<StreamedMesh>), PackError> {
    if meta[6] != 1 {
        return read_mesh_entry(meta, data).map(|m| (m, None));
    }
    let [total, _, lod_count, attributes, _, coarse, _] = meta.map(|m| m as usize);
    if lod_count == 0 || lod_count > 3 || coarse > total {
        return Err(invalid("bad streamed mesh"));
    }
    let mut s = Sections { data, at: 0 };
    let positions = s.next::<[f32; 3]>(coarse)?;
    let normals = if attributes & 1 != 0 {
        s.next(coarse)?
    } else {
        Vec::new()
    };
    let uvs = if attributes & 2 != 0 {
        s.next(coarse)?
    } else {
        Vec::new()
    };
    let tangents = if attributes & 4 != 0 {
        s.next(coarse)?
    } else {
        Vec::new()
    };
    let bounds = s.next::<[f32; 3]>(2)?;
    let table = s.next::<[u32; 3]>(lod_count + 1)?;
    let indices = s.next::<u32>(table[lod_count][0] as usize)?;
    if indices.iter().any(|&i| i as usize >= coarse)
        || table[lod_count][2] as usize != coarse
        || table[0][2] as usize != total
        || table.windows(2).any(|w| w[1][2] > w[0][2])
    {
        return Err(invalid("bad streamed mesh levels"));
    }
    let meshlets = read_meshlets(
        &mut s,
        attributes & MESHLETS as usize != 0,
        &[indices.len()],
    )?
    .into_iter()
    .next()
    .unwrap_or_default();
    let levels = table
        .iter()
        .map(|&[index_count, error, vertices]| MeshLevelRecord {
            entry: NONE,
            index_count,
            error: f32::from_bits(error),
            vertices,
        })
        .collect();
    Ok((
        CookedMesh {
            positions,
            normals,
            uvs,
            tangents,
            indices,
            meshlets,
            lods: Vec::new(),
            min: bounds[0],
            max: bounds[1],
            joints: Vec::new(),
            weights: Vec::new(),
        },
        Some(StreamedMesh { levels }),
    ))
}

/// A whole mesh from its coarsest level and its finer levels' entries (every one).
fn assemble_mesh(
    coarse: CookedMesh,
    streamed: &StreamedMesh,
    finer: &[MeshLevelData],
) -> Result<CookedMesh, PackError> {
    let coarse_indices = coarse.indices.clone();
    let coarse_meshlets = coarse.meshlets.clone();
    let mut mesh = CookedMesh {
        indices: Vec::new(),
        meshlets: Vec::new(),
        ..coarse
    };
    let coarsest = streamed.levels.len() - 1;
    let mut lods: Vec<Lod> = Vec::new();
    // Vertices from the coarsest level's up to the full mesh's.
    for k in (0..coarsest).rev() {
        let level = finer
            .iter()
            .find(|l| l.level == k as u32)
            .ok_or_else(|| invalid("mesh level missing"))?;
        if level.first_vertex as usize != mesh.positions.len()
            || level.first_vertex + level.positions.len() as u32 != streamed.levels[k].vertices
        {
            return Err(invalid("mesh level vertices do not follow"));
        }
        mesh.positions.extend_from_slice(&level.positions);
        mesh.normals.extend_from_slice(&level.normals);
        mesh.uvs.extend_from_slice(&level.uvs);
        mesh.tangents.extend_from_slice(&level.tangents);
    }
    for (k, record) in streamed.levels.iter().enumerate() {
        let (indices, meshlets) = if k == coarsest {
            (coarse_indices.clone(), coarse_meshlets.clone())
        } else {
            finer
                .iter()
                .find(|l| l.level == k as u32)
                .map(|l| (l.indices.clone(), l.meshlets.clone()))
                .ok_or_else(|| invalid("mesh level missing"))?
        };
        if k == 0 {
            mesh.indices = indices;
            mesh.meshlets = meshlets;
        } else {
            lods.push(Lod {
                indices,
                error: record.error,
                meshlets,
            });
        }
    }
    mesh.lods = lods;
    Ok(mesh)
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
    let sets = meta[4] as usize;
    let (joints, weights) = if attributes & 8 != 0 && (1..=8).contains(&sets) {
        let n = vertices
            .checked_mul(sets)
            .ok_or_else(|| invalid("too many influences"))?;
        (s.next::<[u16; 4]>(n)?, s.next::<[f32; 4]>(n)?)
    } else if attributes & 8 != 0 {
        return Err(invalid("bad influence sets"));
    } else {
        (Vec::new(), Vec::new())
    };
    let bounds = s.next::<[f32; 3]>(2)?;
    let indices = s.next::<u32>(index_count)?;
    let table = s.next::<[u32; 2]>(lod_count)?;
    let mut lods = table
        .iter()
        .map(|&[count, error]| {
            Ok(Lod {
                indices: s.next::<u32>(count as usize)?,
                error: f32::from_bits(error),
                meshlets: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, PackError>>()?;
    let out_of_range = |list: &[u32]| list.iter().any(|&i| i as usize >= vertices);
    if out_of_range(&indices) || lods.iter().any(|l| out_of_range(&l.indices)) {
        return Err(invalid("mesh index out of range"));
    }
    let index_counts: Vec<usize> = core::iter::once(indices.len())
        .chain(lods.iter().map(|l| l.indices.len()))
        .collect();
    let mut tables =
        read_meshlets(&mut s, attributes & MESHLETS as usize != 0, &index_counts)?.into_iter();
    let meshlets = tables.next().unwrap_or_default();
    for (l, table) in lods.iter_mut().zip(tables) {
        l.meshlets = table;
    }
    Ok(CookedMesh {
        positions,
        normals,
        uvs,
        tangents,
        indices,
        meshlets,
        lods,
        min: bounds[0],
        max: bounds[1],
        joints,
        weights,
    })
}

fn read_skin_entry(meta: [u32; 7], data: &[u8]) -> Result<CookedSkin, PackError> {
    let [joint_count, skeleton_bytes, ..] = meta.map(|m| m as usize);
    let mut s = Sections { data, at: 0 };
    let joints = s.next::<u16>(joint_count)?;
    let skeleton =
        Skeleton::from_bytes(s.slice(skeleton_bytes)?).map_err(|e| invalid(e.to_string()))?;
    let inverse_bind = s.next::<[f32; 16]>(joint_count)?;
    if joints.iter().any(|&j| j as usize >= skeleton.joint_count()) {
        return Err(invalid("skin joint past its skeleton"));
    }
    Ok(CookedSkin {
        skeleton,
        joints,
        inverse_bind,
        clips: Vec::new(),
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
    /// Per mesh of [`Self::scene`]: its levels' entries when it is stored for streaming
    /// and was opened by [`open_scene_streamed`] (the mesh then holds its coarsest level
    /// only), `None` otherwise.
    pub mesh_streams: Vec<Option<StreamedMesh>>,
}

/// An entry decoded by kind.
enum Decoded {
    Mesh(CookedMesh, Option<StreamedMesh>),
    MeshLevel(MeshLevelData),
    Skin(CookedSkin),
    Clip(u32, CompressedClip),
    Scene(
        Vec<TextureRecord>,
        Vec<MaterialRecord>,
        Vec<InstanceRecord>,
        Vec<MeshRecord>,
        (Vec<TypedMaterialRecord>, Vec<ParamTextureRecord>, Vec<u8>),
    ),
    MaterialType(MaterialTypeData),
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
    open_scene_with(path, jobs, depth, false)
}

/// [`open_scene`], but meshes stored for streaming (version 3, ADR 0076) are loaded with
/// their coarsest level only; [`SceneLayout::mesh_streams`] says where their finer
/// levels are ([`read_mesh_level_entry`] reads them).
pub fn open_scene_streamed(
    path: &Path,
    jobs: &ThreadPool,
    depth: usize,
) -> Result<(Pack, SceneLayout, LoadStats), PackError> {
    open_scene_with(path, jobs, depth, true)
}

fn open_scene_with(
    path: &Path,
    jobs: &ThreadPool,
    depth: usize,
    stream_meshes: bool,
) -> Result<(Pack, SceneLayout, LoadStats), PackError> {
    engine_profiler::zone!("assets::open_scene");
    let pack = Pack::open(path)?;
    let entries = pack.entries();
    let decoded: Vec<Mutex<Option<Decoded>>> = entries.iter().map(|_| Mutex::new(None)).collect();
    let indices: Vec<u32> = (0..entries.len() as u32)
        .filter(|&i| match entries[i as usize].kind() {
            // Panoramas are read on their own (`crate::panorama::load_panorama_pack`), and
            // colliders when a game simulates physics (`crate::colliders::load_colliders`).
            EntryKind::TextureLevels
            | EntryKind::Panorama
            | EntryKind::Collider
            | EntryKind::Bodies => false,
            EntryKind::MeshLevel => !stream_meshes,
            _ => true,
        })
        .collect();
    let stats = pack.load(&indices, jobs, depth, |index, e, data: EntryData| {
        let value = match e.kind() {
            EntryKind::Mesh => {
                let (mesh, streamed) = read_mesh_entry_parts(e.meta, &data)?;
                Decoded::Mesh(mesh, streamed)
            }
            EntryKind::MeshLevel => Decoded::MeshLevel(read_mesh_level_entry(e.meta, &data)?),
            EntryKind::Scene => {
                let mut s = Sections { data: &data, at: 0 };
                let textures = s.next::<TextureRecord>(e.meta[0] as usize)?;
                let materials = s.next::<MaterialRecord>(e.meta[1] as usize)?;
                let instances = s.next::<InstanceRecord>(e.meta[2] as usize)?;
                let meshes = s.next::<MeshRecord>(e.meta[3] as usize)?;
                let typed = s.next::<TypedMaterialRecord>(e.meta[4] as usize)?;
                let param_textures = s.next::<ParamTextureRecord>(e.meta[5] as usize)?;
                let param_bytes = s.next::<u8>(e.meta[6] as usize)?;
                Decoded::Scene(
                    textures,
                    materials,
                    instances,
                    meshes,
                    (typed, param_textures, param_bytes),
                )
            }
            EntryKind::MaterialType => {
                Decoded::MaterialType(read_material_type_entry(e.meta, &data)?)
            }
            EntryKind::Skin => Decoded::Skin(read_skin_entry(e.meta, &data)?),
            EntryKind::Clip => {
                let bytes = data
                    .get(..e.meta[1] as usize)
                    .ok_or_else(|| invalid("clip entry truncated"))?;
                let clip = CompressedClip::from_bytes(bytes).map_err(|e| invalid(e.to_string()))?;
                Decoded::Clip(e.meta[0], clip)
            }
            EntryKind::TextureLevels => return Err(invalid("texture levels loaded early")),
            EntryKind::Panorama => return Err(invalid("a panorama loaded as a scene's entry")),
            EntryKind::Collider | EntryKind::Bodies => {
                return Err(invalid("colliders loaded as a scene's entry"));
            }
        };
        if let Ok(mut slot) = decoded[index as usize].lock() {
            *slot = Some(value);
        }
        Ok(())
    })?;
    let mut layout = SceneLayout::default();
    let mut mesh_of = vec![NONE; entries.len()];
    let mut records = None;
    let mut clips = Vec::new();
    // Finer levels of meshes stored for streaming, by entry.
    let mut levels: Vec<Option<MeshLevelData>> = (0..entries.len()).map(|_| None).collect();
    for (i, slot) in decoded.into_iter().enumerate() {
        match slot.into_inner().ok().flatten() {
            Some(Decoded::Mesh(m, streamed)) => {
                mesh_of[i] = layout.scene.meshes.len() as u32;
                layout.scene.meshes.push(m);
                layout.mesh_streams.push(streamed);
            }
            Some(Decoded::MeshLevel(level)) => levels[i] = Some(level),
            None if entries[i].kind() == EntryKind::MeshLevel && stream_meshes => {}
            Some(Decoded::Skin(skin)) => layout.scene.skins.push(skin),
            Some(Decoded::Clip(skin, clip)) => clips.push((skin, clip)),
            Some(Decoded::Scene(t, m, inst, meshes, typed)) => {
                if records.replace((t, m, inst, meshes, typed)).is_some() {
                    return Err(invalid("more than one scene"));
                }
            }
            Some(Decoded::MaterialType(ty)) => layout.scene.material_types.push(ty),
            None if matches!(
                entries[i].kind(),
                EntryKind::TextureLevels | EntryKind::Collider | EntryKind::Bodies
            ) => {}
            None => return Err(invalid(format!("entry {i} not loaded"))),
        }
    }
    let Some((textures, materials, instances, mesh_records, (typed, param_textures, param_bytes))) =
        records
    else {
        return Err(invalid("no scene"));
    };
    // Meshes stored for streaming: their levels' entries, then whole meshes unless they
    // stream.
    for record in &mesh_records {
        let Some(&m) = mesh_of.get(record.entry as usize).filter(|&&m| m != NONE) else {
            return Err(invalid("mesh record of a missing mesh"));
        };
        let Some(streamed) = layout.mesh_streams[m as usize].as_mut() else {
            continue;
        };
        let coarsest = streamed.levels.len() - 1;
        for k in 0..coarsest {
            let entry = record.levels.get(k).copied().unwrap_or(NONE);
            let fits = entries
                .get(entry as usize)
                .is_some_and(|e| e.kind() == EntryKind::MeshLevel && e.meta[0] == k as u32);
            if !fits {
                return Err(invalid("mesh level in a missing entry"));
            }
            streamed.levels[k].entry = entry;
        }
    }
    if !stream_meshes {
        for (m, streamed) in layout.mesh_streams.iter_mut().enumerate() {
            let Some(s) = streamed.take() else {
                continue;
            };
            let finer: Vec<MeshLevelData> = s.levels[..s.levels.len() - 1]
                .iter()
                .map(|l| {
                    levels
                        .get_mut(l.entry as usize)
                        .and_then(Option::take)
                        .ok_or_else(|| invalid("mesh level not loaded"))
                })
                .collect::<Result<_, _>>()?;
            let coarse = core::mem::take(&mut layout.scene.meshes[m]);
            layout.scene.meshes[m] = assemble_mesh(coarse, &s, &finer)?;
        }
    }
    for (skin, clip) in clips {
        let skin = layout
            .scene
            .skins
            .get_mut(skin as usize)
            .ok_or_else(|| invalid("clip of a missing skin"))?;
        if clip.joint_count() != skin.skeleton.joint_count() {
            return Err(invalid("clip of another skeleton"));
        }
        skin.clips.push(clip);
    }
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
    for t in &typed {
        if t.material as usize >= layout.scene.materials.len()
            || t.ty as usize >= layout.scene.material_types.len()
        {
            return Err(invalid("material of a missing material or type"));
        }
        let params = param_bytes
            .get(
                t.params_offset as usize
                    ..(t.params_offset as usize).saturating_add(t.params_bytes as usize),
            )
            .ok_or_else(|| invalid("material parameters out of bounds"))?
            .to_vec();
        let records = param_textures
            .get(
                t.first_texture as usize
                    ..(t.first_texture as usize).saturating_add(t.texture_count as usize),
            )
            .ok_or_else(|| invalid("material texture parameters out of bounds"))?;
        let mut textures = Vec::with_capacity(records.len());
        for r in records {
            if r.offset as usize + 4 > params.len() {
                return Err(invalid("texture parameter outside its parameters"));
            }
            let s = slot(r.texture, r.wrap)?
                .ok_or_else(|| invalid("texture parameter without a texture"))?;
            textures.push((r.offset, s));
        }
        layout.scene.typed_materials.push(CookedTypedMaterial {
            material: t.material,
            ty: t.ty,
            params,
            textures,
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
        let skin = i.reserved[0].checked_sub(1);
        if skin.is_some_and(|k| k as usize >= layout.scene.skins.len()) {
            return Err(invalid("instance refers to a missing skin"));
        }
        layout.scene.instances.push(CookedInstance {
            mesh,
            material,
            world_from_object: i.world_from_object,
            skin,
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
    (scene.colliders, scene.bodies) = crate::colliders::load_colliders(&pack, jobs, depth)?;
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
            meshlets: Vec::new(),
            lods: vec![Lod {
                indices: vec![0, 1, 2],
                error: 0.25,
                meshlets: Vec::new(),
            }],
            min: [0.0; 3],
            max: [1.0, 1.0, 0.0],
            joints: Vec::new(),
            weights: Vec::new(),
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
                // Without detail levels, with meshlets.
                with_meshlets(CookedMesh {
                    lods: Vec::new(),
                    ..quad
                }),
            ],
            materials: vec![material],
            instances: vec![
                CookedInstance {
                    mesh: 0,
                    material: Some(0),
                    world_from_object: Mat4::from_translation(Vec3::X).to_cols_array(),
                    skin: None,
                },
                CookedInstance {
                    mesh: 1,
                    material: None,
                    world_from_object: Mat4::IDENTITY.to_cols_array(),
                    skin: None,
                },
            ],
            skins: Vec::new(),
            material_types: Vec::new(),
            typed_materials: Vec::new(),
            colliders: Vec::new(),
            bodies: Vec::new(),
        }
    }

    /// `mesh` with meshlets on every level, as the cook makes them.
    fn with_meshlets(mut mesh: CookedMesh) -> CookedMesh {
        mesh.meshlets = build_meshlets(&mesh.positions, &mut mesh.indices);
        for l in &mut mesh.lods {
            l.meshlets = build_meshlets(&mesh.positions, &mut l.indices);
        }
        mesh
    }

    /// The sample with a skinned copy of the quad (two sets of influences), a two-joint
    /// skin and a clip.
    fn skinned_sample() -> CookedScene {
        use engine_anim::{
            CompressSettings, Interpolation, JointDesc, RawClip, RawJointTracks, RawTrack,
            Transform, compress,
        };
        let mut scene = sample();
        let mut mesh = scene.meshes[1].clone();
        // Skinned meshes have no meshlets (the cook makes none).
        mesh.meshlets = Vec::new();
        mesh.joints = [[0, 1, 0, 0], [1, 0, 0, 0]].repeat(4);
        mesh.weights = [[0.5, 0.25, 0.0, 0.0], [0.25, 0.0, 0.0, 0.0]].repeat(4);
        scene.meshes.push(mesh);
        let (skeleton, _) = Skeleton::new(&[
            JointDesc {
                name: "root".into(),
                parent: None,
                rest: Transform::IDENTITY,
            },
            JointDesc {
                name: "tip".into(),
                parent: Some(0),
                rest: Transform::new(Vec3::Y, Quat::IDENTITY, Vec3::ONE),
            },
        ])
        .unwrap();
        let mut raw = RawClip {
            name: "bend".into(),
            duration: 1.0,
            joints: vec![RawJointTracks::default(); 2],
        };
        raw.joints[1].rotation = Some(RawTrack {
            interpolation: Interpolation::Linear,
            times: vec![0.0, 1.0],
            values: vec![Quat::IDENTITY, Quat::from_rotation_z(1.0)],
        });
        let (clip, _) = compress(&raw, &skeleton, &CompressSettings::default());
        scene.skins.push(CookedSkin {
            skeleton,
            joints: vec![0, 1],
            inverse_bind: vec![
                Mat4::IDENTITY.to_cols_array(),
                Mat4::from_translation(-Vec3::Y).to_cols_array(),
            ],
            clips: vec![clip],
        });
        scene.instances.push(CookedInstance {
            mesh: 2,
            material: None,
            world_from_object: Mat4::IDENTITY.to_cols_array(),
            skin: Some(0),
        });
        scene
    }

    #[test]
    fn a_scene_survives_its_pack() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let path =
            std::env::temp_dir().join(format!("engine_assets_cook_{}.pack", std::process::id()));
        for level in [0, 19] {
            let scene = sample();
            let stats = write_pack(&path, &scene, &jobs, level).unwrap();
            // Meshes 2 and the full level of the first (stored for streaming); textures:
            // 512² has 2 large levels + a tail, 2×2 a tail, 256×64 1 large level + a
            // tail; the scene.
            assert_eq!(stats.entries, 3 + 3 + 1 + 2 + 1);
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
            let large: Vec<u64> = pack.entries()[5..9]
                .iter()
                .filter(|e| e.kind() == EntryKind::TextureLevels)
                .map(|e| e.bytes)
                .collect();
            assert_eq!(large.len(), 3);
            assert!(large.is_sorted(), "{large:?}");
            assert!(
                pack.entries()[5..9]
                    .iter()
                    .any(|e| e.kind() == EntryKind::MeshLevel)
            );
            assert_eq!(kinds[9].0, EntryKind::Scene);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn material_types_survive_their_pack_as_version_5() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let path =
            std::env::temp_dir().join(format!("engine_assets_typed_{}.pack", std::process::id()));
        let mut scene = sample();
        scene.material_types = vec![
            MaterialTypeData {
                module: String::from("materials/velvet.slang"),
                type_name: String::from("VelvetMaterial"),
                entries: vec![
                    (String::from("fragmentMain"), vec![0x0723_0203, 1, 2, 3]),
                    (String::from("fragmentMainAo"), vec![0x0723_0203, 4]),
                ],
            },
            MaterialTypeData {
                module: String::from("odd.slang"),
                type_name: String::from("Odd"),
                entries: Vec::new(),
            },
        ];
        scene.typed_materials = vec![CookedTypedMaterial {
            material: 0,
            ty: 0,
            params: (0..44u8).collect(),
            textures: vec![(
                8,
                TextureSlot {
                    texture: 2,
                    wrap: [TextureWrap::ClampToEdge, TextureWrap::Repeat],
                },
            )],
        }];
        write_pack(&path, &scene, &jobs, 3).unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            5
        );
        let (loaded, _) = load_pack(&path, &jobs, 4).unwrap();
        assert_eq!(loaded, scene);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_skinned_scene_survives_its_pack_as_version_2() {
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let path =
            std::env::temp_dir().join(format!("engine_assets_skinned_{}.pack", std::process::id()));
        // Without meshes stored for streaming (version 3): the level does not use a
        // prefix of the vertices.
        let mut scene = skinned_sample();
        scene.meshes[0].lods[0].indices = vec![0, 2, 3];
        write_pack(&path, &scene, &jobs, 3).unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            2
        );
        let (loaded, _) = load_pack(&path, &jobs, 4).unwrap();
        assert_eq!(loaded, scene);
        // Without skins and meshes stored for streaming a pack stays version 1 (engines
        // that only read 1 can open it).
        let mut plain = sample();
        plain.meshes[0].lods[0].indices = vec![0, 2, 3];
        write_pack(&path, &plain, &jobs, 3).unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            1
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn colliders_and_bodies_survive_a_pack() {
        use crate::colliders::{BakedCollider, CookedBody, CookedCollider, Motion};
        use engine_collision::{Collider, ColliderShape, Origin, Shape};
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let path = std::env::temp_dir().join(format!(
            "engine_assets_colliders_{}.pack",
            std::process::id()
        ));
        let mut scene = sample();
        let shape = |id, shape| ColliderShape {
            id,
            shape,
            surface: 3,
        };
        scene.colliders = vec![
            // Not baked: the mesh's triangles go into the pack.
            CookedCollider {
                collider: Collider {
                    shapes: vec![shape(
                        0,
                        Shape::Mesh {
                            vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Y],
                            triangles: vec![[0, 1, 2]],
                            surfaces: vec![7],
                        },
                    )],
                    origin: Origin::Generated,
                },
                baked: None,
            },
            // Baked: a prop's parts, and the backend's bytes.
            CookedCollider {
                collider: Collider {
                    shapes: vec![
                        shape(
                            4,
                            Shape::Box {
                                center: Vec3::new(1.0, 2.0, 3.0),
                                rotation: Quat::from_rotation_y(0.5),
                                half_extents: Vec3::new(0.1, 0.2, 0.3),
                            },
                        ),
                        shape(
                            9,
                            Shape::Capsule {
                                center: Vec3::ZERO,
                                rotation: Quat::IDENTITY,
                                half_height: 0.5,
                                radius: 0.25,
                            },
                        ),
                        shape(
                            11,
                            Shape::Hull {
                                points: vec![Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z],
                            },
                        ),
                    ],
                    origin: Origin::Authored,
                },
                baked: Some(BakedCollider {
                    format: 0x0005_0006_0000_0001,
                    bytes: (0..=255).collect(),
                    castable: true,
                }),
            },
        ];
        scene.bodies = vec![
            CookedBody {
                collider: 0,
                ..CookedBody::default()
            },
            CookedBody {
                collider: 1,
                instances: vec![1, 0],
                motion: Motion::Dynamic,
                friction: 0.25,
                mass: Some(12.0),
                linear_velocity: [1.0, 2.0, 3.0],
                gravity_factor: 0.5,
                world_from_object: Mat4::from_scale(Vec3::new(2.0, 1.0, 1.0)).to_cols_array(),
                ..CookedBody::default()
            },
            CookedBody {
                collider: 1,
                motion: Motion::Kinematic,
                sensor: true,
                ..CookedBody::default()
            },
        ];
        write_pack(&path, &scene, &jobs, 3).unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            6
        );
        let (loaded, _) = load_pack(&path, &jobs, 4).unwrap();
        // A baked collider's meshes stay in the baked form; this one has none.
        assert_eq!(loaded, scene);
        // A scene that is only drawn does not load them.
        let (_, layout, _) = open_scene(&path, &jobs, 4).unwrap();
        assert!(layout.scene.colliders.is_empty() && layout.scene.bodies.is_empty());
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
    fn meshes_stream_by_level_from_their_pack() {
        // A grid of 32 × 32 quads with three levels from the simplifier.
        let n = 33u32;
        let mut positions = Vec::new();
        for y in 0..n {
            for x in 0..n {
                let (fx, fy) = (x as f32 / 32.0, y as f32 / 32.0);
                positions.push([fx, fy, 0.1 * (fx * 7.0).sin() * (fy * 5.0).cos()]);
            }
        }
        let mut indices = Vec::new();
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let a = y * n + x;
                indices.extend_from_slice(&[a, a + 1, a + n, a + 1, a + n + 1, a + n]);
            }
        }
        let lods = generate_lods(&positions, &indices, &LodSettings::default());
        assert!(lods.len() >= 2, "{}", lods.len());
        // With meshlets on every level, made before the vertices are ordered (the cook's
        // order).
        let mut mesh = with_meshlets(CookedMesh {
            uvs: positions.iter().map(|p| [p[0], p[1]]).collect(),
            normals: vec![[0.0, 0.0, 1.0]; positions.len()],
            positions,
            indices,
            lods,
            max: [1.0, 1.0, 0.1],
            ..CookedMesh::default()
        });
        assert!(mesh.meshlets.len() > 1 && mesh.lods.iter().all(|l| !l.meshlets.is_empty()));
        // Not ordered yet: no prefixes.
        assert!(streamed_vertex_counts(&mesh).is_none());
        let before = mesh.clone();
        order_for_streaming(&mut mesh);
        let counts = streamed_vertex_counts(&mesh).unwrap();
        assert_eq!(counts[0] as usize, mesh.positions.len());
        assert!(counts.windows(2).all(|w| w[1] < w[0]), "{counts:?}");
        // The same triangles over the same vertex data.
        let corners = |m: &CookedMesh, list: &[u32]| -> Vec<[u32; 3]> {
            list.iter()
                .map(|&i| m.positions[i as usize].map(f32::to_bits))
                .collect()
        };
        assert_eq!(
            corners(&mesh, &mesh.indices),
            corners(&before, &before.indices)
        );
        for (a, b) in mesh.lods.iter().zip(&before.lods) {
            assert_eq!(corners(&mesh, &a.indices), corners(&before, &b.indices));
        }

        let mut scene = sample();
        scene.meshes = vec![mesh.clone()];
        scene.instances.truncate(1);
        scene.instances[0].mesh = 0;
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let path =
            std::env::temp_dir().join(format!("engine_assets_meshes_{}.pack", std::process::id()));
        write_pack(&path, &scene, &jobs, 3).unwrap();
        // Whole, as before.
        let (loaded, _) = load_pack(&path, &jobs, 4).unwrap();
        assert_eq!(loaded.meshes[0], mesh);
        // Streamed: the coarsest level, then each finer one from its entry.
        let (pack, layout, _) = open_scene_streamed(&path, &jobs, 4).unwrap();
        let streamed = layout.mesh_streams[0].clone().unwrap();
        let coarse = &layout.scene.meshes[0];
        let coarsest = streamed.levels.len() - 1;
        assert_eq!(coarse.positions.len() as u32, counts[coarsest]);
        assert_eq!(coarse.indices, mesh.lods[coarsest - 1].indices);
        assert_eq!(coarse.meshlets, mesh.lods[coarsest - 1].meshlets);
        let mut finer = Vec::new();
        for record in &streamed.levels[..coarsest] {
            let data = Mutex::new(Vec::new());
            pack.load(&[record.entry], &jobs, 1, |_, e, bytes: EntryData| {
                let level = read_mesh_level_entry(e.meta, &bytes)?;
                data.lock().unwrap().push(level);
                Ok(())
            })
            .unwrap();
            finer.extend(data.into_inner().unwrap());
        }
        let whole = assemble_mesh(coarse.clone(), &streamed, &finer).unwrap();
        assert_eq!(whole, mesh);
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
        // Meshlets that do not cover their level.
        let mesh = sample().meshes[1].clone();
        assert!(!mesh.meshlets.is_empty());
        let mut bad = mesh.clone();
        bad.meshlets[0].triangles += 1;
        let entry = mesh_entry(&bad, 0);
        assert!(read_mesh_entry(entry.meta_for_tests(), &entry.raw_for_tests()).is_err());
        // A mesh level entry whose meta disagrees with its meshlet table.
        let mut grid = sample().meshes[0].clone();
        grid.meshlets = build_meshlets(&grid.positions, &mut grid.indices);
        let counts = [grid.positions.len() as u32, 3];
        let entry = mesh_level_entry(&grid, &counts, 0, 0);
        let mut meta = entry.meta_for_tests();
        assert_eq!(meta[5] as usize, grid.meshlets.len());
        let level = read_mesh_level_entry(meta, &entry.raw_for_tests()).unwrap();
        assert_eq!(level.meshlets, grid.meshlets);
        meta[5] += 1;
        assert!(read_mesh_level_entry(meta, &entry.raw_for_tests()).is_err());
        // An engine from before meshlets skips their tables (bit 16 unknown to it): the
        // same mesh without them.
        let entry = mesh_entry(&mesh, 0);
        let mut meta = entry.meta_for_tests();
        assert_ne!(meta[3] & MESHLETS, 0);
        meta[3] &= !MESHLETS;
        let read = read_mesh_entry(meta, &entry.raw_for_tests()).unwrap();
        assert_eq!(
            read,
            CookedMesh {
                meshlets: Vec::new(),
                ..mesh
            }
        );
    }
}
