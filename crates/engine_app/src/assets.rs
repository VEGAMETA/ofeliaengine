//! Assets scenes refer to (ADR 0108): the engine's built-in meshes and material, material
//! files in the game's folder, and the meshes and materials of glTF files there, made once
//! on first use. An entity names them with a [`Model`]; the player keeps its `MeshRenderer`
//! in step.
//!
//! ```text
//! [entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
//! Model.mesh = asset("builtin#cube", 6f66656c6961206275696c74696e0001)
//! Model.material = asset("materials/red.material", 4f8e1d2c3b4a59687706f5e4d3c2b1a0)
//! Model.hidden = false
//!
//! [entity 0d7e21b4c95a4f3e8b6a1c2d3e4f5a60]
//! Model.mesh = asset("scenes/yard.glb#Yard 1", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
//! Model.material = asset("scenes/yard.glb#Yard 1 grid", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
//! ```
//!
//! - **A glTF's parts** are its meshes and its materials, by name or by `#index` (from 0,
//!   as the colliders' rules count). A file of one mesh is that mesh whole.
//! - **A mesh's primitives** are drawn as one, with the model's material, in the mesh's own
//!   space: the file's node transforms are not applied.
//! - **A material's textures** (base color, metallic-roughness, normal, occlusion,
//!   emissive) are decoded on first use with their levels, uncompressed (RGBA8). With a
//!   derived data cache ([`Assets::set_derived_data`], E4-8) they are then compressed in
//!   the background as the cooker compresses them (BC7, BC5 for normal maps) and swapped
//!   in once whole on the GPU; a later run loads them from the cache compressed. Shading
//!   models from extensions (unlit, clearcoat, sheen) are plain PBR here.
//! - **glTF files load in the background** (ADR 0108's E4-4, [`crate::loading`]): read,
//!   imported, their meshes put together and their textures decoded off the frame's
//!   thread. A model draws once its mesh and material are made; its material's textures
//!   fill in as they come. The frame after a scene is loaded waits for its assets, so a
//!   scene shows whole.
//! - **A material type's instance** (a typed `.material`, ADR 0120's E10-1b) draws its look
//!   at once; its type is registered with the forward renderer, which reflects its
//!   parameters' layout and compiles the entry points it is drawn with on its material
//!   compiler's thread ([`Assets::make_material_types`]). Once the layout is back the
//!   parameters are laid out and the material is of its type (its look still drawn until
//!   its pipelines exist); its texture parameters' images load in the background as glTF
//!   textures do and fill in as they come.
//! - **A file written anew** ([`Assets::changed`], E4-5) is made again the same way, its
//!   old version drawn until the new one is whole (a material: with its textures). An
//!   image makes the glTF files and material files that use it again.
//! - **The file's id** is its `.meta`'s ([`AssetMeta`]); a file without one is found by
//!   its path alone.
//! - **A model's collider** ([`Assets::collider`], ADR 0117) is what the cook makes of its
//!   mesh by its file's rules (`<file>.colliders.ron`): cooked in the background the first
//!   time it is asked for, every mesh of the file at once, on a thread of its own (convex
//!   parts can take seconds). A built-in mesh's is its own shape.
//! - **A sprite's image** (ADR 0125, `crate::Sprite`) is an image file loaded as a glTF
//!   material's textures are (in the background, compressed as its `.meta` says), with
//!   its `.meta`'s sprite settings (pixels per unit, pivot, filter), read on first use and
//!   again when the file is read again.

use crate::clip_events::{ClipPlay, ModelEvents, crossed};
use crate::loading::{Done, Job, Loader, Made};
use crate::skins::Look;
use engine_assets::colliders::{ColliderRules, Motion, cook_node_colliders};
use engine_assets::material_file::{ParamValue, TextureEncoding, TypedMaterial};
use engine_assets::{
    DerivedDataCache, EncodedTexture, ImportedScene, MaterialData, MeshData, TextureData,
    TextureImage, TextureSource, TextureUsage, TextureWrap,
};
use engine_collision::{Collider, ColliderShape, Origin, Shape};
use engine_core::math::{Affine3A, Quat, Vec2, Vec3};
use engine_ecs::{Changed, Entity, GlobalTransform, QueryState, Transform, With, Without, World};
use engine_reflect::Reflect;
use engine_render::forward::{ForwardRenderer, MaterialTypeDesc};
use engine_render::material::{
    AlphaMode, MaterialDesc, MaterialId, MaterialParams, MaterialTable, MaterialTypeId,
    PARAMS_BYTES, TextureRef,
};
use engine_render::mesh::{GeometryPool, MeshId, OwnedMeshData};
use engine_render::skinning::{SkinId, SkinnedId, SkinnedMeshData, Skinning};
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_rhi::{Allocator, Bindless, Device, SamplerKind, Wrap};
use engine_runtime::{
    AnimationEvent, AnimationEvents, Animator, Derived, JointAttachment, MAX_GRAPH_PARAMS,
    MeshRenderer, RootMotion, SkinnedMesh,
};
use engine_scene::{AssetId, AssetMeta, AssetPaths, AssetRef, ClipImport, SpriteFilter};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A mesh drawn with a material, both by asset (ADR 0108): what scenes and the editor hold.
/// The player gives the entity a `MeshRenderer` with the mesh and material made from the
/// assets, and keeps it in step: an asset it cannot find leaves the entity undrawn and is
/// reported once in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub struct Model {
    /// The mesh: a built-in one ([`builtin`]) or a glTF file's.
    pub mesh: AssetRef,
    /// The material: the built-in one, a material file ([`MaterialFile`]) or a glTF
    /// file's.
    pub material: AssetRef,
    /// Not drawn; the entity keeps its other components.
    pub hidden: bool,
}
engine_ecs::reflect_component!(Model);

impl Model {
    /// A mesh with a material.
    pub const fn new(mesh: AssetRef, material: AssetRef) -> Self {
        Self {
            mesh,
            material,
            hidden: false,
        }
    }
}

/// Marks the entities whose `MeshRenderer` the player made from their [`Model`], so it
/// goes when the model does.
#[derive(Debug, Clone, Copy, Default)]
struct FromModel;
engine_ecs::component!(FromModel);

/// The materials of a model's mesh's primitives after the first, in the mesh's order (ADR
/// 0115; team decision, 2026-10-22: material slots on the model, one entity per mesh). The
/// first is [`Model::material`], and a primitive past the list draws with it too. Without
/// slots, a mesh's primitives draw as one with the model's material.
///
/// ```text
/// [entity 0d7e21b4c95a4f3e8b6a1c2d3e4f5a60]
/// Model.mesh = asset("props/lamp.glb#Lamp", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
/// Model.material = asset("props/lamp.glb#Metal", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
/// MaterialSlots = [asset("props/lamp.glb#Glass", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Reflect)]
pub struct MaterialSlots(pub Vec<AssetRef>);
engine_ecs::reflect_component!(MaterialSlots);

/// On a model drawn a mesh per primitive ([`MaterialSlots`]): its further primitives are
/// drawn by derived children.
#[derive(Debug, Clone, Copy, Default)]
struct Split;
engine_ecs::component!(Split);

/// A derived child drawing primitive `.0` of its parent's mesh (ADR 0115).
#[derive(Debug, Clone, Copy, Default)]
struct ModelPart(u32);
engine_ecs::component!(ModelPart);

/// The engine's built-in assets: one library of parts, written as `builtin#<part>`.
pub mod builtin {
    use engine_scene::{AssetId, AssetRef};

    /// The library's id.
    pub const LIBRARY: AssetId = AssetId::from_u128(0x6f66_656c_6961_2062_7569_6c74_696e_0001);
    /// The library's path in scene files.
    pub const PATH: &str = "builtin";
    /// A cube of 1 × 1 × 1 centred on the origin.
    pub const CUBE: AssetRef = AssetRef::part(LIBRARY, "cube");
    /// A sphere of diameter 1 centred on the origin.
    pub const SPHERE: AssetRef = AssetRef::part(LIBRARY, "sphere");
    /// A plane of 1 × 1 facing +Y.
    pub const PLANE: AssetRef = AssetRef::part(LIBRARY, "plane");
    /// A cylinder of diameter 1 and height 1 along Y.
    pub const CYLINDER: AssetRef = AssetRef::part(LIBRARY, "cylinder");
    /// A light grey, half-rough material ([`super::MaterialFile::default`]).
    pub const MATERIAL: AssetRef = AssetRef::part(LIBRARY, "default");

    /// The built-in parts by name.
    pub const PARTS: [(&str, AssetRef); 5] = [
        ("cube", CUBE),
        ("sphere", SPHERE),
        ("plane", PLANE),
        ("cylinder", CYLINDER),
        ("default", MATERIAL),
    ];
}

/// A material file (`.material`, ADR 0108, ADR 0120): the engine's standard material, or a
/// material type's instance (drawn with its look until E10-1b draws its type).
pub use engine_assets::material_file::{Alpha, MaterialFile};

/// The renderer's description of a material file: the standard material's values (a typed
/// one's look).
pub(crate) fn material_desc(file: &MaterialFile) -> MaterialDesc {
    MaterialDesc {
        base_color: file.base_color,
        metallic: file.metallic,
        roughness: file.roughness,
        emissive: file.emissive,
        alpha: match file.alpha {
            Alpha::Opaque => AlphaMode::Opaque,
            Alpha::Mask => AlphaMode::Mask {
                cutoff: file.alpha_cutoff,
            },
            Alpha::Blend => AlphaMode::Blend,
        },
        double_sided: file.double_sided,
        ..MaterialDesc::default()
    }
}

/// The built-in meshes' data, by part.
fn builtin_mesh(asset: AssetRef) -> Option<OwnedMeshData> {
    use engine_render::mesh;
    Some(match asset {
        builtin::CUBE => mesh::cube(),
        builtin::SPHERE => mesh::sphere(32, 16),
        builtin::PLANE => mesh::plane(),
        builtin::CYLINDER => mesh::cylinder(32),
        _ => return None,
    })
}

/// A written path without its part: the part starts at the first `#` of the file's name
/// (as [`AssetRef::from_path`] reads it).
fn file_part(path: &str) -> &str {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    match path[name_start..].find('#') {
        Some(hash) => &path[..name_start + hash],
        None => path,
    }
}

/// `full` as a path under `root` with `/` between its parts, `..` resolved (none outside
/// `root`).
fn project_path(root: &Path, full: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for part in full.strip_prefix(root).ok()?.components() {
        match part {
            std::path::Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// The material pack of a built game at `root` (ADR 0126's G2), if it has one; one that
/// cannot be read is said and left (its types compile, where Slang is at hand).
fn read_material_pack(root: &Path) -> Option<engine_assets::material_pack::MaterialPack> {
    let path = root.join(engine_assets::material_pack::MATERIAL_PACK);
    engine_assets::material_pack::MaterialPack::load(&path).unwrap_or_else(|e| {
        log::error!(target: "engine_app::assets", "{e}");
        None
    })
}

/// Whether a written path names a glTF file (`.glb`, `.gltf`).
fn is_gltf(path: &str) -> bool {
    Path::new(file_part(path))
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("glb") || e.eq_ignore_ascii_case("gltf"))
}

/// The path `asset`'s file is known by, without its part: the reference's own, or that of
/// another part of the same file.
fn file_path(paths: &AssetPaths, asset: AssetRef) -> Option<&str> {
    paths
        .get(asset)
        .filter(|p| !p.is_empty())
        .or_else(|| {
            paths
                .iter()
                .find(|(a, p)| a.file == asset.file && !p.is_empty())
                .map(|(_, p)| p)
        })
        .map(file_part)
}

/// The index of part `part` among things named `names`: by name, else by `#index`.
fn find_part<'a>(part: u64, names: impl Iterator<Item = &'a str>) -> Option<usize> {
    let mut by_index = None;
    let mut label = String::new();
    for (i, name) in names.enumerate() {
        if !name.is_empty() && AssetRef::part_hash(name) == part {
            return Some(i);
        }
        if by_index.is_none() {
            label.clear();
            label.push('#');
            label.push_str(&i.to_string());
            if AssetRef::part_hash(&label) == part {
                by_index = Some(i);
            }
        }
    }
    by_index
}

/// A glTF file read for its parts: what the importer made of it, the folder its external
/// files are in, and its import settings (its `.meta`'s, E4-9).
pub(crate) struct GltfFile {
    pub(crate) scene: ImportedScene,
    pub(crate) dir: PathBuf,
    pub(crate) meta: AssetMeta,
    /// The file itself: its colliders' rules are beside it.
    pub(crate) path: PathBuf,
}

/// glTF file `id` at `path` under `root`, its id checked against its `.meta` when it has
/// one, and its settings read from it.
pub(crate) fn open_gltf(root: &Path, path: &str, id: AssetId) -> Result<GltfFile, String> {
    let full = root.join(path);
    let meta_path = AssetMeta::path(&full);
    let meta = match std::fs::read_to_string(&meta_path) {
        Ok(text) => {
            let (meta, report) = AssetMeta::parse(&text).map_err(|e| format!("{path}.meta:{e}"))?;
            if let Some(meta_id) = report.id
                && meta_id != id
            {
                return Err(format!(
                    "{path} is the asset {meta_id} (its .meta), not {id}"
                ));
            }
            for (line, warning) in &report.warnings {
                log::warn!(target: "engine_app::assets", "{path}.meta:{line}: {warning}");
            }
            meta
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AssetMeta::default(),
        Err(e) => return Err(format!("{}: {e}", meta_path.display())),
    };
    let bytes = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
    let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
    let scene =
        engine_assets::import_gltf(&bytes, Some(&dir)).map_err(|e| format!("{path}: {e}"))?;
    Ok(GltfFile {
        scene,
        dir,
        meta,
        path: full,
    })
}

/// Which of a glTF's meshes part `part` is.
fn mesh_part(scene: &ImportedScene, part: u64) -> Result<usize, String> {
    if part == 0 {
        return match scene.meshes.len() {
            1 => Ok(0),
            0 => Err(String::from("the file has no meshes")),
            n => Err(format!(
                "the file has {n} meshes: name one (`file.glb#Name`, or `#0` by index)"
            )),
        };
    }
    find_part(part, scene.meshes.iter().map(|m| m.name.as_str()))
        .ok_or_else(|| String::from("the file has no mesh of that name or index"))
}

/// Which of a glTF's materials part `part` is.
fn material_part(scene: &ImportedScene, part: u64) -> Result<usize, String> {
    if part == 0 {
        return Err(String::from(
            "a glTF's materials are its parts: name one (`file.glb#Name`, or `#0` by index)",
        ));
    }
    find_part(part, scene.materials.iter().map(|m| m.name.as_str()))
        .ok_or_else(|| String::from("the file has no material of that name or index"))
}

/// A glTF mesh as one mesh: its primitives one after another, its positions multiplied by
/// `scale` (the file's import setting). Texture coordinates and tangents are kept when any
/// primitive has them (zero coordinates and +X tangents for those without).
pub(crate) fn gltf_mesh(mesh: &MeshData, scale: f32) -> OwnedMeshData {
    gltf_primitives(&mesh.primitives, scale)
}

/// [`gltf_mesh`] of some of a mesh's primitives (one alone: a part of a model with
/// material slots, ADR 0115).
pub(crate) fn gltf_primitives(
    primitives: &[engine_assets::Primitive],
    scale: f32,
) -> OwnedMeshData {
    let mut data = OwnedMeshData::default();
    let uvs = primitives.iter().any(|p| !p.uvs.is_empty());
    let tangents = primitives.iter().any(|p| !p.tangents.is_empty());
    for p in primitives {
        let (first, n) = (data.positions.len() as u32, p.positions.len());
        if scale == 1.0 {
            data.positions.extend_from_slice(&p.positions);
        } else {
            data.positions
                .extend(p.positions.iter().map(|v| v.map(|c| c * scale)));
        }
        data.normals.extend_from_slice(&p.normals);
        if uvs {
            if p.uvs.len() == n {
                data.uvs.extend_from_slice(&p.uvs);
            } else {
                data.uvs.resize(data.uvs.len() + n, [0.0; 2]);
            }
        }
        if tangents {
            if p.tangents.len() == n {
                data.tangents.extend_from_slice(&p.tangents);
            } else {
                data.tangents
                    .resize(data.tangents.len() + n, [1.0, 0.0, 0.0, 1.0]);
            }
        }
        data.indices.extend(p.indices.iter().map(|&i| first + i));
    }
    data
}

/// A texture of a glTF file as a material's slot uses it: the file, the texture, and its
/// use, which decides how its levels are filtered (a mask's cutoff in bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TextureKey {
    pub(crate) file: AssetId,
    pub(crate) texture: usize,
    usage: u8,
    cutoff: u32,
}

impl TextureKey {
    fn new(file: AssetId, texture: usize, usage: TextureUsage) -> Self {
        let (usage, cutoff) = match usage {
            TextureUsage::Color => (0, 0),
            TextureUsage::ColorMasked { cutoff } => (1, cutoff.to_bits()),
            TextureUsage::Data => (2, 0),
            TextureUsage::Normal => (3, 0),
        };
        Self {
            file,
            texture,
            usage,
            cutoff,
        }
    }

    pub(crate) fn usage(self) -> TextureUsage {
        match self.usage {
            0 => TextureUsage::Color,
            1 => TextureUsage::ColorMasked {
                cutoff: f32::from_bits(self.cutoff),
            },
            2 => TextureUsage::Data,
            _ => TextureUsage::Normal,
        }
    }
}

/// A glTF material's texture slots: base color, metallic-roughness, normal, occlusion,
/// emissive.
type Slots = [Option<(TextureKey, [Wrap; 2])>; 5];

/// A glTF material's description without its textures, and its textures by slot.
fn gltf_material(
    file: AssetId,
    m: &MaterialData,
    textures: &[TextureData],
) -> (MaterialDesc, Slots) {
    let alpha = match m.alpha {
        engine_assets::AlphaMode::Opaque => AlphaMode::Opaque,
        engine_assets::AlphaMode::Mask(bits) => AlphaMode::Mask {
            cutoff: f32::from_bits(bits),
        },
        engine_assets::AlphaMode::Blend => AlphaMode::Blend,
    };
    let color = match alpha {
        AlphaMode::Mask { cutoff } => TextureUsage::ColorMasked { cutoff },
        _ => TextureUsage::Color,
    };
    let wrap = |w: TextureWrap| match w {
        TextureWrap::Repeat => Wrap::Repeat,
        TextureWrap::MirroredRepeat => Wrap::MirroredRepeat,
        TextureWrap::ClampToEdge => Wrap::ClampToEdge,
    };
    let slot = |texture: Option<usize>, usage| {
        let texture = texture?;
        let data = textures.get(texture)?;
        Some((TextureKey::new(file, texture, usage), data.wrap.map(wrap)))
    };
    let desc = MaterialDesc {
        base_color: m.base_color,
        metallic: m.metallic,
        roughness: m.roughness,
        normal_scale: m.normal_scale,
        occlusion_strength: m.occlusion_strength,
        emissive: m.emissive,
        alpha,
        double_sided: m.double_sided,
        ..MaterialDesc::default()
    };
    let slots = [
        slot(m.base_color_texture, color),
        slot(m.metallic_roughness_texture, TextureUsage::Data),
        slot(m.normal_texture, TextureUsage::Normal),
        slot(m.occlusion_texture, TextureUsage::Data),
        slot(m.emissive_texture, TextureUsage::Color),
    ];
    (desc, slots)
}

/// A model's collider (ADR 0117): its mesh's shapes as the cook makes them by its file's
/// rules, and how a body of it moves.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCollider {
    /// The shapes, in the mesh's space (the file's import scale applied).
    pub collider: Collider,
    /// What moves its body.
    pub motion: Motion,
    /// Reports overlaps but does not collide.
    pub sensor: bool,
    /// Friction.
    pub friction: f32,
    /// Restitution.
    pub restitution: f32,
    /// Mass in kg; none: from the shapes' volume.
    pub mass: Option<f32>,
    /// How much gravity acts on it.
    pub gravity_factor: f32,
    /// Its collision layer by name; none: by its motion.
    pub layer: Option<String>,
}

impl ModelCollider {
    /// A static collider of `shapes`, with the cook's defaults.
    fn fixed(shapes: Vec<Shape>) -> Self {
        Self {
            collider: Collider {
                shapes: shapes
                    .into_iter()
                    .enumerate()
                    .map(|(id, shape)| ColliderShape {
                        id: id as u32,
                        shape,
                        surface: 0,
                    })
                    .collect(),
                origin: Origin::Generated,
            },
            motion: Motion::Static,
            sensor: false,
            friction: 0.5,
            restitution: 0.0,
            mass: None,
            gravity_factor: 1.0,
            layer: None,
        }
    }
}

/// A mesh asset's collider as [`Assets::collider`] has it.
#[derive(Debug, Clone, PartialEq)]
pub enum ColliderState {
    /// Made.
    Made(Arc<ModelCollider>),
    /// Being made: asked again later, it may be.
    Coming,
    /// None: the mesh collides with nothing (its rules, a skinned mesh, a failure said in
    /// the log, an asset that is not a mesh).
    None,
}

/// A glTF file's colliders as the loader cooked them: each mesh's (by index), and the
/// meshes' names its parts are found by.
pub(crate) struct CookedColliders {
    names: Vec<String>,
    meshes: Vec<Option<Arc<ModelCollider>>>,
}

