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
//!   emissive) are decoded on first use and kept uncompressed (RGBA8 with their levels);
//!   textures the cooker compresses come with the asset browser (ADR 0107 E4). Shading
//!   models from extensions (unlit, clearcoat, sheen) are plain PBR here.
//! - **The file's id** is its `.meta`'s ([`AssetMeta`]); a file without one is found by
//!   its path alone.

use engine_assets::{
    ImportedScene, MaterialData, MeshData, TextureData, TextureUsage, TextureWrap, build_mips,
    decode_image, read_texture,
};
use engine_ecs::{Changed, Entity, GlobalTransform, QueryState, With, Without, World};
use engine_reflect::Reflect;
use engine_render::material::{AlphaMode, MaterialDesc, MaterialId, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryPool, MeshId, OwnedMeshData};
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_rhi::{Allocator, Bindless, Device, Wrap};
use engine_runtime::MeshRenderer;
use engine_scene::{AssetId, AssetMeta, AssetPaths, AssetRef, LoadError, ValueReport};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// How a material uses alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum Alpha {
    /// Alpha is ignored.
    #[default]
    Opaque,
    /// Fragments below [`MaterialFile::alpha_cutoff`] are cut away (foliage, fences).
    Mask,
    /// Blended over what is behind.
    Blend,
}

/// A material file (`.material`, ADR 0108): `[material version=1 id=…]`, then its values,
/// one per line.
///
/// ```text
/// [material version=1 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]
///
/// base_color = [0.8, 0.15, 0.1, 1]
/// metallic = 0
/// roughness = 0.4
/// emissive = [0, 0, 0]
/// alpha = Opaque
/// alpha_cutoff = 0.5
/// double_sided = false
/// ```
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct MaterialFile {
    /// Linear base color and alpha.
    #[reflect(hint = "color")]
    pub base_color: [f32; 4],
    /// 0 dielectric … 1 metal.
    #[reflect(min = 0.0, max = 1.0)]
    pub metallic: f32,
    /// Perceptual roughness, 0 (mirror) … 1.
    #[reflect(min = 0.0, max = 1.0)]
    pub roughness: f32,
    /// Emitted radiance, linear (above 1 for bright emitters).
    #[reflect(hint = "color")]
    pub emissive: [f32; 3],
    /// How alpha is used.
    pub alpha: Alpha,
    /// With [`Alpha::Mask`], alpha below this is cut away.
    #[reflect(min = 0.0, max = 1.0)]
    pub alpha_cutoff: f32,
    /// Both faces drawn and lit.
    pub double_sided: bool,
}

impl Default for MaterialFile {
    /// The built-in material: grey (0.8), dielectric, roughness 0.5, opaque.
    fn default() -> Self {
        Self {
            base_color: [0.8, 0.8, 0.8, 1.0],
            metallic: 0.0,
            roughness: 0.5,
            emissive: [0.0; 3],
            alpha: Alpha::Opaque,
            alpha_cutoff: 0.5,
            double_sided: false,
        }
    }
}

impl MaterialFile {
    /// The header's kind.
    pub const KIND: &'static str = "material";
    /// The format's version.
    pub const VERSION: u32 = 1;

    /// The renderer's description.
    pub fn desc(&self) -> MaterialDesc {
        MaterialDesc {
            base_color: self.base_color,
            metallic: self.metallic,
            roughness: self.roughness,
            emissive: self.emissive,
            alpha: match self.alpha {
                Alpha::Opaque => AlphaMode::Opaque,
                Alpha::Mask => AlphaMode::Mask {
                    cutoff: self.alpha_cutoff,
                },
                Alpha::Blend => AlphaMode::Blend,
            },
            double_sided: self.double_sided,
            ..MaterialDesc::default()
        }
    }

    /// The file's text, with its id.
    pub fn to_text(&self, id: AssetId) -> String {
        let mut out = String::new();
        engine_scene::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut out);
        out
    }

    /// A material from its file's text.
    ///
    /// # Errors
    ///
    /// Not a material file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = engine_scene::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
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

