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

use crate::loading::{Done, Job, Loader, Made};
use engine_assets::colliders::{ColliderRules, Motion, cook_node_colliders};
use engine_assets::material_file::{TextureEncoding, TypedMaterial};
use engine_assets::{
    DerivedDataCache, EncodedTexture, ImportedScene, MaterialData, MeshData, TextureData,
    TextureImage, TextureSource, TextureUsage, TextureWrap,
};
use engine_collision::{Collider, ColliderShape, Origin, Shape};
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_ecs::{Changed, Entity, GlobalTransform, QueryState, Transform, With, Without, World};
use engine_reflect::Reflect;
use engine_render::forward::{ForwardRenderer, MaterialTypeDesc};
use engine_render::material::{
    AlphaMode, MaterialDesc, MaterialId, MaterialParams, MaterialTable, MaterialTypeId,
    PARAMS_BYTES, TextureRef,
};
use engine_render::mesh::{GeometryPool, MeshId, OwnedMeshData};
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_rhi::{Allocator, Bindless, Device, Wrap};
use engine_runtime::{Derived, MeshRenderer};
use engine_scene::{AssetId, AssetMeta, AssetPaths, AssetRef};
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
    /// Models' colliders (ADR 0117): each glTF file's once cooked (or why not), the files
    /// being cooked, the built-in meshes' as made, and how many times any was made or
    /// forgotten.
    colliders: HashMap<AssetId, Result<CookedColliders, String>>,
    cooking: HashSet<AssetId>,
    /// Of those, the files being read first: cooked once they are.
    read_to_cook: HashSet<AssetId>,
    builtin_colliders: HashMap<AssetRef, Option<Arc<ModelCollider>>>,
    collider_epoch: u64,
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
            colliders: HashMap::new(),
            cooking: HashSet::new(),
            read_to_cook: HashSet::new(),
            builtin_colliders: HashMap::new(),
            collider_epoch: 0,
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
            self.read_material(asset)
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
    fn take_done(&mut self, geometry: &mut GeometryPool, wait: bool) -> bool {
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
                    // Not in the cache: compressed in the background, swapped in once done.
                    if let (Some(source), Some(cache)) = (compress, &self.cache) {
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

    /// Material file `asset` read, and the paths its references were written with.
    fn read_material(&self, asset: AssetRef) -> Result<(MaterialFile, AssetPaths), String> {
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
        let (file, report) = MaterialFile::parse(&text).map_err(|e| format!("{path}:{e}"))?;
        for (line, warning) in &report.warnings {
            log::warn!(target: "engine_app::assets", "{path}:{line}: {warning}");
        }
        match report.id {
            Some(id) if id != asset.file => Err(format!(
                "{path} holds the material {id}, not {}",
                asset.file
            )),
            _ => Ok((file, report.assets)),
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
        let mut images = Vec::new();
        for texture in file.textures.values() {
            let Some(at) = written.get(texture.image).filter(|p| !p.is_empty()) else {
                continue;
            };
            let full = if Path::new(at).is_absolute() {
                PathBuf::from(at)
            } else {
                self.root.join(at)
            };
            let usage = match texture.encoding {
                TextureEncoding::Color => TextureUsage::Color,
                TextureEncoding::Data => TextureUsage::Data,
                TextureEncoding::Normal => TextureUsage::Normal,
            };
            let key = TextureKey::new(texture.image.file, 0, usage);
            if let Some(under) = project_path(&self.root, &full) {
                let users = self.depends.entry(under).or_default();
                if !users.contains(&asset.file) {
                    users.push(asset.file);
                }
            }
            images.push((full, key));
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
        })
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
            typed,
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
                forward.register_material_type(MaterialTypeDesc {
                    name: t.file.type_name.clone(),
                    module: t.file.shader.clone(),
                    type_name: t.file.type_name.clone(),
                })
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
    pub(crate) fn sync(
        &mut self,
        world: &mut World,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        frame: u64,
        wait: bool,
        extracted: bool,
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
        let mut came = self.take_done(geometry, false);
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
            if self.give_renderer(world, e, model, geometry, materials) {
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
                        && !self.give_renderer(world, e, model, geometry, materials)
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
            came = self.take_done(geometry, true);
        }
        let orphans = self.orphans.get_or_insert_with(|| QueryState::new(world));
        self.gone.clear();
        self.gone.extend(orphans.iter_readonly(world));
        for &e in &self.gone {
            world.remove::<FromModel>(e);
            world.remove::<MeshRenderer>(e);
            world.remove::<Split>(e);
            self.waiting.remove(&e);
        }
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
    ) -> bool {
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
            model: engine_scene::ModelImport { scale: 0.5 },
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