/// The colliders of every mesh of `file` (ADR 0117): the cook's, by the rules beside it,
/// each mesh's from the first node that draws it, in the mesh's space; convex parts from
/// `cache` when it has them.
pub(crate) fn cook_model_colliders(
    file: &GltfFile,
    cache: Option<&DerivedDataCache>,
) -> Result<CookedColliders, String> {
    let rules = ColliderRules::for_scene(&file.path).map_err(|e| e.to_string())?;
    let scene = &file.scene;
    // No render instances: a model moves itself.
    let cooked = cook_node_colliders(scene, &[], &rules, cache, None);
    let scale = file.meta.model.applied_scale();
    let mut meshes: Vec<Option<Arc<ModelCollider>>> = vec![None; scene.meshes.len()];
    for (body, made_for) in cooked.bodies.iter().zip(&cooked.nodes) {
        let Some(mesh) = scene
            .nodes
            .get(made_for.node as usize)
            .and_then(|node| node.mesh)
        else {
            continue;
        };
        // In the node's object space: the mesh's.
        let (Some(slot), Some(cooked)) = (
            meshes.get_mut(mesh),
            cooked.colliders.get(made_for.collider as usize),
        ) else {
            continue;
        };
        if slot.is_some() {
            continue;
        }
        let collider = if scale == 1.0 {
            cooked.collider.clone()
        } else {
            cooked
                .collider
                .transformed(Affine3A::from_scale(Vec3::splat(scale)), 1e-3 * scale)
        };
        *slot = Some(Arc::new(ModelCollider {
            collider,
            motion: body.motion,
            sensor: body.sensor,
            friction: body.friction,
            restitution: body.restitution,
            mass: body.mass,
            gravity_factor: body.gravity_factor,
            layer: body.layer.clone(),
        }));
    }
    Ok(CookedColliders {
        names: scene.meshes.iter().map(|m| m.name.clone()).collect(),
        meshes,
    })
}

/// A built-in mesh's collider: its own shape, static.
fn builtin_collider(asset: AssetRef) -> Option<ModelCollider> {
    let shape = match asset {
        builtin::CUBE => Shape::Box {
            center: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_extents: Vec3::splat(0.5),
        },
        builtin::SPHERE => Shape::Sphere {
            center: Vec3::ZERO,
            radius: 0.5,
        },
        builtin::CYLINDER => Shape::Cylinder {
            center: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_height: 0.5,
            radius: 0.5,
        },
        // Its two triangles, facing up.
        builtin::PLANE => Shape::Mesh {
            vertices: vec![
                Vec3::new(-0.5, 0.0, -0.5),
                Vec3::new(-0.5, 0.0, 0.5),
                Vec3::new(0.5, 0.0, 0.5),
                Vec3::new(0.5, 0.0, -0.5),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            surfaces: Vec::new(),
        },
        _ => return None,
    };
    Some(ModelCollider::fixed(vec![shape]))
}

/// Which of meshes `names` part `part` is: the only one for none, else by name or index.
fn mesh_index(names: &[String], part: u64) -> Option<usize> {
    if part == 0 {
        return (names.len() == 1).then_some(0);
    }
    find_part(part, names.iter().map(String::as_str))
}

/// The longest a frame waits for its assets ([`Assets::sync`]'s `wait`).
const WAIT_AT_MOST: std::time::Duration = std::time::Duration::from_secs(10);

/// A glTF file as loading has it.
enum GltfState {
    /// Being read and imported.
    Reading,
    /// Read: parts are made from it while any is wanted, and it is let go a frame after
    /// its last use (`used`, a frame number).
    Read {
        file: Arc<GltfFile>,
        used: u64,
    },
    Failed(String),
}

/// A glTF material made without its textures, given them once they are made.
struct PendingMaterial {
    material: MaterialId,
    desc: MaterialDesc,
    slots: Slots,
}

/// A typed material file's material (ADR 0120's E10-1b): drawn with its look until its
/// type's parameters are laid out ([`Assets::make_material_types`]).
struct TypedState {
    asset: AssetRef,
    material: MaterialId,
    look: MaterialDesc,
    file: TypedMaterial,
    /// Its images: their paths as the parameters name them, and their textures' keys.
    images: Vec<(PathBuf, TextureKey)>,
    /// Its type, once registered, and the revision of the layout its parameters were laid
    /// out with.
    ty: Option<MaterialTypeId>,
    revision: Option<u32>,
    /// The parameters laid out (`None`: the type failed, the look draws), their texture
    /// fields (offset and key), and how many of those were made or failed when the
    /// parameters were last given.
    bytes: Option<Vec<u8>>,
    slots: Vec<(u32, TextureKey)>,
    given: usize,
    /// Drawn as its look while the editor compares what its type costs (E10-4f).
    as_look: bool,
}

impl TypedState {
    /// What it draws now is what it will draw: laid out and given every image (made or
    /// failed), or its type failed.
    fn settled(&self) -> bool {
        self.revision.is_some() && (self.bytes.is_none() || self.given == self.slots.len())
    }
}

/// A texture compressed in the background (E4-8), swapped in for the uncompressed one
/// once it is whole on the GPU.
#[derive(Clone, Copy)]
struct Swap {
    key: TextureKey,
    old: TextureId,
    new: TextureId,
}

/// An uncompressed texture's levels as the pool takes them.
fn decoded_levels(t: TextureImage) -> TextureLevels {
    TextureLevels {
        width: t.width,
        height: t.height,
        format: if t.srgb {
            TextureFormat::Rgba8Srgb
        } else {
            TextureFormat::Rgba8Unorm
        },
        levels: t.levels,
    }
}

/// A compressed texture's levels as the pool takes them.
fn encoded_levels(t: EncodedTexture) -> TextureLevels {
    use engine_assets::TextureFormat as Encoded;
    TextureLevels {
        width: t.width,
        height: t.height,
        format: match t.format {
            Encoded::Rgba8Srgb => TextureFormat::Rgba8Srgb,
            Encoded::Rgba8Unorm => TextureFormat::Rgba8Unorm,
            Encoded::Bc7Srgb => TextureFormat::Bc7Srgb,
            Encoded::Bc7Unorm => TextureFormat::Bc7Unorm,
            Encoded::Bc5Unorm => TextureFormat::Bc5Unorm,
        },
        levels: t.levels,
    }
}

type Changes = QueryState<(Entity, &'static Model), Changed<Model>>;
type Orphans = QueryState<Entity, (With<FromModel>, Without<Model>)>;
type SlotChanges = QueryState<(Entity, &'static Model), Changed<MaterialSlots>>;
type Unsplit = QueryState<(Entity, &'static Model), (With<Split>, Without<MaterialSlots>)>;
type Parts = QueryState<Entity, With<ModelPart>>;
type Sockets = QueryState<(Entity, &'static engine_ecs::Parent), With<JointAttachment>>;

/// The assets the player's scenes use, made on first use (ADR 0108): built-in meshes and
/// the built-in material, material files and glTF files' meshes and materials under the
/// game's folder, found by the paths loaded scenes give.
pub struct Assets {
    root: PathBuf,
    paths: AssetPaths,
    meshes: HashMap<AssetRef, MeshId>,
    materials: HashMap<AssetRef, MaterialId>,
    /// glTF files by id: being read, read, or failed.
    gltf: HashMap<AssetId, GltfState>,
    /// The background's loading (E4-4), and its results taken this frame.
    loader: Loader,
    done: Vec<Done>,
    /// Where textures are compressed into and loaded from compressed (E4-8), if anywhere.
    cache: Option<Arc<DerivedDataCache>>,
    /// Each file's generation, raised when it is read again ([`Self::reload`]): what was
    /// made from an older one is dropped when it comes back.
    generations: HashMap<AssetId, u32>,
    /// Meshes and textures being made in the background.
    building: HashSet<AssetRef>,
    decoding: HashSet<TextureKey>,
    /// Textures decoded or loaded compressed, made where the device is at hand
    /// ([`Self::make_textures`]).
    decoded: Vec<(TextureKey, Result<Made, String>)>,
    /// glTF materials' textures, made or (`None`) failed.
    textures: HashMap<TextureKey, Option<TextureId>>,
    /// Textures compressed in the background, to be made; then made, until they are whole
    /// on the GPU and swapped in. The textures compressed so far.
    compressed: Vec<(TextureKey, EncodedTexture)>,
    swapping: Vec<Swap>,
    compressed_textures: HashSet<TextureKey>,
    /// glTF materials waiting for their textures.
    pending_materials: Vec<PendingMaterial>,
    /// Typed material files' materials (ADR 0120's E10-1b).
    typed: Vec<TypedState>,
    /// A built game's material types compiled ahead (`materials.pack` in its folder, ADR
    /// 0126's G2): a type found there is made without Slang.
    material_pack: Option<engine_assets::material_pack::MaterialPack>,
    /// Typed material files drawn as their looks ([`Self::draw_as_fallback`]).
    as_fallback: HashSet<AssetRef>,
    /// Models waiting for their meshes and materials, given renderers once those are made.
    waiting: HashSet<Entity>,
    retry: Vec<Entity>,
    /// The frame of the last [`Self::sync`].
    frame: u64,
    /// Parts of files read again ([`Self::reload`]) that renderers let go of. The scene's
    /// instances draw them until it is next extracted (a tick), so they are freed in the
    /// first [`Self::sync`] after that: meshes and materials there, the textures of those
    /// materials in the [`Self::make_textures`] that follows.
    retired_meshes: Vec<MeshId>,
    retired_materials: Vec<MaterialId>,
    retiring_textures: Vec<TextureId>,
    /// Textures no material draws any more, freed in the next [`Self::make_textures`].
    retired_textures: Vec<TextureId>,
    /// Parts of files read again, drawn until their new versions are made (E4-5):
    /// meshes, materials, and the textures of those materials.
    stale_meshes: HashMap<AssetRef, MeshId>,
    stale_materials: HashMap<AssetRef, MaterialId>,
    stale_textures: Vec<(AssetId, TextureId)>,
    /// The models waiting are tried again in the next sync: a material's textures were
    /// all made.
    retry_waiting: bool,
    /// The images glTF files read besides themselves, by their paths under the root: the
    /// files to read again when one is written anew.
    depends: HashMap<String, Vec<AssetId>>,
    /// Why an asset could not be made, said once.
    problems: HashMap<AssetRef, String>,
    /// Material files as the editor holds them, read in place of their files
    /// ([`Self::set_material`], ADR 0120's E10-1c), with the paths their references were
    /// written with.
    held: HashMap<AssetId, (MaterialFile, AssetPaths)>,
    /// Material files' variants by base (E10-4a): those read over each, made again when the
    /// editor holds a new version of it.
    variants: HashMap<AssetId, Vec<AssetId>>,
    changes: Option<Changes>,
    orphans: Option<Orphans>,
    changed: Vec<(Entity, Model)>,
    gone: Vec<Entity>,
    /// glTF meshes made a mesh per primitive, for models with material slots (ADR 0115):
    /// made, being made, and those of files read again drawn until made again.
    split: HashMap<AssetRef, Vec<MeshId>>,
    building_split: HashSet<AssetRef>,
    stale_split: HashMap<AssetRef, Vec<MeshId>>,
    slot_changes: Option<SlotChanges>,
    unsplit: Option<Unsplit>,
    parts: Option<Parts>,
    /// The characters' sockets (ADR 0124's E12-1c): each with its character, sorted by
    /// character, kept between ticks.
    sockets: Option<Sockets>,
    socket_list: Vec<(Entity, Entity)>,
    /// Models' colliders (ADR 0117): each glTF file's once cooked (or why not), the files
    /// being cooked, the built-in meshes' as made, and how many times any was made or
    /// forgotten.
    colliders: HashMap<AssetId, Result<CookedColliders, String>>,
    cooking: HashSet<AssetId>,
    /// Of those, the files being read first: cooked once they are.
    read_to_cook: HashSet<AssetId>,
    builtin_colliders: HashMap<AssetRef, Option<Arc<ModelCollider>>>,
    collider_epoch: u64,
    /// Skinned meshes (ADR 0124's E12-1a): made, by mesh and whether a part per primitive,
    /// with their parts' skins and their rig; being made; and the skins of files read again,
    /// let go once no model draws them.
    skinned: HashMap<(AssetRef, bool), SkinnedAsset>,
    building_skins: HashSet<(AssetRef, bool)>,
    retired_skins: Vec<SkinId>,
    /// Each skinned model's instances of its skin.
    owned: HashMap<Entity, Owned>,
    posing: crate::skins::Posing,
    /// Each model file's clips' events (ADR 0124's E12-3b), read from its `.meta` once; the
    /// characters with any (not held by the editor) by entity, with their rigs and events,
    /// made again when `events_dirty` says the skinned models changed; the sounds a tick's
    /// events play.
    clip_events: HashMap<AssetId, Arc<ModelEvents>>,
    event_characters: Vec<(Entity, Arc<crate::skins::Rig>, Arc<ModelEvents>)>,
    events_dirty: bool,
    event_sounds: Vec<(Entity, Arc<str>)>,
    /// The editor's model preview's skin (ADR 0124's E12-1b).
    preview_skin: Option<PreviewSkin>,
    preview_graph: Option<PreviewGraph>,
    /// The clips made in the editor (`.anim`, ADR 0124's E12-4a) and their animators.
    fields: crate::field_clips::FieldClips,
    /// The animation graphs animators play (`.agraph`, ADR 0124's E12-5c).
    graphs: crate::graphs::Graphs,
    /// Graph parameters the editor holds for testing (ADR 0124's E12-5e): the character, the
    /// slot and the value, put in over what its animator says before each tick of its graph.
    held_params: Vec<(Entity, u16, f32)>,
    /// Sprites' images (ADR 0125), each in the slot it got on first use, and the slots by
    /// asset.
    sprite_images: Vec<SpriteImageState>,
    sprite_slots: HashMap<AssetRef, u32>,
    /// The animators of sprites (E13-2) and the tick's list of them, kept.
    sprite_animators: Option<SpriteAnimators>,
    sprite_work: Vec<(Entity, AssetRef)>,
}

type SpriteAnimators = QueryState<(Entity, &'static Animator, &'static crate::Sprite)>;

/// A sprite's image (ADR 0125): its texture's key, its `.meta`'s sprite settings, and what
/// a frame draws of it.
struct SpriteImageState {
    asset: AssetRef,
    key: TextureKey,
    settings: SpriteImage,
    /// Its frames (E13-2): its `.meta`'s slices (the one its part names, for a part), in
    /// pixels; none: the whole image, one frame.
    frames: Vec<SpriteFrame>,
    /// Its clips (E13-2): the `.meta`'s, played by animators of sprites showing the image.
    clips: Vec<SpriteClip>,
    /// No path is known for it (said once): never drawn.
    lost: bool,
    /// This frame's: its settings with its texture's bindless index and size, once made.
    shown: Option<SpriteImage>,
}

/// A sprite's image as a frame draws it (ADR 0125): its texture's bindless index, its size
/// in pixels, and its `.meta`'s pixels a metre, pivot and sampler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SpriteImage {
    pub(crate) index: u32,
    pub(crate) size: Vec2,
    pub(crate) pixels_per_unit: f32,
    pub(crate) pivot: Vec2,
    pub(crate) sampler: SamplerKind,
    /// The part of the texture drawn: min u, min v, max u, max v (the image's top at v 0);
    /// a frame's slice (E13-2), else all of it.
    pub(crate) uv: [f32; 4],
    /// The frame's nine-slice border in pixels (left, top, right, bottom; E13-2d): what a
    /// sliced sprite keeps unstretched.
    pub(crate) border: [f32; 4],
}

/// A sprite clip laid out to play (ADR 0125's E13-2): the moment each of its frames
/// starts, the frame, its length, and its events.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpriteClip {
    pub(crate) name: Box<str>,
    starts: Box<[f32]>,
    frames: Box<[u32]>,
    pub(crate) length: f32,
    pub(crate) once: bool,
    pub(crate) events: Vec<crate::clip_events::EventDef>,
}

impl SpriteClip {
    /// Clip `clip` of a `.meta`: its run of frames (ping-pong's way back too) with their
    /// times.
    pub(crate) fn new(clip: &engine_scene::SpriteClipImport) -> Self {
        let run: Vec<u32> = if clip.last >= clip.first {
            (clip.first..=clip.last).collect()
        } else {
            (clip.last..=clip.first).rev().collect()
        };
        let time_of = |k: usize| {
            let t = clip.durations.get(k).copied().unwrap_or(clip.frame_time);
            if t.is_finite() && t > 0.0 { t } else { 0.1 }
        };
        // The run's places in order: forth, and for ping-pong back without its ends.
        let mut places: Vec<usize> = (0..run.len()).collect();
        if clip.mode == engine_scene::SpriteClipMode::PingPong && run.len() > 2 {
            places.extend((1..run.len() - 1).rev());
        }
        let mut starts = Vec::with_capacity(places.len());
        let mut at = 0.0;
        for &k in &places {
            starts.push(at);
            at += time_of(k);
        }
        Self {
            name: Box::from(clip.name.as_str()),
            starts: starts.into_boxed_slice(),
            frames: places.iter().map(|&k| run[k]).collect(),
            length: at,
            once: clip.mode == engine_scene::SpriteClipMode::Once,
            events: crate::clip_events::event_defs(&clip.events),
        }
    }

    /// The frame shown `t` seconds in (the last from its end on).
    pub(crate) fn frame_at(&self, t: f32) -> u32 {
        let k = self.starts.partition_point(|&s| s <= t).max(1) - 1;
        self.frames.get(k).copied().unwrap_or(0)
    }
}

/// A frame of a sprite image (ADR 0125's E13-2): a slice's rectangle in pixels (left, top,
/// width, height) and its pivot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SpriteFrame {
    pub(crate) rect: [f32; 4],
    pub(crate) pivot: Vec2,
    /// Its nine-slice border in pixels: left, top, right, bottom.
    pub(crate) border: [f32; 4],
}

impl SpriteImage {
    /// The whole image's texture coordinates.
    pub(crate) const ALL: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

    /// This image as `frame` shows it: its size and pivot the frame's, its texture
    /// coordinates the frame's rectangle's.
    pub(crate) fn framed(self, frame: &SpriteFrame) -> Self {
        let [x, y, w, h] = frame.rect;
        let full = self.size.max(Vec2::ONE);
        Self {
            size: Vec2::new(w, h),
            pivot: frame.pivot,
            uv: [x / full.x, y / full.y, (x + w) / full.x, (y + h) / full.y],
            border: frame.border,
            ..self
        }
    }
}

impl Assets {
    /// The slot of sprite image `asset` (ADR 0125), made the first time it is asked for:
    /// its `.meta`'s sprite settings read, its texture loaded in the background. A part of
    /// an image (a slice, E13-2) draws the whole image for now.
    pub(crate) fn sprite_slot(&mut self, asset: AssetRef) -> u32 {
        if let Some(&slot) = self.sprite_slots.get(&asset) {
            return slot;
        }
        let slot = self.sprite_images.len() as u32;
        let lost = self.paths.get(AssetRef::file(asset.file)).is_none() || asset.is_none();
        if lost && !asset.is_none() {
            log::warn!(
                target: "engine_app::assets",
                "sprite image {}: no path known for it; not drawn",
                asset.file
            );
        }
        let (settings, frames, clips) = self.sprite_settings(asset);
        self.sprite_images.push(SpriteImageState {
            asset,
            key: TextureKey::new(asset.file, 0, TextureUsage::Color),
            settings,
            frames,
            clips,
            lost,
            shown: None,
        });
        self.sprite_slots.insert(asset, slot);
        self.load_sprite_image(slot as usize);
        slot
    }

    /// Image `asset`'s sprite settings from its file's `.meta` (the defaults without one),
    /// with no texture yet, and its frames: the slices, or the one slice its part names.
    fn sprite_settings(&self, asset: AssetRef) -> (SpriteImage, Vec<SpriteFrame>, Vec<SpriteClip>) {
        let meta = self
            .paths
            .get(AssetRef::file(asset.file))
            .map(|path| AssetMeta::read(&self.root.join(path)).sprite)
            .unwrap_or_default();
        let frame = |s: &engine_scene::SliceImport| SpriteFrame {
            rect: s.rect.map(|v| v as f32),
            pivot: Vec2::from_array(s.pivot),
            border: s.border.map(|v| v as f32),
        };
        let frames = if asset.part == 0 {
            meta.slices.iter().map(frame).collect()
        } else {
            match meta.slice_of_part(asset.part) {
                Some(k) => vec![frame(&meta.slices[k])],
                None => {
                    log::warn!(
                        target: "engine_app::assets",
                        "sprite image {}: no slice of that name in its .meta; drawn whole",
                        asset.file
                    );
                    Vec::new()
                }
            }
        };
        let settings = SpriteImage {
            index: u32::MAX,
            size: Vec2::ZERO,
            pixels_per_unit: meta.applied_pixels_per_unit(),
            pivot: Vec2::from_array(meta.pivot),
            sampler: match meta.filter {
                SpriteFilter::Nearest => SamplerKind::NearestClamp,
                SpriteFilter::Linear => SamplerKind::LinearClamp,
            },
            uv: SpriteImage::ALL,
            border: [0.0; 4],
        };
        // A part is one slice: no clips.
        let clips = if asset.part == 0 {
            meta.clips.iter().map(SpriteClip::new).collect()
        } else {
            Vec::new()
        };
        (settings, frames, clips)
    }

    /// One tick of the animators of sprites (ADR 0125's E13-2, after the animators moved
    /// on): an animator whose clip is one of its sprite's image's (not among its own
    /// `.anim` clips, not a graph) shows the clip's frame at its time, learns the clip's
    /// length and whether it holds, and fires the events its tick passed. Writes only what
    /// changed; allocates nothing once its vectors have grown.
    pub(crate) fn animate_sprites(&mut self, world: &mut World) {
        let query = self
            .sprite_animators
            .get_or_insert_with(|| QueryState::new(world));
        let mut work = core::mem::take(&mut self.sprite_work);
        work.clear();
        query.for_each_readonly(world, |(e, animator, sprite)| {
            if sprite.image.is_none()
                || sprite.image.part != 0
                || animator.plays_graph()
                || animator.clips.contains_key(&animator.clip)
            {
                return;
            }
            work.push((e, sprite.image));
        });
        for &(e, image) in &work {
            let slot = self.sprite_slot(image) as usize;
            let Some(animator) = world.get::<Animator>(e) else {
                continue;
            };
            let clips = &self.sprite_images[slot].clips;
            // By name; empty or a name it lacks: the first, as a model's clips go.
            let Some(clip) = clips
                .iter()
                .find(|c| *c.name == *animator.clip)
                .or_else(|| clips.first())
            else {
                continue;
            };
            let frame = clip.frame_at(animator.time);
            let (from, to) = (animator.previous, animator.time);
            let told = animator.length == clip.length && animator.hold == clip.once;
            let fired = &mut self.fields.fired;
            crossed(&clip.events, clip.length, from, to, |event| {
                fired.push((e, event.clone()));
            });
            if !told && let Some(animator) = world.get_mut::<Animator>(e) {
                animator.length = clip.length;
                animator.hold = clip.once;
            }
            if world
                .get::<crate::Sprite>(e)
                .is_some_and(|s| s.frame != frame)
                && let Some(sprite) = world.get_mut::<crate::Sprite>(e)
            {
                sprite.frame = frame;
            }
        }
        self.sprite_work = work;
    }

    /// Gives the loader sprite image `slot`'s texture, unless it is made or being made.
    fn load_sprite_image(&mut self, slot: usize) {
        let Some(state) = self.sprite_images.get(slot) else {
            return;
        };
        let key = state.key;
        if state.lost || self.textures.contains_key(&key) || !self.decoding.insert(key) {
            return;
        }
        let Some(path) = self.paths.get(AssetRef::file(key.file)) else {
            return;
        };
        self.loader.give(Job::Image {
            key,
            path: self.root.join(path),
            cache: self.cache.clone(),
            generation: self.generation(key.file),
        });
    }

    /// Each sprite image as this frame draws it (ADR 0125): its texture's bindless index
    /// and size, once its texture is made and resident. Call it after the texture pool's
    /// `begin_frame` (the indices are its).
    pub(crate) fn show_sprite_images(&mut self, pool: &TexturePool) {
        for state in &mut self.sprite_images {
            state.shown = self
                .textures
                .get(&state.key)
                .copied()
                .flatten()
                .and_then(|id| {
                    let info = pool.info(id)?;
                    Some(SpriteImage {
                        index: pool.index(id)?,
                        size: Vec2::new(info.width as f32, info.height as f32),
                        ..state.settings
                    })
                });
        }
    }

    /// Sprite image `slot`'s texture in `pool` as it is now, whatever views draw (the
    /// editor's image tab, ADR 0125's E13-2c): its bindless index and size in pixels;
    /// `None` while it is being made, or when it cannot be.
    pub(crate) fn sprite_texture(&self, slot: u32, pool: &TexturePool) -> Option<(u32, [f32; 2])> {
        let state = self.sprite_images.get(slot as usize)?;
        let id = self.textures.get(&state.key).copied().flatten()?;
        let info = pool.info(id)?;
        Some((pool.index(id)?, [info.width as f32, info.height as f32]))
    }

    /// Sprite image `slot`'s frame `frame` as this frame draws it (ADR 0125's E13-2,
    /// [`Self::show_sprite_images`]): its slice of that number (the last for a number past
    /// them), or the whole image when it has none; `None` while its texture is being made,
    /// or when it cannot be.
    pub(crate) fn sprite_frame(&self, slot: u32, frame: u32) -> Option<SpriteImage> {
        let state = self.sprite_images.get(slot as usize)?;
        let image = state.shown?;
        Some(match state.frames.len() {
            0 => image,
            n => image.framed(&state.frames[(frame as usize).min(n - 1)]),
        })
    }

    /// Sprite images known, and of those made.
    pub fn sprite_images(&self) -> (usize, usize) {
        let made = self
            .sprite_images
            .iter()
            .filter(|s| self.textures.get(&s.key).is_some_and(Option::is_some))
            .count();
        (self.sprite_images.len(), made)
    }
}

/// A skinned mesh made (ADR 0124's E12-1a): a skin per part in `Skinning`, and its rig.
struct SkinnedAsset {
    skins: Vec<SkinId>,
    rig: Arc<crate::skins::Rig>,
}

/// A skinned model's own instances of its skin (ADR 0124's E12-1a), one per part (the
/// entity's first, its parts' after), with their meshes; its rig, its pose, whether the
/// editor holds that pose (else its [`Animator`]'s, its rest pose without one), and whether
/// its palette was set for that pose.
struct Owned {
    skins: Vec<SkinId>,
    instances: Vec<SkinnedId>,
    meshes: Vec<MeshId>,
    rig: Arc<crate::skins::Rig>,
    /// Its file, and that file's clips' events (E12-3b).
    file: AssetId,
    events: Arc<ModelEvents>,
    /// What it shows: a clip's moment, the clip fading out under it (E12-2b), its root
    /// motion taken out or not (E12-2c); or its animation graph (E12-5c).
    look: Look,
    held: bool,
    posed: bool,
    /// Its play of the animation graph its animator names (E12-5c), once it is cooked for its
    /// rig.
    graph: Option<crate::graphs::GraphPlay>,
}

/// The editor's model preview's skin (ADR 0124's E12-1b): the mesh and material it shows,
/// the skins of its mesh drawn whole, its own instance of the first with that instance's
/// mesh, its rig, the pose asked for and whether the palette is set for it.
struct PreviewSkin {
    mesh: AssetRef,
    material: AssetRef,
    skins: Vec<SkinId>,
    instance: Option<(SkinnedId, MeshId)>,
    rig: Option<Arc<crate::skins::Rig>>,
    pose: Option<(usize, f32)>,
    /// Its clip's root motion taken out of the pose (a floor under it, E12-1b-3).
    in_place: bool,
    posed: bool,
    /// A second clip's moment drawn beside it (E12-1b-3b): its own instance of the skin,
    /// made the first time one is asked for and kept with the preview, its pose and
    /// whether its palette was set for it.
    beside: Option<(SkinnedId, MeshId)>,
    beside_pose: Option<(usize, f32)>,
    beside_posed: bool,
}

/// The animation graph the live preview's model plays (ADR 0124's E12-5d-2d): its file, its
/// play once cooked for the preview's rig, the parameter values the editor holds by slot,
/// the seconds not ticked yet and how far the frame is between two ticks.
struct PreviewGraph {
    file: AssetId,
    play: Option<crate::graphs::GraphPlay>,
    held: Vec<(u16, f32)>,
    /// Triggers fired, set at its next tick (E12-5d-2d-2).
    fired: Vec<u16>,
    left: f32,
    alpha: f32,
}

/// The preview's graph's tick: as the characters' (64 Hz).
const PREVIEW_TICK: f32 = 1.0 / 64.0;

/// What an animator shows a frame (ADR 0124's E12-2): its clip at its moment, the clip
/// fading out under it (at its own moment, with the first's weight), its root motion taken
/// out or not (along each clip's axes, E12-3e); and the two clips' lengths and whether each
/// plays once (E12-3e), none for a clip of the animator's own (the clips made in the
/// editor's ticks tell it theirs, E12-4a).
type Shown = (Look, [Option<(f32, bool)>; 2]);

/// How clip `k` of `rig` plays by its model's `.meta` (ADR 0124's E12-3e).
fn clip_play(rig: &crate::skins::Rig, table: &ModelEvents, k: usize) -> ClipPlay {
    let name = rig.clips().get(k).map_or("", crate::RigClip::name);
    table.play(name, k)
}

/// What an animator shows a frame `alpha` of the way from its last tick to its next
/// ([`Shown`]), its clips played as `table` (its model's `.meta`) says; None for a rig
/// without clips.
fn animator_pose(
    rig: &crate::skins::Rig,
    animator: &Animator,
    alpha: f32,
    table: &ModelEvents,
) -> Option<Shown> {
    let clips = rig.clips();
    let names = clips.iter().map(crate::RigClip::name);
    let mut look = Look {
        in_place: animator.root_motion != RootMotion::InPose,
        ..Look::default()
    };
    let mut lengths = [None; 2];
    // One of its own clips (E12-4e): the joints it keys over the rest pose, at its moment as
    // its length (told by the clips made in the editor's ticks) wraps or holds it.
    if let Some(own) = animator.clips.get(&animator.clip) {
        look.own = Some(crate::skins::OwnPose {
            file: own.file,
            time: animator.at(alpha, animator.length),
            playhead: false,
        });
    } else {
        let k = named_clip(names.clone(), &animator.clip);
        let length = clips.get(k)?.duration();
        let play = clip_play(rig, table, k);
        look.pose = Some((k, animator.at(alpha, length)));
        look.axes[0] = play.axes;
        lengths[0] = Some((length, play.once));
    }
    if animator.fade > 0.0 {
        let weight = animator.weight(alpha);
        if let Some(own) = animator.clips.get(&animator.from_clip) {
            let shown = crate::skins::OwnPose {
                file: own.file,
                time: animator.from_at(alpha, animator.from_length),
                playhead: false,
            };
            look.own_fade = Some((shown, weight));
        } else {
            let f = named_clip(names, &animator.from_clip);
            let play = clip_play(rig, table, f);
            let length = clips.get(f).map_or(0.0, crate::RigClip::duration);
            look.fade = Some((f, animator.from_at(alpha, length), weight));
            look.axes[1] = play.axes;
            lengths[1] = Some((length, play.once));
        }
    }
    Some((look, lengths))
}

/// The clips of an animator's own `look` shows (ADR 0124's E12-4e), the one fading in and
/// the one fading out, as `fields` last read or holds them.
fn own_clips<'a>(
    fields: &'a crate::field_clips::FieldClips,
    look: &Look,
) -> [Option<&'a crate::field_clips::FieldClip>; 2] {
    [look.own.map(|o| o.file), look.own_fade.map(|(o, _)| o.file)]
        .map(|file| file.and_then(|f| fields.loaded(f)).map(|c| &**c))
}

/// The root motion an [`Animator`]'s last tick played (ADR 0124's E12-2c), in its rig's
/// mesh space: how far its root crossed the ground, and its turn about the mesh's up in
/// radians. Through a fade the two clips' motions blend by the fade's weight. None for a
/// rig without clips, or while one of the animator's own clips plays alone: those move
/// nothing by root motion (E12-4e), so through a fade into or out of one the model's clip's
/// motion fades.
fn tick_motion(
    rig: &crate::skins::Rig,
    animator: &Animator,
    table: &ModelEvents,
) -> Option<(Vec3, f32)> {
    if animator.plays_own_clip() && animator.fade <= 0.0 {
        return None;
    }
    let names = rig.clips().iter().map(crate::RigClip::name);
    let motion_of = |name: &str, previous: f32, time: f32| {
        if animator.clips.contains_key(name) {
            return Some(engine_anim::RootMotion::NONE);
        }
        let k = named_clip(names.clone(), name);
        rig.clip_motion(k, previous, time, clip_play(rig, table, k).axes)
    };
    let main = motion_of(&animator.clip, animator.previous, animator.time)?;
    let from = (animator.fade > 0.0)
        .then(|| {
            motion_of(
                &animator.from_clip,
                animator.from_previous,
                animator.from_time,
            )
        })
        .flatten();
    let motion = match from {
        Some(from) => engine_anim::RootMotion::blend(from, main, animator.weight(1.0)),
        None => main,
    };
    Some(rig.mesh_motion(motion))
}

/// The root motion an animation graph's last tick played (ADR 0124's E12-5c-2), in its rig's
/// mesh space as [`tick_motion`]'s: its base layer's clips' motions over the tick's step,
/// each along its clip's own axes and by its weight at the tick's end (a clip the rig has
/// not moves nothing). None while the layer plays nothing.
fn graph_motion(
    rig: &crate::skins::Rig,
    table: &ModelEvents,
    play: &crate::graphs::GraphPlay,
) -> Option<(Vec3, f32)> {
    let (mut moved, mut turned, mut any) = (Vec3::ZERO, 0.0f32, false);
    for p in play.cooked.graph.played(&play.state, 0) {
        let k = usize::from(p.clip);
        let Some(motion) = rig.clip_motion(k, p.from, p.to, clip_play(rig, table, k).axes) else {
            continue;
        };
        let (m, t) = rig.mesh_motion(motion);
        moved += m * p.weight;
        turned += t * p.weight;
        any = true;
    }
    any.then_some((moved, turned))
}

/// The clip whose events an [`Animator`]'s last tick fires (ADR 0124's E12-3b), and the span
/// of it the tick played: its clip, but through a cross-fade the clip fading out while that
/// one still weighs more, so a fade never doubles a footstep. None for a rig without clips.
fn played_span(rig: &crate::skins::Rig, animator: &Animator) -> Option<(usize, f32, f32)> {
    let names = rig.clips().iter().map(crate::RigClip::name);
    let (k, from, to) = if animator.fade > 0.0 && animator.weight(1.0) < 0.5 {
        if animator.clips.contains_key(&animator.from_clip) {
            return None;
        }
        let k = named_clip(names, &animator.from_clip);
        (k, animator.from_previous, animator.from_time)
    } else {
        if animator.plays_own_clip() {
            return None;
        }
        let k = named_clip(names, &animator.clip);
        (k, animator.previous, animator.time)
    };
    (k < rig.clips().len()).then_some((k, from, to))
}

/// The clip an animator names among a rig's clip names (ADR 0124's E12-2): by its name in
/// the file, an unnamed one by the name the editor shows it by (`Clip 2` for the second);
/// the first for a name none has.
fn named_clip<'a>(names: impl Iterator<Item = &'a str> + Clone, name: &str) -> usize {
    find_clip(names, name).unwrap_or(0)
}