/// A glTF file read for its parts: what the importer made of it, and the folder its
/// external files are in.
struct GltfFile {
    scene: ImportedScene,
    dir: PathBuf,
}

/// glTF file `id` at `path` under `root`, its id checked against its `.meta` when it has
/// one.
fn open_gltf(root: &Path, path: &str, id: AssetId) -> Result<GltfFile, String> {
    let full = root.join(path);
    let meta = AssetMeta::path(&full);
    match std::fs::read_to_string(&meta) {
        Ok(text) => {
            let (_, report) = AssetMeta::parse(&text).map_err(|e| format!("{path}.meta:{e}"))?;
            if let Some(meta_id) = report.id
                && meta_id != id
            {
                return Err(format!(
                    "{path} is the asset {meta_id} (its .meta), not {id}"
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", meta.display())),
    }
    let bytes = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
    let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
    let scene =
        engine_assets::import_gltf(&bytes, Some(&dir)).map_err(|e| format!("{path}: {e}"))?;
    Ok(GltfFile { scene, dir })
}

/// glTF file `id` at `path`, read the first time a frame asks for it.
fn gltf_file<'a>(
    files: &'a mut HashMap<AssetId, Result<GltfFile, String>>,
    root: &Path,
    path: &str,
    id: AssetId,
) -> Result<&'a GltfFile, String> {
    files
        .entry(id)
        .or_insert_with(|| open_gltf(root, path, id))
        .as_ref()
        .map_err(Clone::clone)
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

/// A glTF mesh as one mesh: its primitives one after another. Texture coordinates and
/// tangents are kept when any primitive has them (zero coordinates and +X tangents for
/// those without).
fn gltf_mesh(mesh: &MeshData) -> OwnedMeshData {
    let mut data = OwnedMeshData::default();
    let uvs = mesh.primitives.iter().any(|p| !p.uvs.is_empty());
    let tangents = mesh.primitives.iter().any(|p| !p.tangents.is_empty());
    for p in &mesh.primitives {
        let (first, n) = (data.positions.len() as u32, p.positions.len());
        data.positions.extend_from_slice(&p.positions);
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
struct TextureKey {
    file: AssetId,
    texture: usize,
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

    fn usage(self) -> TextureUsage {
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

/// A texture to make once the device is at hand: its encoded image, and the file it is
/// from (for what to say when it cannot be made).
struct PendingTexture {
    key: TextureKey,
    bytes: Vec<u8>,
    path: String,
}

/// A glTF material made without its textures, given them once they are made.
struct PendingMaterial {
    material: MaterialId,
    desc: MaterialDesc,
    slots: Slots,
}

type Changes = QueryState<(Entity, &'static Model), Changed<Model>>;
type Orphans = QueryState<Entity, (With<FromModel>, Without<Model>)>;

/// The assets the player's scenes use, made on first use (ADR 0108): built-in meshes and
/// the built-in material, material files and glTF files' meshes and materials under the
/// game's folder, found by the paths loaded scenes give.
pub struct Assets {
    root: PathBuf,
    paths: AssetPaths,
    meshes: HashMap<AssetRef, MeshId>,
    materials: HashMap<AssetRef, MaterialId>,
    /// glTF files read for their parts in this frame, by id: dropped at the end of
    /// [`Self::sync`], the parts made from them kept.
    files: HashMap<AssetId, Result<GltfFile, String>>,
    /// glTF materials' textures, made or (`None`) failed.
    textures: HashMap<TextureKey, Option<TextureId>>,
    /// Textures to make where the device is at hand ([`Self::make_textures`]).
    pending_textures: Vec<PendingTexture>,
    /// glTF materials waiting for their textures.
    pending_materials: Vec<PendingMaterial>,
    /// Parts of files read again ([`Self::reload`]), freed in the next frame: meshes and
    /// materials in [`Self::sync`], textures in [`Self::make_textures`].
    retired_meshes: Vec<MeshId>,
    retired_materials: Vec<MaterialId>,
    retired_textures: Vec<TextureId>,
    /// Why an asset could not be made, said once.
    problems: HashMap<AssetRef, String>,
    changes: Option<Changes>,
    orphans: Option<Orphans>,
    changed: Vec<(Entity, Model)>,
    gone: Vec<Entity>,
}

impl Assets {
    /// Assets under `root`, the folder relative asset paths start from.
    pub fn new(root: PathBuf) -> Self {
        let mut paths = AssetPaths::new();
        for (name, asset) in builtin::PARTS {
            paths.insert(asset, format!("{}#{name}", builtin::PATH));
        }
        Self {
            root,
            paths,
            meshes: HashMap::new(),
            materials: HashMap::new(),
            files: HashMap::new(),
            textures: HashMap::new(),
            pending_textures: Vec::new(),
            pending_materials: Vec::new(),
            retired_meshes: Vec::new(),
            retired_materials: Vec::new(),
            retired_textures: Vec::new(),
            problems: HashMap::new(),
            changes: None,
            orphans: None,
            changed: Vec::new(),
            gone: Vec::new(),
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

    /// Reads file `file` again on its next use: its meshes, materials and textures made
    /// so far are forgotten (and freed in the next frame), what could not be made is tried
    /// again, and every model in `world` that uses the file is made again. What the editor
    /// does after writing the file (a bake over an earlier one).
    pub fn reload(&mut self, world: &mut World, file: AssetId) {
        self.meshes.retain(|asset, &mut mesh| {
            let keep = asset.file != file;
            if !keep {
                self.retired_meshes.push(mesh);
            }
            keep
        });
        self.materials.retain(|asset, &mut material| {
            let keep = asset.file != file;
            if !keep {
                self.retired_materials.push(material);
            }
            keep
        });
        self.textures.retain(|key, made| {
            let keep = key.file != file;
            if !keep && let Some(texture) = made {
                self.retired_textures.push(*texture);
            }
            keep
        });
        self.pending_textures.retain(|t| t.key.file != file);
        self.problems.retain(|asset, _| asset.file != file);
        self.files.remove(&file);
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
        let data = match builtin_mesh(asset) {
            Some(data) => Ok(data),
            None => match file_path(&self.paths, asset) {
                Some(path) if is_gltf(path) => {
                    gltf_file(&mut self.files, &self.root, path, asset.file).and_then(|file| {
                        let k = mesh_part(&file.scene, asset.part)?;
                        Ok(gltf_mesh(&file.scene.meshes[k]))
                    })
                }
                Some(path) => Err(format!(
                    "{path} is not a mesh: meshes are built in or in glTF files (.glb, .gltf)"
                )),
                None => Err(String::from("no path known for it")),
            },
        };
        let data = match data {
            Ok(data) => data,
            Err(why) => {
                self.fail(asset, why);
                return None;
            }
        };
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
                Ok(material) => Some(material),
                Err(why) => {
                    self.fail(asset, why);
                    None
                }
            };
        }
        let file = if asset == builtin::MATERIAL {
            Ok(MaterialFile::default())
        } else {
            self.read_material(asset)
        };
        match file {
            Ok(file) => {
                let material = table.add(file.desc());
                self.materials.insert(asset, material);
                Some(material)
            }
            Err(why) => {
                self.fail(asset, why);
                None
            }
        }
    }

    /// A glTF file's material, added without its textures: their encoded images are read
    /// now, the textures made after the frame's stages.
    fn gltf_material(
        &mut self,
        asset: AssetRef,
        table: &mut MaterialTable,
    ) -> Result<MaterialId, String> {
        let path = file_path(&self.paths, asset).ok_or("no path known for it")?;
        let file = gltf_file(&mut self.files, &self.root, path, asset.file)?;
        let k = material_part(&file.scene, asset.part)?;
        let (desc, slots) =
            gltf_material(asset.file, &file.scene.materials[k], &file.scene.textures);
        for (key, _) in slots.iter().flatten() {
            if self.textures.contains_key(key)
                || self.pending_textures.iter().any(|t| t.key == *key)
            {
                continue;
            }
            let source = &file.scene.textures[key.texture].source;
            match read_texture(source, Some(&file.dir)) {
                Ok(bytes) => self.pending_textures.push(PendingTexture {
                    key: *key,
                    bytes,
                    path: path.to_owned(),
                }),
                Err(e) => {
                    log::warn!(target: "engine_app::assets", "{path}: texture {}: {e}", key.texture);
                    self.textures.insert(*key, None);
                }
            }
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
        Ok(material)
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
        if self.pending_materials.is_empty() && self.pending_textures.is_empty() {
            return;
        }
        engine_profiler::zone!("assets::make_textures");
        for pending in self.pending_textures.drain(..) {
            let made = decode_image(&pending.bytes)
                .map_err(|e| e.to_string())
                .and_then(|image| {
                    let t = build_mips(&image, pending.key.usage());
                    let levels = TextureLevels {
                        width: t.width,
                        height: t.height,
                        format: if t.srgb {
                            TextureFormat::Rgba8Srgb
                        } else {
                            TextureFormat::Rgba8Unorm
                        },
                        levels: t.levels,
                    };
                    textures
                        .add(device, allocator, levels)
                        .map_err(|e| e.to_string())
                });
            let made = match made {
                Ok(id) => Some(id),
                Err(why) => {
                    let (path, texture) = (pending.path, pending.key.texture);
                    log::warn!(target: "engine_app::assets", "{path}: texture {texture}: {why}");
                    None
                }
            };
            self.textures.insert(pending.key, made);
        }
        for pending in self.pending_materials.drain(..) {
            let made = |slot: Option<(TextureKey, [Wrap; 2])>| {
                let (key, wrap) = slot?;
                let texture = (*self.textures.get(&key)?)?;
                Some(TextureRef { texture, wrap })
            };
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
    }

    fn read_material(&self, asset: AssetRef) -> Result<MaterialFile, String> {
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
            _ => Ok(file),
        }
    }

    /// Gives every entity whose [`Model`] changed a `MeshRenderer` of its mesh and material,
    /// and takes it from those whose model went. Runs in the player's frame after the
    /// game's ticks and presentation, before extraction; new meshes and materials reach the
    /// GPU in the same frame. Visits only chunks where models were written.
    pub(crate) fn sync(
        &mut self,
        world: &mut World,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        frame: u64,
    ) {
        for mesh in self.retired_meshes.drain(..) {
            geometry.remove_mesh(mesh, frame);
        }
        for material in self.retired_materials.drain(..) {
            materials.remove(material);
        }
        let changes = self.changes.get_or_insert_with(|| QueryState::new(world));
        self.changed.clear();
        self.changed
            .extend(changes.iter_readonly(world).map(|(e, model)| (e, *model)));
        for i in 0..self.changed.len() {
            let (e, model) = self.changed[i];
            let mesh = self.mesh(model.mesh, geometry);
            let material = self.material(model.material, materials);
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
                }
                _ => {
                    if world.remove::<FromModel>(e).is_some() {
                        world.remove::<MeshRenderer>(e);
                    }
                }
            }
        }
        let orphans = self.orphans.get_or_insert_with(|| QueryState::new(world));
        self.gone.clear();
        self.gone.extend(orphans.iter_readonly(world));
        for &e in &self.gone {
            world.remove::<FromModel>(e);
            world.remove::<MeshRenderer>(e);
        }
        // The glTF files read this frame: their parts are made.
        self.files.clear();
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
        assert!(text.starts_with("[material version=1 id="), "{text}");
        assert!(text.contains("base_color = [0.8, 0.15, 0.1, 1]"), "{text}");
        assert!(text.contains("alpha = Mask"), "{text}");
        let (back, report) = MaterialFile::parse(&text).unwrap();
        assert_eq!(back, red);
        assert_eq!(report.id, Some(id));
        assert!(matches!(
            back.desc().alpha,
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
            gltf_mesh(&file.scene.meshes[k]).positions[0],
            [0.0, 1.0, 0.0]
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
        let data = gltf_mesh(&mesh);
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
        let meshes: Vec<OwnedMeshData> = scene.meshes.iter().map(gltf_mesh).collect();
        let merge = t.elapsed();
        let t = std::time::Instant::now();
        let image = decode_image(&png).unwrap();
        let levels = build_mips(&image, TextureUsage::Color);
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
