//! Assets scenes refer to (ADR 0108): the engine's built-in meshes and material, and
//! material files in the game's folder, made once on first use. An entity names them with a
//! [`Model`]; the player keeps its `MeshRenderer` in step.
//!
//! ```text
//! [entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
//! Model.mesh = asset("builtin#cube", 6f66656c6961206275696c74696e0001)
//! Model.material = asset("materials/red.material", 4f8e1d2c3b4a59687706f5e4d3c2b1a0)
//! Model.hidden = false
//! ```

use engine_ecs::{Changed, Entity, GlobalTransform, QueryState, With, Without, World};
use engine_reflect::Reflect;
use engine_render::material::{AlphaMode, MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryPool, MeshId, OwnedMeshData};
use engine_runtime::MeshRenderer;
use engine_scene::{AssetId, AssetPaths, AssetRef, LoadError, ValueReport};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A mesh drawn with a material, both by asset (ADR 0108): what scenes and the editor hold.
/// The player gives the entity a `MeshRenderer` with the mesh and material made from the
/// assets, and keeps it in step: an asset it cannot find leaves the entity undrawn and is
/// reported once in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub struct Model {
    /// The mesh: a built-in one ([`builtin`]).
    pub mesh: AssetRef,
    /// The material: the built-in one or a material file ([`MaterialFile`]).
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

type Changes = QueryState<(Entity, &'static Model), Changed<Model>>;
type Orphans = QueryState<Entity, (With<FromModel>, Without<Model>)>;

/// The assets the player's scenes use, made on first use (ADR 0108): built-in meshes and
/// the built-in material, and material files under the game's folder, found by the paths
/// loaded scenes give.
pub struct Assets {
    root: PathBuf,
    paths: AssetPaths,
    meshes: HashMap<AssetRef, MeshId>,
    materials: HashMap<AssetRef, MaterialId>,
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

    fn fail(&mut self, asset: AssetRef, why: String) {
        let path = self.paths.get(asset).unwrap_or("");
        log::warn!(target: "engine_app::assets", "asset {path} ({}): {why}", asset.file);
        self.problems.insert(asset, why);
    }

    /// The mesh of a built-in mesh asset, made on first use.
    pub fn mesh(&mut self, asset: AssetRef, geometry: &mut GeometryPool) -> Option<MeshId> {
        if let Some(&mesh) = self.meshes.get(&asset) {
            return Some(mesh);
        }
        if asset.is_none() || self.problems.contains_key(&asset) {
            return None;
        }
        let Some(data) = builtin_mesh(asset) else {
            self.fail(
                asset,
                String::from("not a built-in mesh; meshes from files come with the asset browser (ADR 0107, E4)"),
            );
            return None;
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

    /// The material of the built-in material or a material file, made on first use.
    pub fn material(&mut self, asset: AssetRef, table: &mut MaterialTable) -> Option<MaterialId> {
        if let Some(&material) = self.materials.get(&asset) {
            return Some(material);
        }
        if asset.is_none() || self.problems.contains_key(&asset) {
            return None;
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
    ) {
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