/// The clip `name` names among a rig's clip names, as [`named_clip`] finds it; none for a
/// name none has (an animation graph's clip the model lacks, E12-5c).
fn find_clip<'a>(names: impl Iterator<Item = &'a str> + Clone, name: &str) -> Option<usize> {
    names.clone().position(|n| n == name).or_else(|| {
        let k = name
            .strip_prefix("Clip ")?
            .parse::<usize>()
            .ok()?
            .checked_sub(1)?;
        names.clone().nth(k).filter(|n| n.is_empty()).map(|_| k)
    })
}

/// What a character's animation graph does (ADR 0124's E12-5c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphSync {
    /// Its animator names none: it plays its clip.
    None,
    /// It plays its graph.
    Plays,
    /// Its graph cannot be played (its file could not be read): it shows its rest pose.
    Broken,
}

/// Character `owned`'s play of the animation graph `animator` names (ADR 0124's E12-5c)
/// made, or checked when the graphs changed since: cooked for its rig (`path_of` a graph
/// file's path), its state at its entry with the parameters its animator set, weighed once
/// so a still world shows its first pose. Let go when the animator names none.
fn sync_graph(
    graphs: &mut crate::graphs::Graphs,
    owned: &mut Owned,
    animator: &Animator,
    path_of: impl FnOnce(AssetId) -> Option<PathBuf>,
) -> GraphSync {
    let graph = animator.graph;
    if graph == AssetRef::NONE {
        if owned.graph.take().is_some() {
            owned.posed = false;
        }
        return GraphSync::None;
    }
    let generation = graphs.generation();
    if owned
        .graph
        .as_ref()
        .is_some_and(|p| p.file == graph.file && p.generation == generation)
    {
        return GraphSync::Plays;
    }
    graphs.read(graph.file, || path_of(graph.file));
    let (rig, table) = (&owned.rig, &owned.events);
    let clip = |name: &str| {
        let clips = rig.clips();
        let k = find_clip(clips.iter().map(crate::RigClip::name), name)?;
        Some(engine_anim::ClipInfo {
            index: u16::try_from(k).ok()?,
            length: clips[k].duration(),
            once: clip_play(rig, table, k).once,
        })
    };
    let motion = |k: u16| rig.motion_mode_of(clip_play(rig, table, usize::from(k)).axes);
    let Some(cooked) = graphs.cook(graph.file, rig, owned.file, clip, motion) else {
        if owned.graph.take().is_some() {
            owned.posed = false;
        }
        return GraphSync::Broken;
    };
    match &mut owned.graph {
        Some(play) if play.file == graph.file && Arc::ptr_eq(&play.cooked, &cooked) => {
            play.generation = generation;
        }
        _ => {
            let mut state = engine_anim::GraphState::new(&cooked.graph);
            set_params(&mut state, animator, &cooked.graph);
            state.set_in_place(animator.root_motion != RootMotion::InPose);
            // In the state a loaded save names, or the one it was in when its graph is
            // played anew (E12-5c-2); else at the entry.
            if !animator.state.is_empty() {
                cooked.graph.resume_in(&mut state, 0, &animator.state);
            }
            cooked.graph.tick(&mut state, 0.0);
            owned.graph = Some(crate::graphs::GraphPlay {
                file: graph.file,
                cooked,
                state,
                generation,
            });
            owned.posed = false;
        }
    }
    GraphSync::Plays
}

/// The parameters `animator` set (ADR 0124's E12-5c, the slots its `params_set` marks) into
/// `graph`'s state `state`; the others keep the graph's values.
fn set_params(
    state: &mut engine_anim::GraphState,
    animator: &Animator,
    graph: &engine_anim::AnimGraph,
) {
    let count = graph.params().count().min(MAX_GRAPH_PARAMS);
    for (k, &value) in animator.params.iter().enumerate().take(count) {
        if animator.params_set & (1 << k) != 0 {
            state.set(k as u16, value);
        }
    }
}

/// Character `owned`'s joints as `look` shows them ([`crate::skins::Posing::joints`]), its
/// animation graph's where `look` says so (E12-5c).
fn joints_of<'p>(
    posing: &'p mut crate::skins::Posing,
    fields: &crate::field_clips::FieldClips,
    owned: &Owned,
    look: Look,
) -> &'p [Affine3A] {
    match (look.graph, &owned.graph) {
        (Some(alpha), Some(play)) => {
            posing.joints_graph(&owned.rig, &play.cooked.graph, &play.state, alpha)
        }
        _ => posing.joints(&owned.rig, look, own_clips(fields, &look)),
    }
}

/// What a model's mesh is to skinning: drawn skinned (the file and the mesh's index), or
/// plain, or not known while its file is read.
enum SkinCheck {
    Skinned(Arc<GltfFile>, usize),
    Plain,
    Reading,
}

impl Assets {
    /// Assets under `root`, the folder relative asset paths start from.
    pub fn new(root: PathBuf) -> Self {
        let mut paths = AssetPaths::new();
        for (name, asset) in builtin::PARTS {
            paths.insert(asset, format!("{}#{name}", builtin::PATH));
        }
        Self {
            root: root.clone(),
            paths,
            meshes: HashMap::new(),
            materials: HashMap::new(),
            gltf: HashMap::new(),
            loader: Loader::new(root.clone()),
            done: Vec::new(),
            cache: None,
            generations: HashMap::new(),
            building: HashSet::new(),
            decoding: HashSet::new(),
            decoded: Vec::new(),
            textures: HashMap::new(),
            compressed: Vec::new(),
            swapping: Vec::new(),
            compressed_textures: HashSet::new(),
            pending_materials: Vec::new(),
            typed: Vec::new(),
            material_pack: read_material_pack(&root),
            as_fallback: HashSet::new(),
            waiting: HashSet::new(),
            retry: Vec::new(),
            frame: 0,
            retired_meshes: Vec::new(),
            retired_materials: Vec::new(),
            retiring_textures: Vec::new(),
            retired_textures: Vec::new(),
            stale_meshes: HashMap::new(),
            stale_materials: HashMap::new(),
            stale_textures: Vec::new(),
            retry_waiting: false,
            depends: HashMap::new(),
            problems: HashMap::new(),
            held: HashMap::new(),
            variants: HashMap::new(),
            changes: None,
            orphans: None,
            changed: Vec::new(),
            gone: Vec::new(),
            split: HashMap::new(),
            building_split: HashSet::new(),
            stale_split: HashMap::new(),
            slot_changes: None,
            unsplit: None,
            parts: None,
            sockets: None,
            socket_list: Vec::new(),
            colliders: HashMap::new(),
            cooking: HashSet::new(),
            read_to_cook: HashSet::new(),
            builtin_colliders: HashMap::new(),
            collider_epoch: 0,
            skinned: HashMap::new(),
            building_skins: HashSet::new(),
            retired_skins: Vec::new(),
            owned: HashMap::new(),
            posing: crate::skins::Posing::default(),
            clip_events: HashMap::new(),
            event_characters: Vec::new(),
            events_dirty: false,
            event_sounds: Vec::new(),
            preview_skin: None,
            preview_graph: None,
            fields: crate::field_clips::FieldClips::default(),
            graphs: crate::graphs::Graphs::default(),
            held_params: Vec::new(),
            sprite_images: Vec::new(),
            sprite_slots: HashMap::new(),
            sprite_animators: None,
            sprite_work: Vec::new(),
        }
    }

    /// The folder relative asset paths start from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The paths assets are found and written at: the built-in ones and those of the
    /// scenes loaded (give them to `engine_scene::save_with` to write references the same
    /// way).
    pub fn paths(&self) -> &AssetPaths {
        &self.paths
    }

    /// Learns where assets are (a loaded scene's `LoadReport::assets`).
    pub fn add_paths(&mut self, paths: &AssetPaths) {
        self.paths.extend(paths);
    }

    /// Material file `file` as the editor holds it (ADR 0120's E10-1c), `written` the paths
    /// its references were written with: read from now on in place of the file on disk
    /// (`None`: the file again). Its material, if made, is made again in its place, by the
    /// next frame: a standard file's values at once; a typed file's parameters laid out
    /// again ([`Self::make_material_types`]), its previous ones drawn until then, so a
    /// value dragged in the inspector draws without a frame of its look.
    pub fn set_material(
        &mut self,
        file: AssetId,
        held: Option<(MaterialFile, AssetPaths)>,
        table: &mut MaterialTable,
    ) {
        match held {
            Some(held) => self.held.insert(file, held),
            None => self.held.remove(&file),
        };
        self.remake_material(AssetRef::file(file), table);
        // Its variants, read over it again (E10-4a).
        let variants = self.variants.get(&file).cloned().unwrap_or_default();
        for variant in variants {
            if variant != file {
                self.remake_material(AssetRef::file(variant), table);
            }
        }
    }

    /// Material file `asset`'s material, if made, made again in its place from the file as
    /// it is read now ([`Self::set_material`]).
    fn remake_material(&mut self, asset: AssetRef, table: &mut MaterialTable) {
        let Some(&material) = self.materials.get(&asset) else {
            return;
        };
        let (data, written) = match self.read_material(asset) {
            Ok((data, written, bases)) => {
                self.note_bases(asset.file, &bases);
                (data, written)
            }
            Err(why) => {
                self.fail(asset, why);
                return;
            }
        };
        self.problems.remove(&asset);
        let look = material_desc(&data);
        // Its type kept when the file still names it: laid out again with the layout the
        // forward renderer has, in this frame's `make_material_types`.
        let kept = self
            .typed
            .iter()
            .position(|t| t.material == material)
            .map(|k| self.typed.swap_remove(k));
        if !data.is_typed() {
            table.set(material, look);
            return;
        }
        match self.typed_state(asset, &data, &written, material, look) {
            Ok(mut state) => {
                if let Some(old) = kept
                    && old.file.shader == state.file.shader
                    && old.file.type_name == state.file.type_name
                {
                    state.ty = old.ty;
                } else {
                    table.set(material, look);
                }
                self.typed.push(state);
            }
            Err(why) => {
                table.set(material, look);
                self.fail(asset, why);
            }
        }
    }

    /// Why an asset could not be made, if it could not.
    pub fn problem(&self, asset: AssetRef) -> Option<&str> {
        self.problems.get(&asset).map(String::as_str)
    }

    /// Compresses the textures of glTF files through the derived data cache `cache` (ADR
    /// 0041, E4-8): a texture an earlier run stored there is loaded from it compressed;
    /// another is made uncompressed, compressed in the background (BC7, BC5 for normal
    /// maps, as the cooker does) into the cache, and swapped in once whole on the GPU.
    /// `None`: textures stay uncompressed. Textures made already are kept as they are.
    pub fn set_derived_data(&mut self, cache: Option<DerivedDataCache>) {
        self.cache = cache.map(Arc::new);
    }

    /// Reads file `file` again on its next use: its meshes, materials and textures made
    /// so far are forgotten (and freed in the next frame), what could not be made is tried
    /// again, and every model in `world` that uses the file is made again. What the editor
    /// does after writing the file (a bake over an earlier one).
    pub fn reload(&mut self, world: &mut World, file: AssetId) {
        // What an earlier reading left stale goes; what is made now becomes stale, drawn
        // until the new versions are made.
        self.retire_stale(Some(file));
        let stale_split = &mut self.stale_split;
        self.split.retain(|asset, meshes| {
            let keep = asset.file != file;
            if !keep {
                stale_split.insert(*asset, core::mem::take(meshes));
            }
            keep
        });
        self.building_split.retain(|a| a.file != file);
        // Its skins: let go once no model draws them (its models make theirs again).
        let retired = &mut self.retired_skins;
        self.skinned.retain(|(asset, _), made| {
            let keep = asset.file != file;
            if !keep {
                retired.append(&mut made.skins);
            }
            keep
        });
        self.building_skins.retain(|(a, _)| a.file != file);
        self.clip_events.remove(&file);
        self.fields.forget(file);
        self.graphs.forget(file);
        self.repose_showing(file);
        let (meshes, materials) = (&mut self.stale_meshes, &mut self.stale_materials);
        self.meshes.retain(|asset, &mut mesh| {
            let keep = asset.file != file;
            if !keep {
                meshes.insert(*asset, mesh);
            }
            keep
        });
        self.materials.retain(|asset, &mut material| {
            let keep = asset.file != file;
            if !keep {
                materials.insert(*asset, material);
            }
            keep
        });
        // What was compressed from the old version is not swapped in.
        self.compressed.retain(|(k, _)| k.file != file);
        self.compressed_textures.retain(|k| k.file != file);
        let retired = &mut self.retired_textures;
        self.swapping.retain(|swap| {
            let keep = swap.key.file != file;
            if !keep {
                retired.push(swap.new);
            }
            keep
        });
        let textures = &mut self.stale_textures;
        self.textures.retain(|key, made| {
            let keep = key.file != file;
            if !keep && let Some(texture) = made {
                textures.push((file, *texture));
            }
            keep
        });
        // Stale materials take no more textures, nor parameters.
        let stale = &self.stale_materials;
        self.pending_materials
            .retain(|m| !stale.values().any(|&id| id == m.material));
        self.typed
            .retain(|t| !stale.values().any(|&id| id == t.material));
        *self.generations.entry(file).or_default() += 1;
        self.building.retain(|a| a.file != file);
        self.decoding.retain(|k| k.file != file);
        self.decoded.retain(|(k, _)| k.file != file);
        self.problems.retain(|asset, _| asset.file != file);
        self.gltf.remove(&file);
        // Cooked again when next asked for.
        self.read_to_cook.remove(&file);
        if self.colliders.remove(&file).is_some() || self.cooking.remove(&file) {
            self.collider_epoch += 1;
        }
        let mut models = QueryState::<(Entity, &Model)>::new(world);
        let using: Vec<Entity> = models
            .iter_readonly(world)
            .filter(|(_, m)| m.mesh.file == file || m.material.file == file)
            .map(|(e, _)| e)
            .collect();
        for e in using {
            world.mark_changed::<Model>(e);
        }
        // Its sprites' images: their settings read again, their textures made again.
        for slot in 0..self.sprite_images.len() {
            if self.sprite_images[slot].asset.file == file {
                let asset = self.sprite_images[slot].asset;
                let (settings, frames, clips) = self.sprite_settings(asset);
                self.sprite_images[slot].settings = settings;
                self.sprite_images[slot].frames = frames;
                self.sprite_images[slot].clips = clips;
                self.load_sprite_image(slot);
            }
        }
    }

    /// Asset file `file` at `path` (under the root) was written anew: what was made from it
    /// is made again in the background, its old version drawn until then; an image makes
    /// the glTF files that use it again.
    pub fn changed(&mut self, world: &mut World, file: AssetId, path: &str) {
        self.reload(world, file);
        if let Some(users) = self.depends.get(path).cloned() {
            for user in users {
                self.reload(world, user);
            }
        }
    }

    /// The stale parts of `file` (of every file for none) let go: freed once the scene has
    /// been extracted since ([`Self::sync`]).
    fn retire_stale(&mut self, file: Option<AssetId>) {
        let of = |f: AssetId| file.is_none_or(|file| file == f);
        let retired_meshes = &mut self.retired_meshes;
        self.stale_split.retain(|asset, meshes| {
            let go = of(asset.file);
            if go {
                retired_meshes.append(meshes);
            }
            !go
        });
        self.stale_meshes.retain(|asset, &mut mesh| {
            let go = of(asset.file);
            if go {
                retired_meshes.push(mesh);
            }
            !go
        });
        let retired_materials = &mut self.retired_materials;
        self.stale_materials.retain(|asset, &mut material| {
            let go = of(asset.file);
            if go {
                retired_materials.push(material);
            }
            !go
        });
        let retiring_textures = &mut self.retiring_textures;
        self.stale_textures.retain(|&(f, texture)| {
            let go = of(f);
            if go {
                retiring_textures.push(texture);
            }
            !go
        });
    }

    /// How many assets are still being made: files being read, meshes and textures being
    /// made, and materials waiting for their textures.
    pub fn loading(&self) -> usize {
        let reading = self
            .gltf
            .values()
            .filter(|s| matches!(s, GltfState::Reading))
            .count();
        reading
            + self.building.len()
            + self.decoding.len()
            + self.decoded.len()
            + self.pending_materials.len()
    }

    /// Whether material `material` has its textures (or none to wait for).
    pub(crate) fn settled(&self, material: MaterialId) -> bool {
        !self
            .pending_materials
            .iter()
            .any(|p| p.material == material)
            && !self
                .typed
                .iter()
                .any(|t| t.material == material && !t.settled())
    }

    /// How many textures have been made from glTF files so far.
    pub fn textures_made(&self) -> usize {
        self.textures.values().filter(|t| t.is_some()).count()
    }

    /// How many textures are being compressed in the background, or compressed and not
    /// swapped in yet ([`Self::set_derived_data`]): not counted as loading, the scene shows
    /// whole without them.
    pub fn compressing(&self) -> usize {
        self.loader.compressing() + self.compressed.len() + self.swapping.len()
    }

    /// How many of the textures made from glTF files are compressed: loaded from the
    /// derived data cache, or compressed in the background and swapped in.
    pub fn textures_compressed(&self) -> usize {
        self.compressed_textures.len()
    }

    /// The collider of mesh `asset` (ADR 0117): a built-in mesh's own shape, or what the
    /// cook makes of a glTF mesh by its file's rules (level geometry by default, as a
    /// pack's). A glTF file's are cooked in the background the first time one is asked
    /// for (the file read first if it is not in memory): [`ColliderState::Coming`] until
    /// then. A file written anew is cooked again when next asked for.
    pub fn collider(&mut self, asset: AssetRef) -> ColliderState {
        let made = |c: Option<&Arc<ModelCollider>>| {
            c.map_or(ColliderState::None, |c| ColliderState::Made(Arc::clone(c)))
        };
        if asset.is_none() {
            return ColliderState::None;
        }
        if asset.file == builtin::LIBRARY {
            let c = self
                .builtin_colliders
                .entry(asset)
                .or_insert_with(|| builtin_collider(asset).map(Arc::new));
            return made(c.as_ref());
        }
        match self.colliders.get(&asset.file) {
            Some(Ok(cooked)) => {
                return made(
                    mesh_index(&cooked.names, asset.part)
                        .and_then(|i| cooked.meshes.get(i)?.as_ref()),
                );
            }
            Some(Err(_)) => return ColliderState::None,
            None if self.cooking.contains(&asset.file) => return ColliderState::Coming,
            None => {}
        }
        let Some(path) = file_path(&self.paths, asset).filter(|p| is_gltf(p)) else {
            return ColliderState::None;
        };
        let path = path.to_owned();
        match self.gltf(asset.file, &path) {
            Ok(Some(file)) => {
                self.cook(asset.file, file);
                ColliderState::Coming
            }
            // Being read: cooked once it is.
            Ok(None) => {
                self.cooking.insert(asset.file);
                self.read_to_cook.insert(asset.file);
                ColliderState::Coming
            }
            Err(_) => ColliderState::None,
        }
    }

    /// The colliders of glTF file `id`, read as `file`, cooked in the background.
    fn cook(&mut self, id: AssetId, file: Arc<GltfFile>) {
        let generation = self.generation(id);
        self.cooking.insert(id);
        self.loader.give(Job::Colliders {
            file: id,
            gltf: file,
            cache: self.cache.clone(),
            generation,
        });
    }

    /// Raised each time a model's collider is made or forgotten ([`Self::collider`]): what
    /// keeps colliders of its own (the editor's lines, a game's bodies) compares.
    pub fn collider_epoch(&self) -> u64 {
        self.collider_epoch
    }

    /// glTF files whose colliders are being cooked.
    pub fn cooking(&self) -> usize {
        self.cooking.len()
    }

    fn fail(&mut self, asset: AssetRef, why: String) {
        let path = self.paths.get(asset).unwrap_or("");
        log::warn!(target: "engine_app::assets", "asset {path} ({}): {why}", asset.file);
        self.problems.insert(asset, why);
    }

    /// The mesh of a built-in mesh asset or a glTF file's, made on first use.
    pub fn mesh(&mut self, asset: AssetRef, geometry: &mut GeometryPool) -> Option<MeshId> {
        if let Some(&mesh) = self.meshes.get(&asset) {
            return Some(mesh);
        }
        if asset.is_none() || self.problems.contains_key(&asset) {
            return None;
        }
        if self.building.contains(&asset) {
            return None;
        }
        if let Some(data) = builtin_mesh(asset) {
            return self.add_mesh(asset, &data, geometry);
        }
        // A glTF's: made in the background, once its file is read.
        let given = match file_path(&self.paths, asset) {
            Some(path) if is_gltf(path) => {
                let path = path.to_owned();
                self.gltf(asset.file, &path).and_then(|file| {
                    let Some(file) = file else {
                        return Ok(());
                    };
                    let mesh = mesh_part(&file.scene, asset.part)?;
                    let generation = self.generation(asset.file);
                    self.building.insert(asset);
                    self.loader.give(Job::Mesh {
                        asset,
                        file,
                        mesh,
                        generation,
                    });
                    Ok(())
                })
            }
            Some(path) => Err(format!(
                "{path} is not a mesh: meshes are built in or in glTF files (.glb, .gltf)"
            )),
            None => Err(String::from("no path known for it")),
        };
        if let Err(why) = given {
            self.fail(asset, why);
        }
        None
    }

    /// Mesh `asset` made from `data` in the geometry pool.
    fn add_mesh(
        &mut self,
        asset: AssetRef,
        data: &OwnedMeshData,
        geometry: &mut GeometryPool,
    ) -> Option<MeshId> {
        match geometry.add_mesh(&data.as_data()) {
            Ok(mesh) => {
                self.meshes.insert(asset, mesh);
                Some(mesh)
            }
            Err(e) => {
                self.fail(asset, e.to_string());
                None
            }
        }
    }

    /// The material of the built-in material, a material file or a glTF file's, made on
    /// first use (a glTF material's textures in the same frame, by
    /// [`Self::make_textures`]).
    pub fn material(&mut self, asset: AssetRef, table: &mut MaterialTable) -> Option<MaterialId> {
        if let Some(&material) = self.materials.get(&asset) {
            return Some(material);
        }
        if asset.is_none() || self.problems.contains_key(&asset) {
            return None;
        }
        if asset != builtin::MATERIAL && file_path(&self.paths, asset).is_some_and(is_gltf) {
            return match self.gltf_material(asset, table) {
                Ok(material) => material,
                Err(why) => {
                    self.fail(asset, why);
                    None
                }
            };
        }
        let file = if asset == builtin::MATERIAL {
            Ok((MaterialFile::default(), AssetPaths::new()))
        } else {
            self.read_material(asset).map(|(file, written, bases)| {
                self.note_bases(asset.file, &bases);
                (file, written)
            })
        };
        match file {
            Ok((file, written)) => {
                let look = material_desc(&file);
                let material = table.add(look);
                self.materials.insert(asset, material);
                // A material type's instance (ADR 0120's E10-1b): its look until its type's
                // parameters are laid out.
                if file.is_typed() {
                    match self.typed_state(asset, &file, &written, material, look) {
                        Ok(state) => self.typed.push(state),
                        Err(why) => self.fail(asset, why),
                    }
                }
                Some(material)
            }
            Err(why) => {
                self.fail(asset, why);
                None
            }
        }
    }

    /// A glTF file's material, once its file is read (none before): added without its
    /// textures, which are decoded in the background and given to it as they are made.
    fn gltf_material(
        &mut self,
        asset: AssetRef,
        table: &mut MaterialTable,
    ) -> Result<Option<MaterialId>, String> {
        let path = file_path(&self.paths, asset)
            .ok_or("no path known for it")?
            .to_owned();
        let Some(file) = self.gltf(asset.file, &path)? else {
            return Ok(None);
        };
        let k = material_part(&file.scene, asset.part)?;
        let (desc, slots) =
            gltf_material(asset.file, &file.scene.materials[k], &file.scene.textures);
        let generation = self.generation(asset.file);
        for (key, _) in slots.iter().flatten() {
            if self.textures.contains_key(key) || !self.decoding.insert(*key) {
                continue;
            }
            self.loader.give(Job::Texture {
                key: *key,
                file: Arc::clone(&file),
                cache: self.cache.clone(),
                generation,
            });
        }
        let material = table.add(desc);
        self.materials.insert(asset, material);
        if slots.iter().any(Option::is_some) {
            self.pending_materials.push(PendingMaterial {
                material,
                desc,
                slots,
            });
        }
        Ok(Some(material))
    }

    /// glTF file `file` at `path` as loading has it: read (and marked used this frame),
    /// not yet (its reading begun the first time), or failed.
    fn gltf(&mut self, file: AssetId, path: &str) -> Result<Option<Arc<GltfFile>>, String> {
        let frame = self.frame;
        match self.gltf.get_mut(&file) {
            Some(GltfState::Read { file, used }) => {
                *used = frame;
                Ok(Some(Arc::clone(file)))
            }
            Some(GltfState::Reading) => Ok(None),
            Some(GltfState::Failed(why)) => Err(why.clone()),
            None => {
                let generation = self.generation(file);
                self.gltf.insert(file, GltfState::Reading);
                self.loader.give(Job::Import {
                    file,
                    path: path.to_owned(),
                    generation,
                });
                Ok(None)
            }
        }
    }

    fn generation(&self, file: AssetId) -> u32 {
        self.generations.get(&file).copied().unwrap_or(0)
    }

    /// The images beside glTF file `file` that it reads, noted for [`Self::changed`].
    fn note_images(&mut self, file: AssetId, read: &GltfFile) {
        for texture in &read.scene.textures {
            if let TextureSource::Uri(uri) = &texture.source
                && let Some(path) = project_path(&self.root, &read.dir.join(uri))
            {
                let users = self.depends.entry(path).or_default();
                if !users.contains(&file) {
                    users.push(file);
                }
            }
        }
    }

    /// The loader's results taken: files read, meshes made, textures decoded (made in
    /// [`Self::make_textures`]). Waits a moment for one with `wait`. Whether any came.
    fn take_done(
        &mut self,
        geometry: &mut GeometryPool,
        skinning: &mut Skinning,
        wait: bool,
    ) -> bool {
        let mut done = core::mem::take(&mut self.done);
        if wait {
            self.loader.wait(&mut done);
        } else {
            self.loader.take(&mut done);
        }
        let came = !done.is_empty();
        for result in done.drain(..) {
            match result {
                Done::Imported {
                    file,
                    generation,
                    result,
                } if generation == self.generation(file) => {
                    let state = match result {
                        Ok(read) => {
                            self.note_images(file, &read);
                            // Its colliders were asked for while it was read.
                            if self.read_to_cook.remove(&file) {
                                self.cook(file, Arc::clone(&read));
                            }
                            GltfState::Read {
                                file: read,
                                used: self.frame,
                            }
                        }
                        Err(why) => {
                            if self.read_to_cook.remove(&file) {
                                self.cooking.remove(&file);
                                self.colliders.insert(file, Err(why.clone()));
                                self.collider_epoch += 1;
                            }
                            GltfState::Failed(why)
                        }
                    };
                    self.gltf.insert(file, state);
                }
                Done::Mesh {
                    asset,
                    generation,
                    result,
                } if generation == self.generation(asset.file) => {
                    self.building.remove(&asset);
                    let made = result.and_then(|(data, attributes)| {
                        geometry
                            .add_mesh_with_attributes(&data.as_data(), &attributes)
                            .map_err(|e| e.to_string())
                    });
                    match made {
                        Ok(mesh) => {
                            self.meshes.insert(asset, mesh);
                        }
                        Err(why) => self.fail(asset, why),
                    }
                }
                Done::Primitives {
                    asset,
                    generation,
                    result,
                } if generation == self.generation(asset.file) => {
                    self.building_split.remove(&asset);
                    let made = result.and_then(|parts| {
                        let mut meshes = Vec::with_capacity(parts.len());
                        for (data, attributes) in &parts {
                            match geometry.add_mesh_with_attributes(&data.as_data(), attributes) {
                                Ok(mesh) => meshes.push(mesh),
                                Err(e) => {
                                    // Never drawn: let go at once.
                                    for mesh in meshes {
                                        geometry.remove_mesh(mesh, self.frame);
                                    }
                                    return Err(e.to_string());
                                }
                            }
                        }
                        Ok(meshes)
                    });
                    match made {
                        Ok(meshes) => {
                            self.split.insert(asset, meshes);
                        }
                        Err(why) => self.fail(asset, why),
                    }
                }
                Done::Skin {
                    asset,
                    split,
                    generation,
                    result,
                } if generation == self.generation(asset.file) => {
                    self.building_skins.remove(&(asset, split));
                    let frame = self.frame;
                    let made = result.and_then(|(parts, rig)| {
                        let mut skins = Vec::with_capacity(parts.len());
                        for p in &parts {
                            let data = SkinnedMeshData {
                                mesh: p.mesh.as_data(),
                                joints: &p.joints,
                                weights: &p.weights,
                                joint_count: rig.joint_count() as u16,
                            };
                            match skinning.add_skin(&data) {
                                Ok(skin) => skins.push(skin),
                                Err(e) => {
                                    for skin in skins {
                                        skinning.remove_skin(skin, geometry, frame);
                                    }
                                    return Err(e.to_string());
                                }
                            }
                        }
                        Ok(SkinnedAsset { skins, rig })
                    });
                    match made {
                        Ok(made) => {
                            self.skinned.insert((asset, split), made);
                        }
                        Err(why) => self.fail(asset, why),
                    }
                }
                Done::Texture {
                    key,
                    generation,
                    result,
                } if generation == self.generation(key.file) => {
                    self.decoding.remove(&key);
                    self.decoded.push((key, result));
                }
                Done::Colliders {
                    file,
                    generation,
                    result,
                } if generation == self.generation(file) => {
                    self.cooking.remove(&file);
                    if let Err(why) = &result {
                        let path = self.paths.get(AssetRef::file(file)).unwrap_or("");
                        log::warn!(
                            target: "engine_app::assets",
                            "{path}: its colliders: {why}"
                        );
                    }
                    self.colliders.insert(file, result);
                    self.collider_epoch += 1;
                }
                Done::Compressed {
                    key,
                    generation,
                    result,
                } if generation == self.generation(key.file) => match result {
                    Ok(texture) => self.compressed.push((key, texture)),
                    Err(why) => {
                        let path = self.paths.get(AssetRef::file(key.file)).unwrap_or("");
                        let texture = key.texture;
                        log::warn!(
                            target: "engine_app::assets",
                            "{path}: texture {texture}: compressing it failed, it stays \
                             uncompressed: {why}"
                        );
                    }
                },
                // From a file read again since.
                _ => {}
            }
        }
        self.done = done;
        came
    }

    /// Makes the textures glTF materials made this frame use, and gives the materials
    /// them: run where the device and the texture pool are at hand, after the frame's
    /// stages. A texture that cannot be made leaves its slot empty, said once in the log.
    pub(crate) fn make_textures(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        textures: &mut TexturePool,
        materials: &mut MaterialTable,
        retire: (&Bindless, u64),
    ) {
        for texture in self.retired_textures.drain(..) {
            textures.remove(texture, retire.1, retire.0);
        }
        if !self.swapping.is_empty() {
            self.swap_compressed(textures, materials);
        }
        if self.pending_materials.is_empty()
            && self.decoded.is_empty()
            && self.compressed.is_empty()
        {
            return;
        }
        engine_profiler::zone!("assets::make_textures");
        let decoded = !self.decoded.is_empty();
        let mut made_now = core::mem::take(&mut self.decoded);
        for (key, result) in made_now.drain(..) {
            let (levels, compress, cached) = match result {
                Ok(Made::Decoded { image, compress }) => {
                    (Ok(decoded_levels(image)), compress, false)
                }
                Ok(Made::Cached(texture)) => (Ok(encoded_levels(texture)), None, true),
                Err(why) => (Err(why), None, false),
            };
            let made = levels.and_then(|levels| {
                textures
                    .add(device, allocator, levels)
                    .map_err(|e| e.to_string())
            });
            let made = match made {
                Ok(id) => {
                    if cached {
                        self.compressed_textures.insert(key);
                    }
                    // Not in the cache: compressed in the background, swapped in once done
                    // (not with a built game's read-only pack: it stays as it is).
                    if let (Some(source), Some(cache)) =
                        (compress, self.cache.as_ref().filter(|c| !c.read_only()))
                    {
                        let generation = self.generation(key.file);
                        self.loader.give(Job::Compress {
                            key,
                            source,
                            cache: Arc::clone(cache),
                            generation,
                        });
                    }
                    Some(id)
                }
                Err(why) => {
                    let path = self.paths.get(AssetRef::file(key.file)).unwrap_or("");
                    let texture = key.texture;
                    log::warn!(target: "engine_app::assets", "{path}: texture {texture}: {why}");
                    None
                }
            };
            self.textures.insert(key, made);
        }
        self.decoded = made_now;
        // Textures compressed in the background: made, and swapped in once whole.
        for (key, texture) in self.compressed.drain(..) {
            // Not when what it was made for is gone (failed, or its file read again).
            let Some(&Some(old)) = self.textures.get(&key) else {
                continue;
            };
            match textures.add(device, allocator, encoded_levels(texture)) {
                Ok(new) => self.swapping.push(Swap { key, old, new }),
                Err(why) => {
                    let path = self.paths.get(AssetRef::file(key.file)).unwrap_or("");
                    let texture = key.texture;
                    log::warn!(
                        target: "engine_app::assets",
                        "{path}: texture {texture}: the compressed one: {why}"
                    );
                }
            }
        }
        // Materials given the textures made so far; one leaves the list once each of its
        // slots is made or failed.
        let made_so_far = &self.textures;
        let retry = &mut self.retry_waiting;
        self.pending_materials.retain(|pending| {
            let made = |slot: Option<(TextureKey, [Wrap; 2])>| {
                let (key, wrap) = slot?;
                let texture = (*made_so_far.get(&key)?)?;
                Some(TextureRef { texture, wrap })
            };
            let settled = pending
                .slots
                .iter()
                .flatten()
                .all(|(key, _)| made_so_far.contains_key(key));
            if settled {
                *retry = true;
            }
            if decoded || settled {
                let [color, metallic_roughness, normal, occlusion, emissive] = pending.slots;
                let desc = MaterialDesc {
                    base_color_texture: made(color),
                    metallic_roughness_texture: made(metallic_roughness),
                    normal_texture: made(normal),
                    occlusion_texture: made(occlusion),
                    emissive_texture: made(emissive),
                    ..pending.desc
                };
                materials.set(pending.material, desc);
            }
            !settled
        });
    }

    /// Swaps in the compressed textures whole on the GPU by now (every level uploaded and
    /// shown, so the picture neither blurs nor loses them for a frame): the materials that
    /// use the uncompressed ones take them, and those are freed from the next frame on.
    fn swap_compressed(&mut self, textures: &TexturePool, materials: &mut MaterialTable) {
        let mut i = 0;
        while i < self.swapping.len() {
            let swap = self.swapping[i];
            if textures.shown_level(swap.new) != Some(0.0) {
                i += 1;
                continue;
            }
            self.swapping.swap_remove(i);
            for &material in self.materials.values() {
                let Some(&desc) = materials.get(material) else {
                    continue;
                };
                let mut uses = false;
                let mut swapped = |slot: Option<TextureRef>| match slot {
                    Some(r) if r.texture == swap.old => {
                        uses = true;
                        Some(TextureRef {
                            texture: swap.new,
                            ..r
                        })
                    }
                    slot => slot,
                };
                let desc = MaterialDesc {
                    base_color_texture: swapped(desc.base_color_texture),
                    metallic_roughness_texture: swapped(desc.metallic_roughness_texture),
                    normal_texture: swapped(desc.normal_texture),
                    occlusion_texture: swapped(desc.occlusion_texture),
                    emissive_texture: swapped(desc.emissive_texture),
                    ..desc
                };
                if uses {
                    materials.set(material, desc);
                }
            }
            self.textures.insert(swap.key, Some(swap.new));
            self.compressed_textures.insert(swap.key);
            self.retired_textures.push(swap.old);
        }
    }

    /// Material file `asset` read (as the editor holds it, if it does) over its bases (a
    /// variant's, E10-4a, each as the editor holds it or from its file): the file, the paths
    /// its references were written with, and its bases (each one's id and file).
    #[allow(clippy::type_complexity)]
    fn read_material(
        &self,
        asset: AssetRef,
    ) -> Result<(MaterialFile, AssetPaths, Vec<(AssetId, Option<PathBuf>)>), String> {
        if asset.part == 0
            && let Some((file, written)) = self.held.get(&asset.file)
        {
            return Ok((file.clone(), written.clone(), Vec::new()));
        }
        if asset.part != 0 {
            return Err(String::from(
                "a material is a whole file, not a part of one",
            ));
        }
        let path = self
            .paths
            .get(asset)
            .filter(|p| !p.is_empty())
            .ok_or("no path known for it")?;
        let full = self.root.join(path);
        let text =
            std::fs::read_to_string(&full).map_err(|e| format!("{}: {e}", full.display()))?;
        let held = |r: AssetRef| {
            if r.part == 0 {
                self.held.get(&r.file).cloned()
            } else {
                None
            }
        };
        let read =
            MaterialFile::read_with_bases(&text, &self.root, &held).map_err(|e| match &e.path {
                Some(_) => e.to_string(),
                None => format!("{path}: {}", e.message),
            })?;
        for (line, warning) in &read.report.warnings {
            log::warn!(target: "engine_app::assets", "{path}:{line}: {warning}");
        }
        match read.report.id {
            Some(id) if id != asset.file => Err(format!(
                "{path} holds the material {id}, not {}",
                asset.file
            )),
            _ => Ok((read.file, read.report.assets, read.bases)),
        }
    }

    /// Material file `file`'s bases (`bases`, E10-4a) noted: their files as files it reads
    /// ([`Self::changed`] makes it again when one is written anew), and it as their
    /// variant ([`Self::set_material`] makes it again when the editor holds one anew).
    fn note_bases(&mut self, file: AssetId, bases: &[(AssetId, Option<PathBuf>)]) {
        for (base, at) in bases {
            let variants = self.variants.entry(*base).or_default();
            if !variants.contains(&file) {
                variants.push(file);
            }
            if let Some(under) = at.as_ref().and_then(|at| project_path(&self.root, at)) {
                let users = self.depends.entry(under).or_default();
                if !users.contains(&file) {
                    users.push(file);
                }
            }
        }
    }

    /// Typed material file `asset` (`file`, its references written as `written`) resolved
    /// for its material: its images' keys, and noted as their user, so that an image
    /// written anew reads it again ([`Self::changed`]).
    fn typed_state(
        &mut self,
        asset: AssetRef,
        file: &MaterialFile,
        written: &AssetPaths,
        material: MaterialId,
        look: MaterialDesc,
    ) -> Result<TypedState, String> {
        let path = self.paths.get(asset).unwrap_or("").to_owned();
        let typed = file
            .typed(written, &self.root)
            .map_err(|e| format!("{path}: {}", e.message))?
            .ok_or_else(|| format!("{path}: not a material type's instance"))?;
        // Its images: the file's textures, and a graph's own (ADR 0123) where it gives none,
        // each resolved by the file (`typed.params`).
        let mut images = Vec::new();
        let mut users_of = Vec::new();
        for (name, image) in &typed.textures {
            let Some(ParamValue::Texture(full, encoding)) = typed.params.get(name) else {
                continue;
            };
            let usage = match encoding {
                TextureEncoding::Color => TextureUsage::Color,
                TextureEncoding::Data => TextureUsage::Data,
                TextureEncoding::Normal => TextureUsage::Normal,
            };
            let key = TextureKey::new(image.file, 0, usage);
            users_of.push(full.clone());
            images.push((full.clone(), key));
        }
        // A graph's functions' files (E10-3g-2): it is made again when one changes.
        users_of.extend(typed.reads.iter().cloned());
        // A graph's file: read again when it changes, its module made anew.
        if let Some(at) = written.get(file.shader).filter(|p| p.ends_with(".mgraph")) {
            users_of.push(if Path::new(at).is_absolute() {
                PathBuf::from(at)
            } else {
                self.root.join(at)
            });
        }
        for full in users_of {
            if let Some(under) = project_path(&self.root, &full) {
                let users = self.depends.entry(under).or_default();
                if !users.contains(&asset.file) {
                    users.push(asset.file);
                }
            }
        }
        Ok(TypedState {
            asset,
            material,
            look,
            file: typed,
            images,
            ty: None,
            revision: None,
            bytes: None,
            slots: Vec::new(),
            given: 0,
            as_look: false,
        })
    }

    /// Draws typed material file `material` with its look instead of its type, or with its
    /// type again (E10-4f's Draw as Fallback).
    pub(crate) fn draw_as_fallback(&mut self, material: AssetRef, on: bool) {
        if on {
            self.as_fallback.insert(material);
        } else {
            self.as_fallback.remove(&material);
        }
    }

    /// Typed material files' materials made of their type (ADR 0120's E10-1b), where the
    /// forward renderer is at hand, after the frame's stages: the type registered; the
    /// parameters laid out once its layout is reflected (on the forward renderer's
    /// material compiler thread, never this one) and again when a reload of its module
    /// moves the layout; their images asked for and given as they are made. Until then a
    /// material draws its look, as it does while its type's pipelines compile. Nothing
    /// when no typed file is loaded.
    pub(crate) fn make_material_types(
        &mut self,
        forward: &mut ForwardRenderer,
        materials: &mut MaterialTable,
    ) {
        if self.typed.is_empty() {
            return;
        }
        // Those whose material went (read again and retired, forgotten) go too.
        self.typed
            .retain(|t| materials.material_type(t.material).is_some());
        let Self {
            root,
            typed,
            material_pack,
            as_fallback,
            textures,
            decoding,
            loader,
            cache,
            generations,
            paths,
            problems,
            retry_waiting,
            ..
        } = self;
        for t in typed.iter_mut() {
            // Models drawing a stale version of it try again once it settles.
            let was = t.settled();
            let ty = *t.ty.get_or_insert_with(|| {
                let desc = MaterialTypeDesc {
                    name: t.file.type_name.clone(),
                    module: t.file.shader.clone(),
                    type_name: t.file.type_name.clone(),
                };
                // A built game's: compiled ahead, its parameters' layout with it.
                let packed = material_pack.as_ref().and_then(|pack| {
                    pack.find(&project_path(root, &t.file.shader)?, &t.file.type_name)
                });
                match packed {
                    Some(packed) => forward.register_material_type_packed(
                        desc,
                        &packed.entries,
                        packed.layout.clone(),
                    ),
                    None => forward.register_material_type(desc),
                }
            });
            if let Some((layout, revision)) = forward.material_params(ty)
                && t.revision != Some(revision)
            {
                t.revision = Some(revision);
                let built = layout.map_err(str::to_owned).and_then(|layout| {
                    let built = t.file.build_params(layout).map_err(|e| e.message)?;
                    if built.bytes.len() > PARAMS_BYTES {
                        return Err(format!(
                            "`{}` is {} bytes; parameters are at most {PARAMS_BYTES}",
                            layout.name,
                            built.bytes.len()
                        ));
                    }
                    Ok(built)
                });
                match built {
                    Ok(built) => {
                        t.slots.clear();
                        for param in &built.textures {
                            let Some(&(_, key)) = t.images.iter().find(|(p, _)| *p == param.path)
                            else {
                                continue;
                            };
                            t.slots.push((param.offset, key));
                            if !textures.contains_key(&key) && decoding.insert(key) {
                                loader.give(Job::Image {
                                    key,
                                    path: param.path.clone(),
                                    cache: cache.clone(),
                                    generation: generations.get(&key.file).copied().unwrap_or(0),
                                });
                            }
                        }
                        t.bytes = Some(built.bytes);
                        t.given = usize::MAX;
                        problems.remove(&t.asset);
                    }
                    Err(why) => {
                        let path = paths.get(t.asset).unwrap_or("");
                        log::error!(target: "engine_app::assets", "{path}: {why}; its look draws");
                        problems.insert(t.asset, why);
                        t.bytes = None;
                        t.slots.clear();
                        materials.set(t.material, t.look);
                    }
                }
            }
            if !was && t.settled() {
                *retry_waiting = true;
            }
            // Its look while the editor compares what its type costs (E10-4f); its
            // parameters given again once that ends.
            if as_fallback.contains(&t.asset) {
                if !t.as_look {
                    t.as_look = true;
                    materials.set(t.material, t.look);
                }
                continue;
            }
            if core::mem::take(&mut t.as_look) {
                t.given = usize::MAX;
            }
            // The parameters with the images made so far, given again as more are made.
            let Some(bytes) = &t.bytes else {
                continue;
            };
            let made = t
                .slots
                .iter()
                .filter(|(_, key)| textures.contains_key(key))
                .count();
            if made == t.given {
                continue;
            }
            t.given = made;
            let params = MaterialParams {
                bytes: bytes.clone(),
                textures: t
                    .slots
                    .iter()
                    .filter_map(|&(offset, key)| {
                        let texture = (*textures.get(&key)?)?;
                        let wrap = [Wrap::Repeat; 2];
                        Some((offset, TextureRef { texture, wrap }))
                    })
                    .collect(),
            };
            materials.set_typed(t.material, ty, t.look, Some(params));
            if !was && t.settled() {
                *retry_waiting = true;
            }
        }
    }

    /// Gives every entity whose [`Model`] changed a `MeshRenderer` of its mesh and material,
    /// and takes it from those whose model went. Runs in the player's frame after the
    /// game's ticks (each extracting the scene) and presentation: the renderers given here
    /// reach the scene in the next tick's extraction, new meshes and materials reach the
    /// GPU in this frame. `extracted`: the scene was extracted this frame, so the parts
    /// renderers let go of before are drawn no more and are freed. Visits only chunks where
    /// models were written.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sync(
        &mut self,
        world: &mut World,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        skinning: &mut Skinning,
        frame: u64,
        wait: bool,
        extracted: bool,
        alpha: f32,
    ) {
        self.frame = frame;
        if extracted {
            for mesh in self.retired_meshes.drain(..) {
                geometry.remove_mesh(mesh, frame);
            }
            for material in self.retired_materials.drain(..) {
                materials.remove(material);
            }
            self.retired_textures.append(&mut self.retiring_textures);
        }
        let mut came = self.take_done(geometry, skinning, false);
        let changes = self.changes.get_or_insert_with(|| QueryState::new(world));
        self.changed.clear();
        self.changed
            .extend(changes.iter_readonly(world).map(|(e, model)| (e, *model)));
        // Material slots changed, or gone from a model drawn per primitive (ADR 0115).
        let slot_changes = self
            .slot_changes
            .get_or_insert_with(|| QueryState::new(world));
        let unsplit = self.unsplit.get_or_insert_with(|| QueryState::new(world));
        let before = self.changed.len();
        self.changed.extend(
            slot_changes
                .iter_readonly(world)
                .map(|(e, model)| (e, *model)),
        );
        self.changed
            .extend(unsplit.iter_readonly(world).map(|(e, model)| (e, *model)));
        if self.changed.len() > before {
            self.changed.sort_unstable_by_key(|&(e, _)| e.to_bits());
            self.changed.dedup_by_key(|&mut (e, _)| e);
        }
        for i in 0..self.changed.len() {
            let (e, model) = self.changed[i];
            if self.give_renderer(world, e, model, geometry, materials, skinning) {
                self.waiting.remove(&e);
            } else {
                self.waiting.insert(e);
            }
        }
        let since = std::time::Instant::now();
        came |= core::mem::take(&mut self.retry_waiting);
        loop {
            // The models waiting, again once something came back.
            if came && !self.waiting.is_empty() {
                self.retry.clear();
                self.retry.extend(self.waiting.drain());
                // In the entities' order, so materials are numbered the same each run.
                self.retry.sort_unstable_by_key(|e| e.to_bits());
                for i in 0..self.retry.len() {
                    let e = self.retry[i];
                    if let Some(model) = world.get::<Model>(e).copied()
                        && !self.give_renderer(world, e, model, geometry, materials, skinning)
                    {
                        self.waiting.insert(e);
                    }
                }
            }
            // The frame after a scene was loaded waits for its assets, up to a point.
            if !wait || !self.loader.busy() {
                break;
            }
            if since.elapsed() > WAIT_AT_MOST {
                log::warn!(
                    target: "engine_app::assets",
                    "assets still loading after {} s: the frame goes on, they come later",
                    WAIT_AT_MOST.as_secs()
                );
                break;
            }
            came = self.take_done(geometry, skinning, true);
        }
        let orphans = self.orphans.get_or_insert_with(|| QueryState::new(world));
        self.gone.clear();
        self.gone.extend(orphans.iter_readonly(world));
        for i in 0..self.gone.len() {
            let e = self.gone[i];
            self.release_skin(world, e, skinning, geometry);
            world.remove::<FromModel>(e);
            world.remove::<MeshRenderer>(e);
            world.remove::<Split>(e);
            self.waiting.remove(&e);
        }
        self.animate(world, alpha);
        self.sync_skins(world, skinning, geometry);
        // Parts whose model went or draws whole again (a model despawned alone leaves its
        // children a dead parent): gone.
        let parts = self.parts.get_or_insert_with(|| QueryState::new(world));
        self.gone.clear();
        self.gone.extend(parts.iter_readonly(world).filter(|&p| {
            world
                .parent(p)
                .is_none_or(|model| world.get::<Split>(model).is_none())
        }));
        for &p in &self.gone {
            world.despawn(p);
        }
        // A model drawing a stale part waits for its new one: with none waiting, none is
        // drawn.
        if self.waiting.is_empty()
            && !(self.stale_meshes.is_empty()
                && self.stale_split.is_empty()
                && self.stale_materials.is_empty()
                && self.stale_textures.is_empty())
        {
            self.retire_stale(None);
        }
        // Files no part is made from any more: let go a frame on (their parts kept).
        if !self.gltf.is_empty() {
            self.gltf.retain(|_, state| match state {
                GltfState::Read { used, .. } => *used + 1 >= frame,
                GltfState::Reading | GltfState::Failed(_) => true,
            });
        }
    }

    /// Entity `e`'s renderer from its model: given, or taken away while its mesh or
    /// material cannot be had. False while one is still being made (the model waits).
    fn give_renderer(
        &mut self,
        world: &mut World,
        e: Entity,
        model: Model,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        skinning: &mut Skinning,
    ) -> bool {
        // Drawn skinned where a node of its file draws its mesh with a skin (ADR 0124's
        // E12-1a); a model that was is drawn plain again without its instances.
        match self.skin_check(model.mesh) {
            SkinCheck::Skinned(file, mesh) => {
                return self
                    .give_skinned(world, e, model, &file, mesh, geometry, materials, skinning);
            }
            SkinCheck::Reading => {
                self.release_skin(world, e, skinning, geometry);
                return false;
            }
            SkinCheck::Plain => self.release_skin(world, e, skinning, geometry),
        }
        // Material slots on a glTF mesh: a mesh per primitive, each with its own material.
        let slots = world
            .get::<MaterialSlots>(e)
            .filter(|s| !s.0.is_empty())
            .map(|s| s.0.clone());
        if let Some(slots) = slots
            && model.mesh.file != builtin::LIBRARY
            && !model.mesh.is_none()
        {
            return self.give_split(world, e, model, &slots, materials);
        }
        // Drawn whole (again): its parts go with the sync's sweep.
        world.remove::<Split>(e);
        let (mesh, stale_mesh) = self.drawn_mesh(model.mesh, geometry);
        let (material, stale_material) = self.drawn_material(model.material, materials);
        match (mesh, material) {
            (Some(mesh), Some(material)) => {
                let renderer = MeshRenderer {
                    mesh,
                    material,
                    visible: u32::from(!model.hidden),
                };
                if world.get::<MeshRenderer>(e) != Some(&renderer) {
                    world.insert(e, renderer);
                }
                if world.get::<FromModel>(e).is_none() {
                    world.insert(e, FromModel);
                    // Drawn where its transform puts it: a model spawned with a
                    // `Transform` alone gets the world transform propagation fills.
                    if world.get::<GlobalTransform>(e).is_none() {
                        world.insert(e, GlobalTransform::IDENTITY);
                    }
                }
                !(stale_mesh || stale_material)
            }
            (mesh, material) => {
                if world.remove::<FromModel>(e).is_some() {
                    world.remove::<MeshRenderer>(e);
                }
                // Still to come unless it failed.
                let coming = |made: bool, asset: AssetRef| {
                    !made && !asset.is_none() && !self.problems.contains_key(&asset)
                };
                !(coming(mesh.is_some(), model.mesh) || coming(material.is_some(), model.material))
            }
        }
    }

    /// Mesh `asset` as a renderer draws it: made, else, until its new version is made, the
    /// version of a file read again (E4-5). Whether it is that stale one; a stale one its
    /// new version replaced goes.
    fn drawn_mesh(
        &mut self,
        asset: AssetRef,
        geometry: &mut GeometryPool,
    ) -> (Option<MeshId>, bool) {
        let drawn = match (self.mesh(asset, geometry), self.stale_meshes.get(&asset)) {
            (Some(mesh), _) => (Some(mesh), false),
            (None, Some(&old)) => (Some(old), true),
            (None, None) => (None, false),
        };
        if !drawn.1
            && let Some(old) = self.stale_meshes.remove(&asset)
        {
            self.retired_meshes.push(old);
        }
        drawn
    }

    /// Material `asset` as a renderer draws it: made with its textures, else the version
    /// of a file read again until then (E4-5). Whether it is that stale one; a stale one
    /// its new version replaced goes, the file's old textures with its last.
    fn drawn_material(
        &mut self,
        asset: AssetRef,
        materials: &mut MaterialTable,
    ) -> (Option<MaterialId>, bool) {
        let made = self.material(asset, materials);
        let unsettled = |m: MaterialId| {
            self.pending_materials.iter().any(|p| p.material == m)
                || self.typed.iter().any(|t| t.material == m && !t.settled())
        };
        let drawn = match (made, self.stale_materials.get(&asset)) {
            (Some(new), Some(&old)) if unsettled(new) => (Some(old), true),
            (None, Some(&old)) => (Some(old), true),
            (made, _) => (made, false),
        };
        if !drawn.1
            && let Some(old) = self.stale_materials.remove(&asset)
        {
            self.retired_materials.push(old);
            let file = asset.file;
            if !self.stale_materials.keys().any(|a| a.file == file) {
                let retiring = &mut self.retiring_textures;
                self.stale_textures.retain(|&(f, texture)| {
                    if f == file {
                        retiring.push(texture);
                    }
                    f != file
                });
            }
        }
        drawn
    }

    /// Mesh `asset` of a glTF file made a mesh per primitive (ADR 0115), on first use: in
    /// the background, as [`Self::mesh`].
    fn split_mesh(&mut self, asset: AssetRef) -> Option<Vec<MeshId>> {
        if let Some(meshes) = self.split.get(&asset) {
            return Some(meshes.clone());
        }
        if asset.is_none()
            || self.problems.contains_key(&asset)
            || self.building_split.contains(&asset)
        {
            return None;
        }
        let given = match file_path(&self.paths, asset) {
            Some(path) if is_gltf(path) => {
                let path = path.to_owned();
                self.gltf(asset.file, &path).and_then(|file| {
                    let Some(file) = file else {
                        return Ok(());
                    };
                    let mesh = mesh_part(&file.scene, asset.part)?;
                    let generation = self.generation(asset.file);
                    self.building_split.insert(asset);
                    self.loader.give(Job::Primitives {
                        asset,
                        file,
                        mesh,
                        generation,
                    });
                    Ok(())
                })
            }
            Some(path) => Err(format!(
                "{path} is not a mesh: meshes are built in or in glTF files (.glb, .gltf)"
            )),
            None => Err(String::from("no path known for it")),
        };
        if let Err(why) = given {
            self.fail(asset, why);
        }
        None
    }

    /// [`Self::split_mesh`] as a renderer draws it, as [`Self::drawn_mesh`].
    fn drawn_split(&mut self, asset: AssetRef) -> (Option<Vec<MeshId>>, bool) {
        match (self.split_mesh(asset), self.stale_split.get(&asset)) {
            (Some(meshes), _) => {
                if let Some(old) = self.stale_split.remove(&asset) {
                    self.retired_meshes.extend(old);
                }
                (Some(meshes), false)
            }
            (None, Some(old)) => (Some(old.clone()), true),
            (None, None) => (None, false),
        }
    }

    /// Whether mesh `asset` is drawn skinned (ADR 0124's E12-1a): a node of its glTF file
    /// draws it with a skin. Its file read on first use.
    fn skin_check(&mut self, asset: AssetRef) -> SkinCheck {
        if asset.is_none() || asset.file == builtin::LIBRARY {
            return SkinCheck::Plain;
        }
        let Some(path) = file_path(&self.paths, asset)
            .filter(|p| is_gltf(p))
            .map(str::to_owned)
        else {
            return SkinCheck::Plain;
        };
        match self.gltf(asset.file, &path) {
            Ok(Some(file)) => match mesh_part(&file.scene, asset.part) {
                Ok(mesh) if crate::skins::skinned_node(&file.scene, mesh).is_some() => {
                    SkinCheck::Skinned(file, mesh)
                }
                // A mesh that is not there is the plain path's to report.
                _ => SkinCheck::Plain,
            },
            Ok(None) => SkinCheck::Reading,
            Err(_) => SkinCheck::Plain,
        }
    }

    /// [`Self::give_renderer`] for a skinned mesh (ADR 0124's E12-1a): its skin made in the
    /// background on first use (a part per primitive with material slots), then an instance
    /// of it per part for this model, drawn by the entity and, past the first, by derived
    /// parts as plain split models are; posed in the rest pose until asked otherwise. False
    /// while the skin or a material is still being made.
    #[allow(clippy::too_many_arguments)]
    fn give_skinned(
        &mut self,
        world: &mut World,
        e: Entity,
        model: Model,
        file: &Arc<GltfFile>,
        mesh: usize,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        skinning: &mut Skinning,
    ) -> bool {
        let slots = world
            .get::<MaterialSlots>(e)
            .filter(|s| !s.0.is_empty())
            .map(|s| s.0.clone());
        let split = slots.is_some();
        let key = (model.mesh, split);
        let Some(made) = self.skinned.get(&key) else {
            self.release_skin(world, e, skinning, geometry);
            if self.problems.contains_key(&model.mesh) {
                return true;
            }
            if self.building_skins.insert(key) {
                let generation = self.generation(model.mesh.file);
                self.loader.give(Job::Skin {
                    asset: model.mesh,
                    split,
                    file: Arc::clone(file),
                    mesh,
                    generation,
                });
            }
            return false;
        };
        let (skins, rig) = (made.skins.clone(), Arc::clone(&made.rig));
        let slot = |i: usize| match (&slots, i) {
            (_, 0) | (None, _) => model.material,
            (Some(slots), i) => slots.get(i - 1).copied().unwrap_or(model.material),
        };
        let mut drawn = Vec::with_capacity(skins.len());
        let (mut stale, mut missing) = (false, Vec::new());
        for i in 0..skins.len() {
            let (material, s) = self.drawn_material(slot(i), materials);
            stale |= s;
            match material {
                Some(m) => drawn.push(m),
                None => missing.push(slot(i)),
            }
        }
        if !missing.is_empty() {
            let coming = |asset: &AssetRef| !asset.is_none() && !self.problems.contains_key(asset);
            return !missing.iter().any(coming);
        }
        // Its instances: kept while they are this skin's, else made anew.
        if self.owned.get(&e).is_none_or(|o| o.skins != skins) {
            self.release_skin(world, e, skinning, geometry);
            let mut instances = Vec::with_capacity(skins.len());
            let mut meshes = Vec::with_capacity(skins.len());
            for &skin in &skins {
                match skinning.add_instance(skin, geometry) {
                    Ok((id, mesh)) => {
                        instances.push(id);
                        meshes.push(mesh);
                    }
                    Err(why) => {
                        for id in instances {
                            skinning.remove_instance(id, geometry, self.frame);
                        }
                        self.fail(model.mesh, why.to_string());
                        return true;
                    }
                }
            }
            let events = self.model_events(model.mesh.file, file);
            self.events_dirty = true;
            self.owned.insert(
                e,
                Owned {
                    skins,
                    instances,
                    meshes,
                    rig,
                    file: model.mesh.file,
                    events,
                    look: Look::default(),
                    held: false,
                    posed: false,
                    graph: None,
                },
            );
        }
        let Some(owned) = self.owned.get(&e) else {
            return true;
        };
        let visible = u32::from(!model.hidden);
        let first = MeshRenderer {
            mesh: owned.meshes[0],
            material: drawn[0],
            visible,
        };
        if world.get::<MeshRenderer>(e) != Some(&first) {
            world.insert(e, first);
        }
        let skinned = SkinnedMesh {
            id: owned.instances[0],
        };
        if world.get::<SkinnedMesh>(e) != Some(&skinned) {
            world.insert(e, skinned);
        }
        if world.get::<FromModel>(e).is_none() {
            world.insert(e, FromModel);
            if world.get::<GlobalTransform>(e).is_none() {
                world.insert(e, GlobalTransform::IDENTITY);
            }
        }
        if !split {
            // Drawn whole: parts left from a split go with the sync's sweep.
            world.remove::<Split>(e);
            return !stale;
        }
        if world.get::<Split>(e).is_none() {
            world.insert(e, Split);
        }
        let mut have: Vec<(u32, Entity)> = world
            .children(e)
            .filter_map(|c| world.get::<ModelPart>(c).map(|p| (p.0, c)))
            .collect();
        let rest = owned.instances.iter().zip(&owned.meshes).zip(&drawn);
        for (i, ((&id, &mesh), &material)) in rest.enumerate().skip(1) {
            let renderer = MeshRenderer {
                mesh,
                material,
                visible,
            };
            let skinned = SkinnedMesh { id };
            match have.iter().position(|&(k, _)| k as usize == i) {
                Some(at) => {
                    let (_, part) = have.swap_remove(at);
                    if world.get::<MeshRenderer>(part) != Some(&renderer) {
                        world.insert(part, renderer);
                    }
                    if world.get::<SkinnedMesh>(part) != Some(&skinned) {
                        world.insert(part, skinned);
                    }
                }
                None => {
                    let part = world.spawn((
                        Derived,
                        ModelPart(i as u32),
                        Transform::IDENTITY,
                        GlobalTransform::IDENTITY,
                        renderer,
                        skinned,
                    ));
                    let _ = world.set_parent(part, Some(e));
                }
            }
        }
        for (_, part) in have {
            world.despawn(part);
        }
        !stale
    }

    /// Entity `e`'s instances of its skin let go (ADR 0124's E12-1a), with the components
    /// that drew them.
    fn release_skin(
        &mut self,
        world: &mut World,
        e: Entity,
        skinning: &mut Skinning,
        geometry: &mut GeometryPool,
    ) {
        self.events_dirty = true;
        let Some(owned) = self.owned.remove(&e) else {
            return;
        };
        for id in owned.instances {
            skinning.remove_instance(id, geometry, self.frame);
        }
        if !world.contains(e) {
            return;
        }
        world.remove::<SkinnedMesh>(e);
        if world.remove::<FromModel>(e).is_some() {
            world.remove::<MeshRenderer>(e);
        }
        // Its skinned parts go with it.
        let parts: Vec<Entity> = world
            .children(e)
            .filter(|&c| world.get::<ModelPart>(c).is_some())
            .collect();
        for part in parts {
            world.despawn(part);
        }
        world.remove::<Split>(e);
    }

    /// The skinned models each frame (ADR 0124's E12-1a): those gone let their instances go;
    /// those whose pose changed are posed again; the skins of files read again that no model
    /// draws any more are let go. Allocates nothing when nothing changed.
    /// The characters' sockets this tick (ADR 0124's E12-1c): each [`JointAttachment`] right
    /// under a skinned model placed where its joint is in the model's pose now (the editor's
    /// held one, else its animator's at its time, else its rest pose), in the model's space.
    /// Called in each tick before transforms propagate, so what hangs under a socket follows
    /// its joint; a socket already there is not written.
    pub(crate) fn place_sockets(&mut self, world: &mut World) {
        if self.owned.is_empty() {
            return;
        }
        engine_profiler::zone!("app::Assets::place_sockets");
        let sockets = self.sockets.get_or_insert_with(|| QueryState::new(world));
        self.socket_list.clear();
        self.socket_list.extend(
            sockets
                .iter_readonly(world)
                .map(|(e, parent)| (parent.get(), e)),
        );
        self.socket_list.sort_unstable();
        let mut k = 0;
        while k < self.socket_list.len() {
            let character = self.socket_list[k].0;
            let end = k + self.socket_list[k..]
                .iter()
                .take_while(|(c, _)| *c == character)
                .count();
            if let Some(owned) = self.owned.get(&character) {
                let animator = world.get::<Animator>(character);
                let look = if owned.held {
                    Look {
                        own: owned.look.own,
                        ..Look::at(owned.look.pose)
                    }
                } else if owned.graph.is_some() && animator.is_some_and(Animator::plays_graph) {
                    // Its graph at the tick's moment (E12-5c).
                    Look {
                        graph: Some(1.0),
                        ..Look::default()
                    }
                } else {
                    animator
                        .and_then(|animator| {
                            animator_pose(&owned.rig, animator, 1.0, &owned.events)
                        })
                        .map_or(Look::default(), |(look, _)| look)
                };
                let rig_id = Arc::as_ptr(&owned.rig) as usize as u64;
                let joints = joints_of(&mut self.posing, &self.fields, owned, look);
                let skeleton = owned.rig.skeleton();
                for &(_, socket) in &self.socket_list[k..end] {
                    let Some(attachment) = world.get::<JointAttachment>(socket) else {
                        continue;
                    };
                    // The joint looked up by name once per rig.
                    let j = if attachment.found_rig == rig_id {
                        Some(usize::from(attachment.found_joint))
                    } else {
                        let j = skeleton
                            .find(&attachment.joint)
                            .and_then(|j| u16::try_from(j).ok());
                        if let Some(a) = world.get_mut::<JointAttachment>(socket) {
                            a.found_joint = j.unwrap_or(u16::MAX);
                            a.found_rig = rig_id;
                        }
                        j.map(usize::from)
                    };
                    let Some(at) = j.and_then(|j| joints.get(j)) else {
                        continue;
                    };
                    let (scale, rotation, translation) = at.to_scale_rotation_translation();
                    let placed = Transform::from_translation(translation)
                        .with_rotation(rotation)
                        .with_scale(scale);
                    if world.get::<Transform>(socket) != Some(&placed)
                        && let Some(t) = world.get_mut::<Transform>(socket)
                    {
                        *t = placed;
                    }
                }
            }
            k = end;
        }
    }

    /// The characters' root motion over the tick just run (ADR 0124's E12-2c). Where an
    /// [`Animator`] takes it out of the pose (`RootMotion::Move` or `Report`), its clips'
    /// ground motion over the tick (blended through a fade) goes into its `moved` and
    /// `turned`, `moved` in the entity's parent's space; `Move` also moves the entity's
    /// `Transform` by them. A pose the editor holds moves nothing. Called in each tick
    /// after the animators move on, before the sockets are placed.
    pub(crate) fn move_roots(&mut self, world: &mut World) {
        if self.owned.is_empty() {
            return;
        }
        engine_profiler::zone!("app::Assets::move_roots");
        for (&e, owned) in &self.owned {
            let Some(animator) = world.get::<Animator>(e) else {
                continue;
            };
            let mode = animator.root_motion;
            let motion = if owned.held || mode == RootMotion::InPose {
                None
            } else if animator.plays_graph() {
                // An animation graph's (E12-5c-2): its base layer's clips' motions. A tick
                // the network plays again does not tick the graph, so it moves the character
                // by its last tick's motion again (as each tick it plays again had).
                owned
                    .graph
                    .as_ref()
                    .and_then(|play| graph_motion(&owned.rig, &owned.events, play))
            } else {
                tick_motion(&owned.rig, animator, &owned.events)
            };
            let Some((moved, turned)) = motion else {
                if (animator.moved != Vec3::ZERO || animator.turned != 0.0)
                    && let Some(animator) = world.get_mut::<Animator>(e)
                {
                    animator.moved = Vec3::ZERO;
                    animator.turned = 0.0;
                }
                continue;
            };
            let Some(&t) = world.get::<Transform>(e) else {
                continue;
            };
            let moved = t.rotation * (t.scale * moved);
            if let Some(animator) = world.get_mut::<Animator>(e) {
                animator.moved = moved;
                animator.turned = turned;
            }
            if mode == RootMotion::Move
                && (moved != Vec3::ZERO || turned != 0.0)
                && let Some(t) = world.get_mut::<Transform>(e)
            {
                t.translation += moved;
                if turned != 0.0 {
                    let turn = Quat::from_axis_angle(owned.rig.mesh_up(), turned);
                    t.rotation = (t.rotation * turn).normalize();
                }
            }
        }
    }

    /// The clips' events the tick just run played past (ADR 0124's E12-3b), into the world's
    /// [`AnimationEvents`] in place of the last tick's: per character with an [`Animator`],
    /// in the order of their entities, the events of its clip (through a cross-fade, the
    /// clip that weighs more) over the tick's span of play. An event with a sound plays it
    /// where the character stands, but while the network plays ticks again. A pose the
    /// editor holds fires nothing. Called in each tick after the roots move.
    pub(crate) fn fire_events(&mut self, world: &mut World) {
        let Some(mut fired) = world.resource_mut::<AnimationEvents>().map(core::mem::take) else {
            return;
        };
        fired.clear();
        if core::mem::take(&mut self.events_dirty) {
            self.event_characters.clear();
            self.event_characters.extend(
                self.owned
                    .iter()
                    .filter(|(_, o)| !o.held && !o.events.is_empty())
                    .map(|(&e, o)| (e, Arc::clone(&o.rig), Arc::clone(&o.events))),
            );
            self.event_characters.sort_unstable_by_key(|c| c.0);
        }
        if !self.event_characters.is_empty() || !self.fields.fired.is_empty() {
            engine_profiler::zone!("app::Assets::fire_events");
            let mut sounds = core::mem::take(&mut self.event_sounds);
            let again = engine_net::Resimulating::now(world);
            for (e, rig, events) in &self.event_characters {
                let e = *e;
                let Some(animator) = world.get::<Animator>(e) else {
                    continue;
                };
                // An animation graph's (E12-5c-2): in each layer, the clip that weighs most
                // fires its events over the tick's step. A tick the network plays again
                // does not tick a graph (its state is the player's), so it fires nothing.
                if animator.plays_graph() {
                    if again {
                        continue;
                    }
                    let Some(play) = self.owned.get(&e).and_then(|o| o.graph.as_ref()) else {
                        continue;
                    };
                    let graph = &play.cooked.graph;
                    for l in 0..graph.layer_count() {
                        // A layer that shows nothing fires nothing.
                        if graph.layer_weight(&play.state, l) <= 1e-4 {
                            continue;
                        }
                        let heaviest = graph
                            .played(&play.state, l)
                            .max_by(|a, b| a.weight.total_cmp(&b.weight));
                        let Some(p) = heaviest else {
                            continue;
                        };
                        let k = usize::from(p.clip);
                        let Some(clip) = rig.clips().get(k) else {
                            continue;
                        };
                        let events = events.of_clip(clip.name(), k);
                        crossed(events, clip.duration(), p.from, p.to, |event| {
                            fired.push(AnimationEvent {
                                entity: e,
                                name: Arc::clone(&event.name),
                                data: Arc::clone(&event.data),
                            });
                            if let Some(sound) = &event.sound {
                                sounds.push((e, Arc::clone(sound)));
                            }
                        });
                    }
                    continue;
                }
                let Some((k, from, to)) = played_span(rig, animator) else {
                    continue;
                };
                let clip = &rig.clips()[k];
                let events = events.of_clip(clip.name(), k);
                crossed(events, clip.duration(), from, to, |event| {
                    fired.push(AnimationEvent {
                        entity: e,
                        name: Arc::clone(&event.name),
                        data: Arc::clone(&event.data),
                    });
                    if let Some(sound) = &event.sound {
                        sounds.push((e, Arc::clone(sound)));
                    }
                });
            }
            // Then the clips made in the editor's (E12-4a), in their tick's order.
            for (e, event) in &self.fields.fired {
                fired.push(AnimationEvent {
                    entity: *e,
                    name: Arc::clone(&event.name),
                    data: Arc::clone(&event.data),
                });
                if let Some(sound) = &event.sound {
                    sounds.push((*e, Arc::clone(sound)));
                }
            }
            for (e, sound) in sounds.drain(..) {
                if again {
                    continue;
                }
                let at = world
                    .get::<GlobalTransform>(e)
                    .map_or(Vec3::ZERO, |g| Vec3::from(g.0.translation));
                world.spawn((
                    Transform::from_translation(at),
                    GlobalTransform(Affine3A::from_translation(at)),
                    crate::AudioSource3D::new(&sound).despawned_at_end(),
                ));
            }
            self.event_sounds = sounds;
        }
        if let Some(events) = world.resource_mut::<AnimationEvents>() {
            *events = fired;
        }
    }

    /// One tick of the animators playing clips made in the editor (ADR 0124's E12-4a), after
    /// the animators moved on: the fields they key written at the tick's moment, their
    /// events kept for [`Self::fire_events`].
    pub(crate) fn animate_fields(&mut self, world: &mut World) {
        let (paths, root) = (&self.paths, &self.root);
        self.fields
            .tick(world, |asset| file_path(paths, asset).map(|p| root.join(p)));
    }

    /// Clip file `file` (an `.anim`) shown on `entity` at `time` seconds while the editor
    /// edits (ADR 0124's E12-4b): the fields it keys written as it says there; each one's
    /// value before goes into `kept` once, for the editor to put back before it edits
    /// ([`crate::KeptField::put_back`]), so saves, undo and Play see the scene's own values.
    pub fn preview_anim(
        &mut self,
        world: &mut World,
        entity: Entity,
        file: AssetRef,
        time: f32,
        kept: &mut Vec<crate::KeptField>,
    ) {
        let (paths, root) = (&self.paths, &self.root);
        self.fields
            .preview(world, entity, file, time, kept, |asset| {
                file_path(paths, asset).map(|p| root.join(p))
            });
        // A character: its joints the clip keys, posed over its rest pose (E12-4e), held
        // while the preview lasts ([`Self::end_anim_preview`]).
        if let Some(owned) = self.owned.get_mut(&entity) {
            let look = Look {
                own: Some(crate::skins::OwnPose {
                    file: file.file,
                    time,
                    playhead: true,
                }),
                ..Look::default()
            };
            self.events_dirty |= !owned.held;
            owned.held = true;
            if owned.look != look {
                owned.look = look;
                owned.posed = false;
            }
        }
    }

    /// The editor's preview of a clip made in it no longer shows on character `entity` (E12-4e):
    /// back to its animator's pose.
    pub fn end_anim_preview(&mut self, entity: Entity) {
        if let Some(owned) = self
            .owned
            .get_mut(&entity)
            .filter(|o| o.held && o.look.own.is_some())
        {
            owned.held = false;
            owned.look = Look::default();
            owned.posed = false;
            self.events_dirty = true;
        }
    }

    /// Clip file `file` (an `.anim`) as the editor holds it (ADR 0124's E12-4a), read in
    /// place of the file until `None` gives the file back: its animators follow from the
    /// next tick.
    pub fn set_anim_clip(&mut self, file: AssetId, anim: Option<&engine_scene::AnimFile>) {
        self.fields.hold(file, anim);
        self.repose_showing(file);
    }

    /// Graph file `file` (an `.agraph`) as the editor holds it while it edits it (ADR 0124's
    /// E12-5c), read in place of the file until `None` gives the file back: the characters
    /// playing it are cooked again from their next tick or frame, their states made anew.
    pub fn set_anim_graph(&mut self, file: AssetId, graph: Option<&engine_scene::AnimGraphFile>) {
        self.graphs.hold(file, graph);
    }

    /// The parameters of every animation graph an [`Animator`] names, known to scripts
    /// before they run (ADR 0124's E12-5c): a graph file not read yet is read, its
    /// parameters told to the world's [`engine_runtime::GraphParams`]. Called at each tick's
    /// start.
    pub(crate) fn know_graphs(&mut self, world: &mut World) {
        let (paths, root) = (&self.paths, &self.root);
        self.graphs.know(world, |id| {
            file_path(paths, AssetRef::file(id)).map(|p| root.join(p))
        });
    }

    /// One tick of the characters playing animation graphs (ADR 0124's E12-5c), after the
    /// scripts' tick and before the game's: the parameters each one's animator set go in,
    /// its transitions are taken and its clips move on by the tick times its animator's
    /// speed (held while paused, or at a speed below 0), the pose an inertialized transition
    /// starts from is taken (E12-5b-2), its parameters come back (a trigger a transition took
    /// used up, those not set at the graph's values) and its first layer's state is named in
    /// `Animator::state`. A pose the editor holds moves nothing; ticks the network plays
    /// again move nothing (a graph's state is not part of what it rolls back).
    pub(crate) fn tick_graphs(&mut self, world: &mut World, dt: f32) {
        if self.owned.is_empty() || engine_net::Resimulating::now(world) {
            return;
        }
        engine_profiler::zone!("app::Assets::tick_graphs");
        let (graphs, paths, root, posing, held) = (
            &mut self.graphs,
            &self.paths,
            &self.root,
            &mut self.posing,
            &self.held_params,
        );
        for (&e, owned) in &mut self.owned {
            if owned.held {
                continue;
            }
            let Some(animator) = world.get::<Animator>(e) else {
                continue;
            };
            let path_of = |id| file_path(paths, AssetRef::file(id)).map(|p| root.join(p));
            if sync_graph(graphs, owned, animator, path_of) != GraphSync::Plays {
                continue;
            }
            let (Some(play), Some(animator)) = (owned.graph.as_mut(), world.get_mut::<Animator>(e))
            else {
                continue;
            };
            let graph = &play.cooked.graph;
            set_params(&mut play.state, animator, graph);
            // What the editor holds wins over the game's (E12-5e).
            for &(_, slot, value) in held.iter().filter(|h| h.0 == e) {
                play.state.set(slot, value);
            }
            // Root motion out of its pose where the game moves it by it (E12-5c-2).
            play.state
                .set_in_place(animator.root_motion != RootMotion::InPose);
            let step = if animator.paused {
                0.0
            } else {
                dt * animator.speed
            };
            graph.tick(&mut play.state, step);
            if play.state.wants_capture() {
                posing.capture_graph(&owned.rig, graph, &mut play.state);
            }
            let count = graph.params().count().min(MAX_GRAPH_PARAMS);
            for (k, value) in animator.params.iter_mut().enumerate().take(count) {
                // A value the editor holds stays the editor's: the game's own comes back
                // when it lets go (E12-5e-2).
                if held.iter().any(|h| h.0 == e && usize::from(h.1) == k) {
                    continue;
                }
                *value = play.state.get(k as u16);
            }
            let name = graph.state_name(&play.state, 0).unwrap_or_default();
            if animator.state != name {
                animator.state.clear();
                animator.state.push_str(name);
            }
            owned.posed = false;
        }
    }

    /// The characters showing a clip of file `file` (an `.anim`, E12-4e) posed again: it
    /// changed.
    fn repose_showing(&mut self, file: AssetId) {
        for owned in self.owned.values_mut() {
            let look = owned.look;
            let own = [look.own.map(|o| o.file), look.own_fade.map(|(o, _)| o.file)];
            if own.contains(&Some(file)) {
                owned.posed = false;
            }
        }
    }

    /// The clips' events of model file `file` as `clips` say (ADR 0124's E12-3b), in place of
    /// its `.meta`'s until the file is read again: what the editor sets as it edits them. Its
    /// characters fire them from the next tick.
    pub fn set_clip_events(&mut self, file: AssetId, clips: &[ClipImport]) {
        let events = Arc::new(ModelEvents::new(clips));
        for owned in self.owned.values_mut().filter(|o| o.file == file) {
            owned.events = Arc::clone(&events);
        }
        // Its graphs cooked again: a clip may play once now, or loop (E12-5c).
        self.graphs.forget(file);
        self.clip_events.insert(file, events);
        self.events_dirty = true;
    }

    /// File `id`'s clips' events (ADR 0124's E12-3b), read from its `.meta` (`file`'s) once.
    fn model_events(&mut self, id: AssetId, file: &GltfFile) -> Arc<ModelEvents> {
        let events = self
            .clip_events
            .entry(id)
            .or_insert_with(|| Arc::new(ModelEvents::new(&file.meta.model.clips)));
        Arc::clone(events)
    }

    /// The skinned models' [`Animator`]s' poses this frame (ADR 0124's E12-2): each one's
    /// clip at the frame's moment `alpha` of the way from the last tick to the next, but
    /// where the editor holds the pose; a model without one rests. The clip's length goes
    /// back to the animator for the ticks to wrap its time at.
    fn animate(&mut self, world: &mut World, alpha: f32) {
        engine_profiler::zone!("app::Assets::animate");
        let (graphs, paths, root) = (&mut self.graphs, &self.paths, &self.root);
        for (&e, owned) in &mut self.owned {
            if owned.held {
                continue;
            }
            // An animation graph in place of a clip (E12-5c): posed from its state each frame.
            if let Some(animator) = world.get::<Animator>(e) {
                let path_of = |id| file_path(paths, AssetRef::file(id)).map(|p| root.join(p));
                match sync_graph(graphs, owned, animator, path_of) {
                    GraphSync::None => {}
                    GraphSync::Plays => {
                        owned.look = Look {
                            graph: Some(alpha),
                            ..Look::default()
                        };
                        owned.posed = false;
                        continue;
                    }
                    GraphSync::Broken => {
                        if owned.look != Look::default() {
                            owned.look = Look::default();
                            owned.posed = false;
                        }
                        continue;
                    }
                }
            }
            let shown = world
                .get::<Animator>(e)
                .and_then(|animator| animator_pose(&owned.rig, animator, alpha, &owned.events));
            // The model's clips' lengths back to the animator, for its ticks to wrap at, and
            // whether each plays once, for them to hold at its end instead (E12-3e).
            if let Some((_, [main, from])) = shown
                && world.get::<Animator>(e).is_some_and(|a| {
                    main.is_some_and(|m| m != (a.length, a.hold))
                        || (a.fade > 0.0 && from.is_some_and(|f| f != (a.from_length, a.from_hold)))
                })
                && let Some(animator) = world.get_mut::<Animator>(e)
            {
                if let Some((length, hold)) = main {
                    animator.length = length;
                    animator.hold = hold;
                }
                if let Some((length, hold)) = from
                    && animator.fade > 0.0
                {
                    animator.from_length = length;
                    animator.from_hold = hold;
                }
            }
            let look = shown.map_or(Look::default(), |(look, _)| look);
            // Its own clips read the first time it shows them (E12-4e; while the world runs
            // their ticks read them first).
            for file in [look.own.map(|o| o.file), look.own_fade.map(|(o, _)| o.file)]
                .into_iter()
                .flatten()
            {
                let (paths, root) = (&self.paths, &self.root);
                self.fields.read_once(file, || {
                    file_path(paths, AssetRef::file(file)).map(|p| root.join(p))
                });
            }
            if owned.look != look {
                owned.look = look;
                owned.posed = false;
            }
        }
    }

    fn sync_skins(&mut self, world: &World, skinning: &mut Skinning, geometry: &mut GeometryPool) {
        self.sync_preview_skin(skinning, geometry);
        if self.owned.is_empty() && self.retired_skins.is_empty() {
            return;
        }
        let frame = self.frame;
        let before = self.owned.len();
        self.owned.retain(|&e, owned| {
            let alive = world.contains(e);
            if !alive {
                for &id in &owned.instances {
                    skinning.remove_instance(id, geometry, frame);
                }
            }
            alive
        });
        self.events_dirty |= self.owned.len() != before;
        let (posing, fields) = (&mut self.posing, &self.fields);
        for owned in self.owned.values_mut().filter(|o| !o.posed) {
            let palette = match (owned.look.graph, &owned.graph) {
                (Some(alpha), Some(play)) => {
                    posing.palette_graph(&owned.rig, &play.cooked.graph, &play.state, alpha)
                }
                _ => {
                    let own = own_clips(fields, &owned.look);
                    posing.palette(&owned.rig, owned.look, own)
                }
            };
            for &id in &owned.instances {
                skinning.set_palette(id, palette);
            }
            owned.posed = true;
        }
        if !self.retired_skins.is_empty() {
            let (owned, preview) = (&self.owned, &self.preview_skin);
            self.retired_skins.retain(|skin| {
                let drawn = owned.values().any(|o| o.skins.contains(skin))
                    || preview.as_ref().is_some_and(|p| p.skins.contains(skin));
                if !drawn {
                    skinning.remove_skin(*skin, geometry, frame);
                }
                drawn
            });
        }
    }

    /// The model the editor's live preview shows, or none (ADR 0124's E12-1b): another one
    /// lets the last one's instance go; the same one asked again changes nothing.
    pub(crate) fn set_preview_skin(
        &mut self,
        preview: Option<(AssetRef, AssetRef)>,
        skinning: &mut Skinning,
        geometry: &mut GeometryPool,
    ) {
        let shown = self.preview_skin.as_ref().map(|p| (p.mesh, p.material));
        if shown == preview {
            return;
        }
        if let Some(p) = self.preview_skin.take() {
            for (id, _) in p.instance.into_iter().chain(p.beside) {
                skinning.remove_instance(id, geometry, self.frame);
            }
        }
        self.preview_skin = preview.map(|(mesh, material)| PreviewSkin {
            mesh,
            material,
            skins: Vec::new(),
            instance: None,
            rig: None,
            pose: None,
            in_place: false,
            posed: false,
            beside: None,
            beside_pose: None,
            beside_posed: false,
        });
    }

    /// The animation graph file the live preview's model plays in place of its clip (ADR
    /// 0124's E12-5d-2d), or none: another one starts anew, at its entry with its own values.
    /// Whether it changed (the preview is drawn again).
    pub(crate) fn set_preview_graph(&mut self, file: Option<AssetId>) -> bool {
        if self.preview_graph.as_ref().map(|g| g.file) == file {
            return false;
        }
        self.preview_graph = file.map(|file| PreviewGraph {
            file,
            play: None,
            held: Vec::new(),
            fired: Vec::new(),
            left: 0.0,
            alpha: 1.0,
        });
        if let Some(p) = self.preview_skin.as_mut() {
            p.posed = false;
        }
        true
    }

    /// Trigger `slot` of the preview's graph fired: set at its next tick, used up by the
    /// transition it lets go (E12-5d-2d-2).
    pub(crate) fn fire_preview_param(&mut self, slot: u16) {
        if let Some(g) = self.preview_graph.as_mut() {
            g.fired.push(slot);
        }
    }

    /// Whether the live preview's model plays a graph now: drawn every frame.
    pub(crate) fn preview_graph_plays(&self) -> bool {
        self.preview_skin.is_some()
            && self
                .preview_graph
                .as_ref()
                .is_some_and(|g| g.play.is_some())
    }

    /// Parameter `slot` of the preview's graph held at `value` (the editor's preview
    /// values; none: back to the graph's own), from its next tick.
    pub(crate) fn hold_preview_param(&mut self, slot: u16, value: Option<f32>) {
        let Some(g) = self.preview_graph.as_mut() else {
            return;
        };
        g.held.retain(|h| h.0 != slot);
        if let Some(value) = value {
            g.held.push((slot, value));
        }
    }

    /// Where machine node `node` of the preview's graph is (as
    /// [`engine_anim::AnimGraph::machine_now_of_node`]), once it plays.
    pub(crate) fn preview_graph_now(&self, node: usize) -> Option<engine_anim::MachineNow<'_>> {
        let play = self.preview_graph.as_ref()?.play.as_ref()?;
        play.cooked.graph.machine_now_of_node(&play.state, node)
    }

    /// The preview's graph `dt` seconds on (ADR 0124's E12-5d-2d): cooked for the preview's
    /// rig once its skin is made (again when the graph file or the rig changes), ticked at
    /// [`PREVIEW_TICK`], posed again each frame. Nothing without a preview or a graph.
    pub(crate) fn tick_preview_graph(&mut self, dt: f32) {
        let (Some(g), Some(p)) = (self.preview_graph.as_mut(), self.preview_skin.as_mut()) else {
            return;
        };
        let Some(rig) = p.rig.as_ref() else {
            return;
        };
        let (graphs, paths, root, posing) =
            (&mut self.graphs, &self.paths, &self.root, &mut self.posing);
        let file = g.file;
        graphs.read(file, || {
            file_path(paths, AssetRef::file(file)).map(|f| root.join(f))
        });
        let default_table = ModelEvents::default();
        let table = self
            .clip_events
            .get(&p.mesh.file)
            .map_or(&default_table, |t| &**t);
        let clip = |name: &str| {
            let clips = rig.clips();
            let k = find_clip(clips.iter().map(crate::RigClip::name), name)?;
            Some(engine_anim::ClipInfo {
                index: u16::try_from(k).ok()?,
                length: clips[k].duration(),
                once: clip_play(rig, table, k).once,
            })
        };
        let motion = |k: u16| rig.motion_mode_of(clip_play(rig, table, usize::from(k)).axes);
        let Some(cooked) = graphs.cook(file, rig, p.mesh.file, clip, motion) else {
            g.play = None;
            return;
        };
        let generation = graphs.generation();
        match &mut g.play {
            Some(play) if Arc::ptr_eq(&play.cooked, &cooked) => play.generation = generation,
            _ => {
                let mut state = engine_anim::GraphState::new(&cooked.graph);
                // Its root motion out of its pose: the preview walks on its spot.
                state.set_in_place(true);
                cooked.graph.tick(&mut state, 0.0);
                g.play = Some(crate::graphs::GraphPlay {
                    file,
                    cooked,
                    state,
                    generation,
                });
                g.left = 0.0;
            }
        }
        let Some(play) = g.play.as_mut() else {
            return;
        };
        let graph = &play.cooked.graph;
        g.left += dt.clamp(0.0, 0.25);
        while g.left >= PREVIEW_TICK {
            g.left -= PREVIEW_TICK;
            for &(slot, value) in &g.held {
                play.state.set(slot, value);
            }
            for slot in g.fired.drain(..) {
                play.state.set(slot, 1.0);
            }
            graph.tick(&mut play.state, PREVIEW_TICK);
            if play.state.wants_capture() {
                posing.capture_graph(rig, graph, &mut play.state);
            }
        }
        g.alpha = g.left / PREVIEW_TICK;
        p.posed = false;
    }

    /// The live preview's model posed at a clip's moment, or in its rest pose: set by the
    /// next sync.
    pub(crate) fn pose_preview(
        &mut self,
        pose: Option<(usize, f32)>,
        in_place: bool,
        beside: Option<(usize, f32)>,
    ) {
        let Some(p) = self.preview_skin.as_mut() else {
            return;
        };
        if p.pose != pose || p.in_place != in_place {
            p.pose = pose;
            p.in_place = in_place;
            p.posed = false;
            p.beside_posed = false;
        }
        if p.beside_pose != beside {
            p.beside_pose = beside;
            p.beside_posed = false;
        }
    }

    /// The mesh of the live preview's second model (ADR 0124's E12-1b-3b), while one is asked
    /// for and once its instance is made and posed.
    pub(crate) fn preview_beside(&self) -> Option<MeshId> {
        let p = self.preview_skin.as_ref()?;
        p.beside_pose?;
        p.beside.filter(|_| p.beside_posed).map(|(_, mesh)| mesh)
    }

    /// The live preview's rig, once its skin is made.
    pub(crate) fn preview_rig(&self) -> Option<&crate::skins::Rig> {
        self.preview_skin.as_ref()?.rig.as_deref()
    }

    /// What the live preview draws, once made: its instance's mesh, its material and its
    /// rig's root.
    pub(crate) fn preview_draw(
        &mut self,
        materials: &mut MaterialTable,
    ) -> Option<(MeshId, MaterialId, Affine3A)> {
        let p = self.preview_skin.as_ref()?;
        let (mesh, material, root) = (p.instance?.1, p.material, p.rig.as_ref()?.root());
        let material = self.material(material, materials)?;
        Some((mesh, material, root))
    }

    /// The live preview's skin each sync: asked for until it is made (as a model's is,
    /// drawn whole), an instance of it made once it is (made anew with the skin, after its
    /// file is read again), its palette set when its pose changed.
    fn sync_preview_skin(&mut self, skinning: &mut Skinning, geometry: &mut GeometryPool) {
        let Some(mesh) = self.preview_skin.as_ref().map(|p| p.mesh) else {
            return;
        };
        let key = (mesh, false);
        if !self.skinned.contains_key(&key) {
            if self.building_skins.contains(&key) || self.problems.contains_key(&mesh) {
                return;
            }
            if let SkinCheck::Skinned(file, index) = self.skin_check(mesh) {
                self.building_skins.insert(key);
                let generation = self.generation(mesh.file);
                self.loader.give(Job::Skin {
                    asset: mesh,
                    split: false,
                    file,
                    mesh: index,
                    generation,
                });
            }
            return;
        }
        let frame = self.frame;
        let (Some(made), Some(p)) = (self.skinned.get(&key), self.preview_skin.as_mut()) else {
            return;
        };
        if p.skins != made.skins {
            for (id, _) in p.instance.take().into_iter().chain(p.beside.take()) {
                skinning.remove_instance(id, geometry, frame);
            }
            p.skins.clone_from(&made.skins);
            p.rig = Some(Arc::clone(&made.rig));
            p.posed = false;
            p.beside_posed = false;
            if let Some(&skin) = p.skins.first() {
                match skinning.add_instance(skin, geometry) {
                    Ok(made) => p.instance = Some(made),
                    Err(why) => log::warn!(target: "engine_app", "the model preview: {why}"),
                }
            }
        }
        // The second model, the first time one is asked for (E12-1b-3b).
        if p.beside_pose.is_some()
            && p.beside.is_none()
            && let Some(&skin) = p.skins.first()
        {
            match skinning.add_instance(skin, geometry) {
                Ok(made) => p.beside = Some(made),
                Err(why) => log::warn!(target: "engine_app", "the model preview's second: {why}"),
            }
        }
        if !p.posed
            && let (Some((id, _)), Some(rig)) = (p.instance, p.rig.as_ref())
        {
            // The graph the editor previews plays in place of the clip (E12-5d-2d).
            let graph = self
                .preview_graph
                .as_ref()
                .and_then(|g| Some((g.play.as_ref()?, g.alpha)));
            let palette = match graph {
                Some((play, alpha)) => {
                    self.posing
                        .palette_graph(rig, &play.cooked.graph, &play.state, alpha)
                }
                None => {
                    let look = Look {
                        in_place: p.in_place,
                        ..Look::at(p.pose)
                    };
                    self.posing.palette(rig, look, [None; 2])
                }
            };
            skinning.set_palette(id, palette);
            p.posed = true;
        }
        if !p.beside_posed
            && p.beside_pose.is_some()
            && let (Some((id, _)), Some(rig)) = (p.beside, p.rig.as_ref())
        {
            let look = Look {
                in_place: p.in_place,
                ..Look::at(p.beside_pose)
            };
            skinning.set_palette(id, self.posing.palette(rig, look, [None; 2]));
            p.beside_posed = true;
        }
    }

    /// The rig of entity `e`'s skinned model (ADR 0124's E12-1a), once its skin is made.
    pub(crate) fn model_rig(&self, e: Entity) -> Option<&crate::skins::Rig> {
        self.owned.get(&e).map(|o| &*o.rig)
    }

    /// Every character's animation graph let go, to be made anew from its animator on the
    /// next frame or tick (ADR 0124's E12-5e): after a run ends and its world is put back
    /// (the editor's Stop), the characters start their graphs again as their animators now
    /// say, not from where the run left them.
    pub(crate) fn restart_graphs(&mut self) {
        for owned in self.owned.values_mut() {
            if owned.graph.take().is_some() {
                owned.posed = false;
            }
        }
        self.held_params.clear();
    }

    /// Parameter `slot` of entity `e`'s animation graph held at `value` for testing (ADR
    /// 0124's E12-5e), over what its animator and the game set, from its next tick until
    /// `None` lets it go or the run ends.
    pub(crate) fn hold_graph_param(&mut self, e: Entity, slot: u16, value: Option<f32>) {
        let at = self
            .held_params
            .iter()
            .position(|&(he, s, _)| he == e && s == slot);
        match (at, value) {
            (Some(k), Some(v)) => self.held_params[k].2 = v,
            (None, Some(v)) => self.held_params.push((e, slot, v)),
            (Some(k), None) => {
                self.held_params.swap_remove(k);
            }
            (None, None) => {}
        }
    }

    /// The parameters of entity `e`'s animation graph as its last tick left them (ADR 0124's
    /// E12-5e): each one's name, kind and value, by slot. None for a character playing no
    /// graph.
    pub(crate) fn graph_params(
        &self,
        e: Entity,
    ) -> Option<impl Iterator<Item = (&str, engine_anim::GraphParamKind, f32)> + '_> {
        let play = self.owned.get(&e)?.graph.as_ref()?;
        Some(
            play.cooked
                .graph
                .params()
                .enumerate()
                .map(move |(k, (name, kind, _))| (name, kind, play.state.get(k as u16))),
        )
    }

    /// Where entity `e`'s animation graph's first layer that plays a machine is now (ADR
    /// 0124's E12-5e): its state, the one it fades from and how far, how far the state's
    /// pose has played. None for a character playing no graph (or not one its rig could
    /// cook), or a graph without a machine.
    pub(crate) fn graph_now(&self, e: Entity) -> Option<engine_anim::MachineNow<'_>> {
        let play = self.owned.get(&e)?.graph.as_ref()?;
        let graph = &play.cooked.graph;
        graph.machine_now(&play.state, graph.machine_layer()?)
    }

    /// [`Self::graph_now`] of layer `layer` (in the layers' order; ADR 0124's E12-5d-2b):
    /// none where it plays no machine.
    pub(crate) fn graph_now_in(
        &self,
        e: Entity,
        layer: usize,
    ) -> Option<engine_anim::MachineNow<'_>> {
        let play = self.owned.get(&e)?.graph.as_ref()?;
        play.cooked.graph.machine_now(&play.state, layer)
    }

    /// [`Self::graph_now`] of the machine whose node is the graph file's `node`th (in its
    /// nodes' ids' order; ADR 0124's E12-5d-2c): a machine inside another's state.
    pub(crate) fn graph_now_node(
        &self,
        e: Entity,
        node: usize,
    ) -> Option<engine_anim::MachineNow<'_>> {
        let play = self.owned.get(&e)?.graph.as_ref()?;
        play.cooked.graph.machine_now_of_node(&play.state, node)
    }

    /// Entity `e`'s skinned model's joints where its pose puts them (ADR 0124's E12-1c), in
    /// the entity's space, into `out` (its room kept). False for a model drawn without a
    /// skin, or before its skin is made.
    pub(crate) fn model_joints(&mut self, e: Entity, out: &mut Vec<Affine3A>) -> bool {
        let Some(owned) = self.owned.get(&e) else {
            return false;
        };
        out.clear();
        out.extend_from_slice(joints_of(&mut self.posing, &self.fields, owned, owned.look));
        true
    }

    /// The entities whose models are drawn skinned (ADR 0124's E12-1c), once their skins are
    /// made, in no order.
    pub(crate) fn skinned_models(&self) -> impl Iterator<Item = Entity> + '_ {
        self.owned.keys().copied()
    }

    /// Holds entity `e`'s skinned model at a clip's moment (ADR 0124's E12-1a), or with
    /// `None` lets it go: back to its [`Animator`]'s pose, its rest pose without one. Set by
    /// the next sync. False for a model drawn without a skin.
    pub(crate) fn pose_model(&mut self, e: Entity, pose: Option<(usize, f32)>) -> bool {
        let Some(owned) = self.owned.get_mut(&e) else {
            return false;
        };
        self.events_dirty |= owned.held != pose.is_some();
        owned.held = pose.is_some();
        let look = Look::at(pose);
        if owned.look != look {
            owned.look = look;
            owned.posed = false;
        }
        true
    }

    /// [`Self::give_renderer`] for a model with material slots (ADR 0115): its mesh made a
    /// mesh per primitive, the first drawn on the entity with [`Model::material`], each
    /// further one by a derived child with its slot's material (the model's own past the
    /// slots). False while a part is still being made.
    fn give_split(
        &mut self,
        world: &mut World,
        e: Entity,
        model: Model,
        slots: &[AssetRef],
        materials: &mut MaterialTable,
    ) -> bool {
        let slot = |i: usize| {
            if i == 0 {
                model.material
            } else {
                slots.get(i - 1).copied().unwrap_or(model.material)
            }
        };
        let (meshes, mut stale) = self.drawn_split(model.mesh);
        let n = meshes.as_ref().map_or(0, Vec::len);
        let mut drawn = Vec::with_capacity(n);
        let mut missing = Vec::new();
        for i in 0..n {
            let (material, s) = self.drawn_material(slot(i), materials);
            stale |= s;
            match material {
                Some(m) => drawn.push(m),
                None => missing.push(slot(i)),
            }
        }
        let Some(meshes) = meshes.filter(|_| missing.is_empty()) else {
            // Not all made: nothing drawn meanwhile, the parts gone with the sweep.
            if world.remove::<FromModel>(e).is_some() {
                world.remove::<MeshRenderer>(e);
            }
            world.remove::<Split>(e);
            let coming = |asset: &AssetRef| !asset.is_none() && !self.problems.contains_key(asset);
            let waits = (n == 0 && coming(&model.mesh)) || missing.iter().any(coming);
            return !waits;
        };
        let visible = u32::from(!model.hidden);
        let first = MeshRenderer {
            mesh: meshes[0],
            material: drawn[0],
            visible,
        };
        if world.get::<MeshRenderer>(e) != Some(&first) {
            world.insert(e, first);
        }
        if world.get::<FromModel>(e).is_none() {
            world.insert(e, FromModel);
            if world.get::<GlobalTransform>(e).is_none() {
                world.insert(e, GlobalTransform::IDENTITY);
            }
        }
        if world.get::<Split>(e).is_none() {
            world.insert(e, Split);
        }
        // The further primitives: a derived child each, kept or made, in the mesh's order.
        let mut have: Vec<(u32, Entity)> = world
            .children(e)
            .filter_map(|c| world.get::<ModelPart>(c).map(|p| (p.0, c)))
            .collect();
        for i in 1..n {
            let renderer = MeshRenderer {
                mesh: meshes[i],
                material: drawn[i],
                visible,
            };
            match have.iter().position(|&(k, _)| k as usize == i) {
                Some(at) => {
                    let (_, part) = have.swap_remove(at);
                    if world.get::<MeshRenderer>(part) != Some(&renderer) {
                        world.insert(part, renderer);
                    }
                }
                None => {
                    let part = world.spawn((
                        Derived,
                        ModelPart(i as u32),
                        Transform::IDENTITY,
                        GlobalTransform::IDENTITY,
                        renderer,
                    ));
                    let _ = world.set_parent(part, Some(e));
                }
            }
        }
        for (_, part) in have {
            world.despawn(part);
        }
        !stale
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A sprite clip's frames and times (ADR 0125's E13-2): forth, backwards, ping-pong
    /// without doubled ends, its own durations, and its last frame from its end on.
    #[test]
    fn sprite_clips_lay_out_their_frames() {
        use engine_scene::{SpriteClipImport, SpriteClipMode};
        let run = SpriteClip::new(&SpriteClipImport {
            name: String::from("run"),
            first: 4,
            last: 6,
            frame_time: 0.1,
            ..SpriteClipImport::default()
        });
        assert!((run.length - 0.3).abs() < 1e-6);
        let at = |c: &SpriteClip, ts: &[f32]| ts.iter().map(|&t| c.frame_at(t)).collect::<Vec<_>>();
        assert_eq!(
            at(&run, &[0.0, 0.05, 0.1, 0.25, 0.31, 9.0]),
            [4, 4, 5, 6, 6, 6]
        );
        let back = SpriteClip::new(&SpriteClipImport {
            first: 2,
            last: 0,
            ..SpriteClipImport::default()
        });
        assert_eq!(at(&back, &[0.0, 0.1, 0.2]), [2, 1, 0]);
        let swing = SpriteClip::new(&SpriteClipImport {
            first: 0,
            last: 3,
            durations: vec![0.5],
            mode: SpriteClipMode::PingPong,
            ..SpriteClipImport::default()
        });
        // 0 1 2 3 2 1: the first for half a second, the rest a tenth each.
        assert!((swing.length - 1.0).abs() < 1e-6);
        assert_eq!(
            at(&swing, &[0.0, 0.45, 0.55, 0.65, 0.75, 0.85, 0.95]),
            [0, 0, 1, 2, 3, 2, 1]
        );
        assert!(!swing.once);
        let once = SpriteClip::new(&SpriteClipImport {
            mode: SpriteClipMode::Once,
            ..SpriteClipImport::default()
        });
        assert!(once.once && (once.length - 0.1).abs() < 1e-6);
    }

    /// ADR 0124's E12-2a: an animator names its clip as the file does, an unnamed one as
    /// the editor shows it; any other name plays the first.
    #[test]
    fn an_animator_finds_its_clip_by_name() {
        let names = ["Walk", "", "Run"];
        let find = |name: &str| named_clip(names.iter().copied(), name);
        assert_eq!(find("Run"), 2);
        assert_eq!(find("Clip 2"), 1, "the unnamed second");
        assert_eq!(find("Clip 3"), 0, "the third has a name of its own");
        assert_eq!(find("Clip 0"), 0);
        assert_eq!(find(""), 1, "the unnamed one by its empty name");
        assert_eq!(find("Fly"), 0, "none: the first");
    }

    #[test]
    fn material_files_round_trip() {
        let red = MaterialFile {
            base_color: [0.8, 0.15, 0.1, 1.0],
            roughness: 0.4,
            alpha: Alpha::Mask,
            ..MaterialFile::default()
        };
        let id = AssetId::from_u128(42);
        let text = red.to_text(id);
        assert!(text.starts_with("[material version=2 id="), "{text}");
        assert!(text.contains("base_color = [0.8, 0.15, 0.1, 1]"), "{text}");
        // The standard material's file holds none of a type's fields (ADR 0120).
        assert!(
            !text.contains("shader") && !text.contains("numbers") && !text.contains("look"),
            "{text}"
        );
        assert!(text.contains("alpha = Mask"), "{text}");
        let (back, report) = MaterialFile::parse(&text).unwrap();
        assert_eq!(back, red);
        assert_eq!(report.id, Some(id));
        assert!(matches!(
            material_desc(&back).alpha,
            AlphaMode::Mask { cutoff } if (cutoff - 0.5).abs() < 1e-6
        ));
    }

    #[test]
    fn builtin_parts_are_distinct_and_named() {
        let assets = Assets::new(PathBuf::new());
        for (i, (name, a)) in builtin::PARTS.iter().enumerate() {
            assert_eq!(
                assets.paths().get(*a),
                Some(format!("builtin#{name}").as_str())
            );
            for (_, b) in &builtin::PARTS[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert!(
            builtin_mesh(builtin::SPHERE).is_some() && builtin_mesh(builtin::MATERIAL).is_none()
        );
    }

    /// A quad of tint `tint` at height `y`, facing up, for baking.
    fn quad(tint: [f32; 4], y: f32) -> crate::BakedBrush {
        crate::BakedBrush {
            tint,
            positions: vec![[0.0, y, 0.0], [0.0, y, 1.0], [1.0, y, 1.0], [1.0, y, 0.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
        }
    }

    #[test]
    fn written_paths_name_files_and_parts() {
        assert_eq!(file_part("scenes/yard.glb#Yard 1"), "scenes/yard.glb");
        assert_eq!(file_part("scenes/yard.glb##0"), "scenes/yard.glb");
        assert_eq!(
            file_part("materials/red.material"),
            "materials/red.material"
        );
        assert!(is_gltf("scenes/yard.glb#Yard 1") && is_gltf("props/Crate.GLTF"));
        assert!(!is_gltf("materials/red.material") && !is_gltf("builtin#cube"));
        // A part known by another part's path of the same file.
        let id = AssetId::from_u128(7);
        let mut paths = AssetPaths::new();
        paths.insert(AssetRef::part(id, "Yard 1"), "scenes/yard.glb#Yard 1");
        assert_eq!(
            file_path(&paths, AssetRef::part(id, "Yard 2")),
            Some("scenes/yard.glb")
        );
        assert_eq!(
            file_path(&paths, AssetRef::part(AssetId::from_u128(8), "Yard 1")),
            None
        );
    }

    #[test]
    fn parts_are_found_by_name_then_by_index() {
        let names = ["Crate", "Barrel", "#0"];
        let find = |part: &str| find_part(AssetRef::part_hash(part), names.iter().copied());
        assert_eq!(find("Barrel"), Some(1));
        assert_eq!(find("#1"), Some(1));
        // A name wins over an index.
        assert_eq!(find("#0"), Some(2));
        assert_eq!(find("#3"), None);
        assert_eq!(find("Lamp"), None);
    }

    #[test]
    fn a_glb_is_read_with_its_meta_and_its_parts() {
        let dir = std::env::temp_dir().join(format!("ofelia-assets-glb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let grey = [0.72, 0.72, 0.72, 1.0];
        let red = [0.8, 0.1, 0.1, 1.0];
        let baked = crate::bake::glb(
            "Yard",
            &[quad(grey, 0.0), quad(red, 1.0)],
            &crate::grid_png(),
        );
        std::fs::write(dir.join("yard.glb"), &baked.bytes).unwrap();
        // No `.meta`: found by its path; `ensure` then gives it one, the same each time.
        let any = AssetId::from_u128(1);
        assert!(open_gltf(&dir, "yard.glb", any).is_ok());
        let id = AssetMeta::ensure(&dir.join("yard.glb")).unwrap();
        assert_eq!(AssetMeta::ensure(&dir.join("yard.glb")).unwrap(), id);
        let text = std::fs::read_to_string(dir.join("yard.glb.meta")).unwrap();
        assert!(
            text.starts_with(&format!("[asset version=1 id={id}]")),
            "{text}"
        );
        let why = open_gltf(&dir, "yard.glb", any).err().unwrap();
        assert!(why.contains("is the asset"), "{why}");
        let file = open_gltf(&dir, "yard.glb", id).unwrap();
        // Its parts, as the bake names them.
        let (mesh, material) = &baked.parts[1];
        let k = mesh_part(&file.scene, AssetRef::part_hash(mesh)).unwrap();
        assert_eq!(
            gltf_mesh(&file.scene.meshes[k], 1.0).positions[0],
            [0.0, 1.0, 0.0]
        );
        // Its import setting: no scale in a `.meta` of defaults; one written scales it.
        assert_eq!(file.meta, AssetMeta::default());
        let meta = AssetMeta {
            model: engine_scene::ModelImport {
                scale: 0.5,
                ..engine_scene::ModelImport::default()
            },
            ..AssetMeta::default()
        };
        meta.write(&dir.join("yard.glb"), id).unwrap();
        let file = open_gltf(&dir, "yard.glb", id).unwrap();
        assert_eq!(file.meta.model.applied_scale(), 0.5);
        assert_eq!(
            gltf_mesh(&file.scene.meshes[k], file.meta.model.applied_scale()).positions[0],
            [0.0, 0.5, 0.0]
        );
        assert!(mesh_part(&file.scene, 0).unwrap_err().contains("2 meshes"));
        assert!(material_part(&file.scene, 0).is_err());
        let k = material_part(&file.scene, AssetRef::part_hash(material)).unwrap();
        let (desc, slots) = gltf_material(id, &file.scene.materials[k], &file.scene.textures);
        assert_eq!(desc.base_color, red);
        assert!((desc.roughness - 0.85).abs() < 1e-6);
        let (key, wrap) = slots[0].unwrap();
        assert_eq!(key, TextureKey::new(id, 0, TextureUsage::Color));
        assert_eq!(key.usage(), TextureUsage::Color);
        assert_eq!(wrap, [Wrap::Repeat; 2]);
        assert!(slots[1..].iter().all(Option::is_none));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_meshs_primitives_are_drawn_as_one() {
        let primitive = |y: f32, uvs: bool| engine_assets::Primitive {
            positions: vec![[0.0, y, 0.0], [0.0, y, 1.0], [1.0, y, 0.0]],
            normals: vec![[0.0, 1.0, 0.0]; 3],
            uvs: if uvs { vec![[0.5, 0.5]; 3] } else { Vec::new() },
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            material: None,
            min: [0.0, y, 0.0],
            max: [1.0, y, 1.0],
            joints: Vec::new(),
            weights: Vec::new(),
        };
        let mesh = MeshData {
            name: String::from("Two"),
            primitives: vec![primitive(0.0, true), primitive(1.0, false)],
        };
        let data = gltf_mesh(&mesh, 1.0);
        assert_eq!(data.positions.len(), 6);
        assert_eq!(data.indices, [0, 1, 2, 3, 4, 5]);
        assert_eq!(data.uvs[0], [0.5, 0.5]);
        assert_eq!(data.uvs[5], [0.0, 0.0]);
        assert!(data.tangents.is_empty());
    }

    /// What a baked level's file costs on the frame's thread the first time its parts are
    /// used: 4,000 boxes (48,000 triangles) in two tints, over the grid. Run with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn measure_a_baked_files_first_use() {
        let mut brushes = Vec::new();
        for i in 0..4000 {
            let (x, z) = ((i % 64) as f32 * 2.0, (i / 64) as f32 * 2.0);
            let tint = if i % 2 == 0 {
                [0.72; 4]
            } else {
                [0.8, 0.1, 0.1, 1.0]
            };
            let mut q = quad(tint, 0.0);
            for p in &mut q.positions {
                p[0] += x;
                p[2] += z;
            }
            // A box's worth of triangles: the quad six times.
            let mut b = q.clone();
            for _ in 1..6 {
                let first = b.positions.len() as u32;
                b.positions.extend_from_slice(&q.positions);
                b.normals.extend_from_slice(&q.normals);
                b.uvs.extend_from_slice(&q.uvs);
                b.indices.extend(q.indices.iter().map(|i| i + first));
            }
            brushes.push(b);
        }
        let png = crate::grid_png();
        let t = std::time::Instant::now();
        let baked = crate::bake::glb("Level", &brushes, &png);
        let write = t.elapsed();
        let t = std::time::Instant::now();
        let scene = engine_assets::import_gltf(&baked.bytes, None).unwrap();
        let import = t.elapsed();
        let t = std::time::Instant::now();
        let meshes: Vec<OwnedMeshData> = scene.meshes.iter().map(|m| gltf_mesh(m, 1.0)).collect();
        let merge = t.elapsed();
        let t = std::time::Instant::now();
        let image = engine_assets::decode_image(&png).unwrap();
        let levels = engine_assets::build_mips(&image, TextureUsage::Color);
        let texture = t.elapsed();
        let triangles: usize = meshes.iter().map(|m| m.indices.len() / 3).sum();
        println!(
            "{} KB, {triangles} triangles: written {write:?}, imported {import:?}, merged {merge:?}, grid {}x{} decoded with {} levels {texture:?}",
            baked.bytes.len() / 1024,
            levels.width,
            levels.height,
            levels.levels.len()
        );
    }

    #[test]
    fn scenes_hold_models_not_renderers() {
        let registry = engine_scene::SceneRegistry::from_program();
        assert!(registry.contains("Model"));
        assert!(
            !registry.contains("MeshRenderer"),
            "runtime ids stay out of scenes"
        );
        assert!(registry.contains("Camera") && registry.contains("Sun"));
    }
}
