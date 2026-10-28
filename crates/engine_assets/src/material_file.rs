//! Material files (ADR 0092, ADR 0120): `.material`, in the editor's grammar (ADR 0108),
//! its id in its header. With no shader it is the engine's standard material; with one, a
//! material type's instance.
//!
//! ```text
//! [material version=2 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]
//!
//! shader = asset("materials/velvet.slang", 7d1c0e9a2b3f4c5d8e6f7a8b9c0d1e2f)
//! type = "VelvetMaterial"
//! numbers["sheen_roughness"] = 0.6
//! vectors["color"] = [0.4, 0.05, 0.05, 1]
//! vectors["sheen_color"] = [0.9, 0.6, 0.6]
//! textures["pattern"].image = asset("textures/velvet.png", 0a1b2c3d4e5f60718293a4b5c6d7e8f9)
//! textures["pattern"].encoding = Color
//! look = Own
//! base_color = [0.4, 0.05, 0.05, 1]
//! roughness = 0.9
//! alpha = Mask
//! alpha_cutoff = 0.5
//! double_sided = true
//! ```
//!
//! - **The type's parameters** by their `Params` field's name (`outer.inner` for a nested
//!   struct's field, `items[2].inner` for an array of structs'): `numbers` for a field of one
//!   number, `vectors` for a vector's values or an array's in order (an array of vectors
//!   written flat), `textures` for a `TextureRef` field's image and encoding.
//! - **The look** (`look`): what a typed material looks like before its shaders are made and
//!   in GI's rays: its own standard values (`Own`), or the glTF material it replaces in a
//!   scene's map (`Replaced`).
//! - **Paths are asset references** from the project's root ([`MaterialFile::typed`] is given
//!   it); an absolute path is used as it is.
//! - [`TypedMaterial::build_params`] lays the values out as the type's `Params` struct (from
//!   Slang's reflection, `engine_shader::StructLayout`). A field the file does not set is zero
//!   (a texture none: white) and listed in [`BuiltParams::unset`]; a name the struct does not
//!   have, or a value of the wrong shape, is an error naming it.
//!
//! A scene's glTF materials are replaced by material files through a file next to it,
//! `<scene>.materials.ron` ([`MaterialMap`]): `{ "glTF material name": "file.material" }`;
//! `"#3"` names the scene's fourth material (glTF materials need not have names).

use crate::gltf_import::{
    AlphaMode, ImportedScene, MaterialTypeData, TextureData, TextureSource, TextureWrap,
    TypedMaterialData,
};
use crate::pso_record::PsoRecord;
use engine_reflect::Reflect;
use engine_scene::{AssetId, AssetPaths, AssetRef, LoadError, ValueReport};
use engine_shader::{FieldKind, StructLayout};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The extension of material files.
pub const MATERIAL_FILE_EXTENSION: &str = "material";

/// How a texture parameter's image is encoded (the texture pipeline's usage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Reflect)]
#[repr(u8)]
pub enum TextureEncoding {
    /// sRGB color (BC7).
    #[default]
    Color,
    /// Linear data, one value per channel (BC7).
    Data,
    /// A tangent-space normal map (BC5).
    Normal,
}

/// A parameter's value, resolved.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    /// An image for a `TextureRef` field.
    Texture(PathBuf, TextureEncoding),
    /// A number.
    Number(f64),
    /// A vector's or an array's values.
    List(Vec<ParamValue>),
}

/// How a material uses alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum Alpha {
    /// Alpha is ignored.
    #[default]
    Opaque,
    /// Fragments below the file's `alpha_cutoff` are cut away (foliage, fences).
    Mask,
    /// Blended over what is behind.
    Blend,
}

/// Whose look a typed material has before its shaders are made, and in GI's rays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum Look {
    /// Its own standard values (`base_color`, `metallic`, `roughness`, `emissive`).
    #[default]
    Own,
    /// The glTF material it replaces in a scene's map; its own values where it replaces
    /// none.
    Replaced,
}

/// A texture parameter of a material file: its image and how it is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub struct MaterialTexture {
    /// The image, by asset.
    pub image: AssetRef,
    /// How the cook compresses it.
    pub encoding: TextureEncoding,
}

/// The glTF look of a material of a type: its first frames (before its shaders are made)
/// and what GI's rays see (ADR 0092).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fallback {
    /// Linear base color and alpha.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Roughness factor.
    pub roughness: f32,
    /// Linear emitted light.
    pub emissive: [f32; 3],
}

impl Default for Fallback {
    /// The engine's default material: grey (0.8), dielectric, roughness 0.5.
    fn default() -> Self {
        Self {
            base_color: [0.8, 0.8, 0.8, 1.0],
            metallic: 0.0,
            roughness: 0.5,
            emissive: [0.0; 3],
        }
    }
}

/// A material file (`.material`, see the module documentation): the engine's standard
/// material, or with a shader a material type's instance.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct MaterialFile {
    /// The Slang module defining its type; none: the standard material.
    pub shader: AssetRef,
    /// The type's name in it.
    #[reflect(rename = "type")]
    pub type_name: String,
    /// Its type's fields of one number, by name.
    pub numbers: BTreeMap<String, f32>,
    /// Its type's vectors and arrays, by name: the values in order.
    pub vectors: BTreeMap<String, Vec<f32>>,
    /// Its type's textures, by name.
    pub textures: BTreeMap<String, MaterialTexture>,
    /// A typed material's look before its shaders are made, and in GI's rays.
    pub look: Look,
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
    /// The standard material: grey (0.8), dielectric, roughness 0.5, opaque.
    fn default() -> Self {
        Self {
            shader: AssetRef::NONE,
            type_name: String::new(),
            numbers: BTreeMap::new(),
            vectors: BTreeMap::new(),
            textures: BTreeMap::new(),
            look: Look::Own,
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

/// A typed material file resolved: what the cook and the run time build its parameters from.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedMaterial {
    /// The Slang module defining the type.
    pub shader: PathBuf,
    /// The type's name in it.
    pub type_name: String,
    /// Its parameters by field name.
    pub params: BTreeMap<String, ParamValue>,
    /// Its texture parameters' images as assets, by field name: the file's, or a graph's
    /// own (ADR 0123) where the file gives none.
    pub textures: BTreeMap<String, AssetRef>,
    /// Its glTF look; `None`: the replaced glTF material's, or [`Fallback::default`].
    pub fallback: Option<Fallback>,
    /// Alpha.
    pub alpha: Alpha,
    /// With [`Alpha::Mask`], alpha below this is cut away.
    pub alpha_cutoff: f32,
    /// Drawn without back-face culling.
    pub double_sided: bool,
}

/// An error in a material file or a material map: the file, and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialFileError {
    /// The file, when known.
    pub path: Option<PathBuf>,
    /// What is wrong (with the line and column for syntax errors).
    pub message: String,
}

impl fmt::Display for MaterialFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for MaterialFileError {}

/// A texture parameter: where its bindless reference goes, and its image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureParam {
    /// Byte offset in the parameters.
    pub offset: u32,
    /// The image (resolved like the file's other paths).
    pub path: PathBuf,
    /// How it is encoded.
    pub encoding: TextureEncoding,
}

/// A material file's parameters laid out as its type's `Params` struct.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BuiltParams {
    /// The struct's bytes; texture fields hold `u32::MAX` (none) until the material
    /// table writes their bindless references.
    pub bytes: Vec<u8>,
    /// The texture fields the file sets.
    pub textures: Vec<TextureParam>,
    /// Fields the file does not set (zero; textures none).
    pub unset: Vec<String>,
}

/// What `material_params` reads for a texture field without a texture: `NO_TEXTURE`, white.
const NO_TEXTURE: u32 = u32::MAX;

fn resolve(dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        dir.join(path)
    }
}

/// RON as material maps are written: optional fields without `Some(...)`.
fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
}

fn ron_error(path: Option<&Path>, e: &ron::error::SpannedError) -> MaterialFileError {
    MaterialFileError {
        path: path.map(Path::to_owned),
        message: format!("{}:{}: {}", e.span.start.line, e.span.start.col, e.code),
    }
}

impl MaterialFile {
    /// The header's kind.
    pub const KIND: &'static str = "material";
    /// The format's version (version 1: the standard material's values alone).
    pub const VERSION: u32 = 2;

    /// Whether it is a material type's instance (a shader and a type), not the standard
    /// material.
    pub fn is_typed(&self) -> bool {
        self.shader != AssetRef::NONE && !self.type_name.is_empty()
    }

    /// The file's text, with its id; asset references written with no path.
    pub fn to_text(&self, id: AssetId) -> String {
        self.text(id, None)
    }

    /// The file's text, with its id, its asset references written with their paths from
    /// `assets`.
    pub fn to_text_with(&self, id: AssetId, assets: &AssetPaths) -> String {
        self.text(id, Some(assets))
    }

    /// The text: every field, but the type's when they hold nothing (a standard material's
    /// file reads as version 1's did; a missing field reads as its default).
    fn text(&self, id: AssetId, assets: Option<&AssetPaths>) -> String {
        let mut all = String::new();
        engine_scene::write_value(Self::KIND, Self::VERSION, Some(id), self, assets, &mut all);
        let typed = self.is_typed();
        let mut out = String::with_capacity(all.len());
        for line in all.lines() {
            let empty = matches!(
                line,
                "shader = none" | "type = \"\"" | "numbers = {}" | "vectors = {}" | "textures = {}"
            ) || (!typed && line == "look = Own");
            if !empty {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// A material from its file's text, and what reading it reported: its id, the paths its
    /// asset references were written with, lines it skipped.
    ///
    /// # Errors
    ///
    /// Not a material file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = engine_scene::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// Reads and parses the file at `path`.
    ///
    /// # Errors
    ///
    /// When it cannot be read or parsed.
    pub fn load(path: &Path) -> Result<(Self, ValueReport), MaterialFileError> {
        let error = |message: String| MaterialFileError {
            path: Some(path.to_owned()),
            message,
        };
        let text = std::fs::read_to_string(path).map_err(|e| error(e.to_string()))?;
        Self::parse(&text).map_err(|e| error(e.to_string()))
    }

    /// A typed file resolved for the cook and the run time: its shader's and textures' paths
    /// as `paths` has them (those it was written with), from the project's `root` (an
    /// absolute path as it is). `None` for the standard material.
    ///
    /// A shader that is a material graph (`.graph`, ADR 0123) is its generated module
    /// ([`crate::graph_module::graph_module`], in the project's cache), its type
    /// [`crate::graph_module::GRAPH_TYPE`], the graph's own values under the file's.
    ///
    /// # Errors
    ///
    /// A reference with no path, a field set twice (as a number and as a vector), or a
    /// graph that cannot be made a module.
    pub fn typed(
        &self,
        paths: &AssetPaths,
        root: &Path,
    ) -> Result<Option<TypedMaterial>, MaterialFileError> {
        if !self.is_typed() {
            return Ok(None);
        }
        let error = |message: String| MaterialFileError {
            path: None,
            message,
        };
        let path_of = |asset: AssetRef, what: &str| {
            paths
                .get(asset)
                .filter(|p| !p.is_empty())
                .map(|p| resolve(root, Path::new(p)))
                .ok_or_else(|| error(format!("`{what}`: no path written for {}", asset.file)))
        };
        let shader = path_of(self.shader, "shader")?;
        // A graph: its module, its type, its own values.
        let (shader, type_name, mut params, mut textures) =
            if crate::graph_module::is_graph(&shader) {
                let module = crate::graph_module::graph_module(&shader, root)?;
                (
                    module.module,
                    String::from(crate::graph_module::GRAPH_TYPE),
                    module.defaults,
                    module.textures,
                )
            } else {
                (
                    shader,
                    self.type_name.clone(),
                    BTreeMap::new(),
                    BTreeMap::new(),
                )
            };
        // The file's own values, each once, over the graph's.
        let mut own = std::collections::BTreeSet::new();
        let mut put = |name: &String, value: ParamValue| {
            if !own.insert(name.clone()) {
                return Err(error(format!("`{name}` is set twice")));
            }
            params.insert(name.clone(), value);
            Ok(())
        };
        for (name, &n) in &self.numbers {
            put(name, ParamValue::Number(f64::from(n)))?;
        }
        for (name, values) in &self.vectors {
            let list = values.iter().map(|&v| ParamValue::Number(f64::from(v)));
            put(name, ParamValue::List(list.collect()))?;
        }
        for (name, texture) in &self.textures {
            put(
                name,
                ParamValue::Texture(path_of(texture.image, name)?, texture.encoding),
            )?;
            textures.insert(name.clone(), texture.image);
        }
        Ok(Some(TypedMaterial {
            shader,
            type_name,
            params,
            textures,
            fallback: match self.look {
                Look::Own => Some(Fallback {
                    base_color: self.base_color,
                    metallic: self.metallic,
                    roughness: self.roughness,
                    emissive: self.emissive,
                }),
                Look::Replaced => None,
            },
            alpha: self.alpha,
            alpha_cutoff: self.alpha_cutoff,
            double_sided: self.double_sided,
        }))
    }
}

impl TypedMaterial {
    /// The parameters laid out as `layout` (the type's `Params`, from
    /// `engine_shader::Compiler::reflect_struct`); see the module documentation.
    pub fn build_params(&self, layout: &StructLayout) -> Result<BuiltParams, MaterialFileError> {
        let error = |message: String| MaterialFileError {
            path: None,
            message,
        };
        if let Some(unknown) = self.params.keys().find(|name| layout.field(name).is_none()) {
            let known: Vec<&str> = layout.fields.iter().map(|f| f.name.as_str()).collect();
            return Err(error(format!(
                "`{unknown}`: `{}` has no such field (it has {})",
                layout.name,
                known.join(", ")
            )));
        }
        let mut out = BuiltParams {
            bytes: vec![0; layout.size as usize],
            ..BuiltParams::default()
        };
        for field in &layout.fields {
            let elements = |at: u32| (0..field.count).map(move |i| at + i * field.stride);
            let Some(value) = self.params.get(&field.name) else {
                if field.kind == FieldKind::Texture {
                    for offset in elements(field.offset) {
                        put(&mut out.bytes, offset, NO_TEXTURE.to_le_bytes());
                    }
                }
                out.unset.push(field.name.clone());
                continue;
            };
            let per = match field.kind {
                FieldKind::Float(n) | FieldKind::Uint(n) | FieldKind::Int(n) => usize::from(n),
                FieldKind::Texture => 1,
            };
            let chunked: Vec<ParamValue>;
            let values: Vec<&ParamValue> = match (field.count, value) {
                (1, value) => vec![value],
                (count, ParamValue::List(list)) if list.len() == count as usize => {
                    list.iter().collect()
                }
                // An array of vectors written flat (ADR 0120).
                (count, ParamValue::List(list))
                    if per > 1 && list.len() == count as usize * per =>
                {
                    chunked = list
                        .chunks(per)
                        .map(|c| ParamValue::List(c.to_vec()))
                        .collect();
                    chunked.iter().collect()
                }
                (count, _) => {
                    return Err(error(format!(
                        "`{}`: an array of {count}: {count} values, or {} numbers written flat",
                        field.name,
                        count as usize * per
                    )));
                }
            };
            for (offset, value) in elements(field.offset).zip(values) {
                write_value(&mut out, offset, field.kind, value)
                    .map_err(|e| error(format!("`{}`: {e}", field.name)))?;
            }
        }
        Ok(out)
    }
}

fn put(bytes: &mut [u8], offset: u32, value: [u8; 4]) {
    let at = offset as usize;
    bytes[at..at + 4].copy_from_slice(&value);
}

fn numbers(value: &ParamValue, count: u8) -> Result<Vec<f64>, String> {
    let shape = || {
        if count == 1 {
            String::from("a number")
        } else {
            format!(
                "{count} numbers, as ({})",
                vec!["x"; usize::from(count)].join(", ")
            )
        }
    };
    match value {
        ParamValue::Number(n) if count == 1 => Ok(vec![*n]),
        ParamValue::List(list) if list.len() == usize::from(count) => list
            .iter()
            .map(|v| match v {
                ParamValue::Number(n) => Ok(*n),
                _ => Err(format!("expects {}", shape())),
            })
            .collect(),
        ParamValue::Texture(..) => Err(format!("expects {}, not a texture", shape())),
        _ => Err(format!("expects {}", shape())),
    }
}

fn write_value(
    out: &mut BuiltParams,
    offset: u32,
    kind: FieldKind,
    value: &ParamValue,
) -> Result<(), String> {
    match kind {
        FieldKind::Texture => match value {
            ParamValue::Texture(path, encoding) => {
                put(&mut out.bytes, offset, NO_TEXTURE.to_le_bytes());
                out.textures.push(TextureParam {
                    offset,
                    path: path.clone(),
                    encoding: *encoding,
                });
                Ok(())
            }
            _ => Err(String::from(
                "a TextureRef: expects Texture(\"path\", Color | Data | Normal)",
            )),
        },
        FieldKind::Float(n) => {
            for (k, v) in numbers(value, n)?.into_iter().enumerate() {
                put(
                    &mut out.bytes,
                    offset + 4 * k as u32,
                    (v as f32).to_le_bytes(),
                );
            }
            Ok(())
        }
        FieldKind::Uint(n) => {
            for (k, v) in numbers(value, n)?.into_iter().enumerate() {
                if v.fract() != 0.0 || !(0.0..=f64::from(u32::MAX)).contains(&v) {
                    return Err(format!("{v} is not a uint"));
                }
                put(
                    &mut out.bytes,
                    offset + 4 * k as u32,
                    (v as u32).to_le_bytes(),
                );
            }
            Ok(())
        }
        FieldKind::Int(n) => {
            for (k, v) in numbers(value, n)?.into_iter().enumerate() {
                if v.fract() != 0.0 || !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&v) {
                    return Err(format!("{v} is not an int"));
                }
                put(
                    &mut out.bytes,
                    offset + 4 * k as u32,
                    (v as i32).to_le_bytes(),
                );
            }
            Ok(())
        }
    }
}

/// The material files replacing a scene's glTF materials (`<scene>.materials.ron`, next to
/// the scene): glTF material name → material file, resolved against the map's folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaterialMap {
    /// By glTF material name.
    pub files: BTreeMap<String, PathBuf>,
}

impl MaterialMap {
    /// The map's path for a scene: `scene.gltf` → `scene.materials.ron`.
    pub fn path_for(scene: &Path) -> PathBuf {
        scene.with_extension("materials.ron")
    }

    /// Parses a map's text, resolving its paths against `dir`.
    pub fn parse(text: &str, dir: &Path) -> Result<Self, MaterialFileError> {
        let files: BTreeMap<String, PathBuf> = ron_options()
            .from_str(text)
            .map_err(|e| ron_error(None, &e))?;
        Ok(Self {
            files: files
                .into_iter()
                .map(|(name, path)| (name, resolve(dir, &path)))
                .collect(),
        })
    }

    /// The map next to `scene`, or an empty one when there is none.
    pub fn for_scene(scene: &Path) -> Result<Self, MaterialFileError> {
        let path = Self::path_for(scene);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(MaterialFileError {
                    path: Some(path),
                    message: e.to_string(),
                });
            }
        };
        let dir = path.parent().unwrap_or(Path::new(""));
        Self::parse(&text, dir).map_err(|e| MaterialFileError {
            path: Some(path.clone()),
            message: e.message,
        })
    }
}

/// What cooking material files needs of material types: their parameters' layout and
/// their compiled entry points (`engine_render` implements it with Slang, ADR 0092).
pub trait MaterialTypeBuilder {
    /// The layout of the type's `Params`.
    fn layout(&mut self, module: &Path, type_name: &str) -> Result<StructLayout, String>;
    /// Its fragment entry points' SPIR-V, by name.
    fn entries(
        &mut self,
        module: &Path,
        type_name: &str,
    ) -> Result<Vec<(String, Vec<u32>)>, String>;
}

/// [`MaterialTypeBuilder`] through Slang: the engine's `forward.slang` in `shader_dir`
/// compiled for each type (`engine_shader::material_types`); with a PSO record, only the
/// entry points it names for the types it names ([`Self::with_pso_record`]).
pub struct SlangMaterialTypes {
    compiler: engine_shader::Compiler,
    shader_dir: PathBuf,
    record: Option<PsoRecord>,
}

impl SlangMaterialTypes {
    /// Loads Slang and opens a session (a few hundred milliseconds).
    pub fn new(shader_dir: &Path) -> Result<Self, String> {
        let library = engine_shader::SlangLibrary::load().map_err(|e| e.to_string())?;
        let compiler = engine_shader::Compiler::new(library).map_err(|e| e.to_string())?;
        Ok(Self {
            compiler,
            shader_dir: shader_dir.to_owned(),
            record: None,
        })
    }

    /// Compiles only the entry points `record` names for the types it names (ADR 0092);
    /// the others whole, as without a record.
    pub fn with_pso_record(mut self, record: PsoRecord) -> Self {
        self.record = Some(record);
        self
    }
}

impl MaterialTypeBuilder for SlangMaterialTypes {
    fn layout(&mut self, module: &Path, type_name: &str) -> Result<StructLayout, String> {
        engine_shader::material_types::reflect_material_params(
            &mut self.compiler,
            &self.shader_dir,
            module,
            type_name,
        )
        .map_err(|e| e.to_string())
    }

    fn entries(
        &mut self,
        module: &Path,
        type_name: &str,
    ) -> Result<Vec<(String, Vec<u32>)>, String> {
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8);
        let recorded = self
            .record
            .as_ref()
            .and_then(|r| r.entries(module, type_name));
        let keep = |name: &str| recorded.is_none_or(|entries| entries.contains(name));
        engine_shader::material_types::compile_material_type_entries(
            &mut self.compiler,
            &self.shader_dir,
            module,
            type_name,
            threads,
            &keep,
        )
        .map_err(|e| e.to_string())
    }
}

/// Replaces `scene`'s materials named in `map` by their material files: a typed one becomes
/// a material of its file's type (built once per module and type), its parameters laid out,
/// its texture parameters added to the scene's textures, its glTF record the look
/// (overridden by the file's own look, `alpha` and `double_sided`); a standard one sets the
/// glTF record's values. The files' paths are read from the project's `root`. Returns
/// warnings: names the scene does not have, fields the files leave unset.
pub fn apply_material_map(
    scene: &mut ImportedScene,
    map: &MaterialMap,
    root: &Path,
    builder: &mut dyn MaterialTypeBuilder,
) -> Result<Vec<String>, MaterialFileError> {
    let mut warnings = Vec::new();
    for (name, path) in &map.files {
        let matching: Vec<usize> = scene
            .materials
            .iter()
            .enumerate()
            .filter(|(k, m)| &m.name == name || name.strip_prefix('#') == Some(&k.to_string()))
            .map(|(k, _)| k)
            .collect();
        if matching.is_empty() {
            warnings.push(format!(
                "{}: the scene has no material `{name}`",
                path.display()
            ));
            continue;
        }
        let error = |message: String| MaterialFileError {
            path: Some(path.clone()),
            message,
        };
        let (source, report) = MaterialFile::load(path)?;
        let alpha = |file_alpha: Alpha, cutoff: f32| match file_alpha {
            Alpha::Opaque => AlphaMode::Opaque,
            Alpha::Mask => AlphaMode::Mask(cutoff.to_bits()),
            Alpha::Blend => AlphaMode::Blend,
        };
        let Some(file) = source
            .typed(&report.assets, root)
            .map_err(|e| error(e.message))?
        else {
            // The standard material: its values on the glTF record.
            for material in matching {
                let m = &mut scene.materials[material];
                m.base_color = source.base_color;
                m.metallic = source.metallic;
                m.roughness = source.roughness;
                m.emissive = source.emissive;
                m.alpha = alpha(source.alpha, source.alpha_cutoff);
                m.double_sided = source.double_sided;
            }
            continue;
        };
        let module = file.shader.to_string_lossy().into_owned();
        let ty = match scene
            .material_types
            .iter()
            .position(|t| t.module == module && t.type_name == file.type_name)
        {
            Some(ty) => ty,
            None => {
                let entries = builder
                    .entries(&file.shader, &file.type_name)
                    .map_err(error)?;
                scene.material_types.push(MaterialTypeData {
                    module: module.clone(),
                    type_name: file.type_name.clone(),
                    entries,
                });
                scene.material_types.len() - 1
            }
        };
        let layout = builder
            .layout(&file.shader, &file.type_name)
            .map_err(error)?;
        let built = file.build_params(&layout).map_err(|e| error(e.message))?;
        if !built.unset.is_empty() {
            warnings.push(format!(
                "{}: unset (zero): {}",
                path.display(),
                built.unset.join(", ")
            ));
        }
        // Texture parameters' images, read now: a material file's paths may leave the
        // scene's folder, which glTF's own may not.
        let mut textures: Vec<(u32, usize, TextureEncoding)> = Vec::new();
        for t in &built.textures {
            let bytes =
                std::fs::read(&t.path).map_err(|e| error(format!("{}: {e}", t.path.display())))?;
            scene.textures.push(TextureData {
                source: TextureSource::Embedded {
                    bytes,
                    mime: String::new(),
                },
                wrap: [TextureWrap::Repeat; 2],
            });
            textures.push((t.offset, scene.textures.len() - 1, t.encoding));
        }
        for material in matching {
            let m = &mut scene.materials[material];
            if let Some(f) = file.fallback {
                m.base_color = f.base_color;
                m.metallic = f.metallic;
                m.roughness = f.roughness;
                m.emissive = f.emissive;
            }
            m.alpha = alpha(file.alpha, file.alpha_cutoff);
            m.double_sided = file.double_sided;
            scene.typed_materials.push(TypedMaterialData {
                material,
                ty,
                params: built.bytes.clone(),
                textures: textures.clone(),
            });
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_shader::Field;

    fn layout() -> StructLayout {
        let field = |name: &str, offset, kind: FieldKind, count, stride| Field {
            name: name.to_owned(),
            offset,
            kind,
            count,
            stride,
        };
        StructLayout {
            name: String::from("VelvetParams"),
            size: 72,
            fields: vec![
                field("color", 0, FieldKind::Float(4), 1, 16),
                field("sheen_color", 16, FieldKind::Float(3), 1, 12),
                field("sheen_roughness", 28, FieldKind::Float(1), 1, 4),
                field("pattern", 32, FieldKind::Texture, 1, 4),
                field("layers", 36, FieldKind::Uint(1), 2, 4),
                field("detail", 44, FieldKind::Texture, 1, 4),
                field("points", 48, FieldKind::Float(3), 2, 12),
            ],
        }
    }

    const VELVET: &str = r#"
[material version=2 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]

# A comment.
shader = asset("materials/velvet.slang", 7d1c0e9a2b3f4c5d8e6f7a8b9c0d1e2f)
type = "VelvetMaterial"
numbers["sheen_roughness"] = 0.6
vectors["color"] = [0.4, 0.05, 0.05, 1]
vectors["sheen_color"] = [0.9, 0.6, 0.6]
vectors["layers"] = [3, 7]
vectors["points"] = [1, 2, 3, 4, 5, 6]
textures["pattern"].image = asset("textures/velvet.png", 0a1b2c3d4e5f60718293a4b5c6d7e8f9)
textures["pattern"].encoding = Color
base_color = [0.4, 0.05, 0.05, 1]
roughness = 0.9
alpha = Mask
alpha_cutoff = 0.5
double_sided = true
"#;

    fn f32_at(bytes: &[u8], offset: usize) -> f32 {
        f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn a_material_file_reads_resolves_and_lays_out() {
        let (file, report) = MaterialFile::parse(VELVET).unwrap();
        assert!(file.is_typed());
        assert_eq!(report.version, 2);
        let root = Path::new("project");
        let typed = file.typed(&report.assets, root).unwrap().unwrap();
        assert_eq!(typed.shader, root.join("materials/velvet.slang"));
        assert_eq!(typed.type_name, "VelvetMaterial");
        assert_eq!((typed.alpha, typed.alpha_cutoff), (Alpha::Mask, 0.5));
        assert!(typed.double_sided);
        let fallback = typed.fallback.unwrap();
        assert_eq!(fallback.base_color, [0.4, 0.05, 0.05, 1.0]);
        assert_eq!(fallback.roughness, 0.9);
        assert_eq!(fallback.metallic, 0.0, "unset values keep the default");

        let built = typed.build_params(&layout()).unwrap();
        assert_eq!(built.bytes.len(), 72);
        assert_eq!(f32_at(&built.bytes, 0), 0.4);
        assert_eq!(f32_at(&built.bytes, 12), 1.0);
        assert_eq!(f32_at(&built.bytes, 24), 0.6);
        assert_eq!(f32_at(&built.bytes, 28), 0.6);
        assert_eq!(u32_at(&built.bytes, 36), 3);
        assert_eq!(u32_at(&built.bytes, 40), 7);
        // An array of vectors written flat.
        assert_eq!(f32_at(&built.bytes, 48), 1.0);
        assert_eq!(f32_at(&built.bytes, 60), 4.0);
        assert_eq!(f32_at(&built.bytes, 68), 6.0);
        assert_eq!(
            u32_at(&built.bytes, 32),
            u32::MAX,
            "filled by the material table"
        );
        assert_eq!(u32_at(&built.bytes, 44), u32::MAX, "an unset texture: none");
        assert_eq!(
            built.textures,
            vec![TextureParam {
                offset: 32,
                path: root.join("textures/velvet.png"),
                encoding: TextureEncoding::Color,
            }]
        );
        assert_eq!(built.unset, vec![String::from("detail")]);

        // Written again with its paths, it reads the same.
        let text = file.to_text_with(report.id.unwrap(), &report.assets);
        let (again, again_report) = MaterialFile::parse(&text).unwrap();
        assert_eq!(again, file);
        assert_eq!(
            again.typed(&again_report.assets, root).unwrap().unwrap(),
            typed
        );
    }

    #[test]
    fn version_1_is_the_standard_material_and_absolute_paths_stay() {
        let (file, report) = MaterialFile::parse(
            "[material version=1 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]\nroughness = 0.25\n",
        )
        .unwrap();
        assert_eq!(report.version, 1);
        assert!(!file.is_typed());
        assert_eq!(file.roughness, 0.25);
        assert_eq!(file.typed(&report.assets, Path::new("p")).unwrap(), None);
        // A file outside any project names its files by absolute paths.
        let module = std::env::temp_dir().join("velvet.slang");
        let text = format!(
            "[material version=2 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]\n\
             shader = asset({}, 7d1c0e9a2b3f4c5d8e6f7a8b9c0d1e2f)\ntype = \"V\"\nlook = Replaced\n",
            engine_scene::text::quote(&module.to_string_lossy())
        );
        let (file, report) = MaterialFile::parse(&text).unwrap();
        let typed = file
            .typed(&report.assets, Path::new("elsewhere"))
            .unwrap()
            .unwrap();
        assert_eq!(typed.shader, module);
        assert_eq!(typed.fallback, None, "the replaced glTF material's look");
    }

    #[test]
    fn errors_name_the_field() {
        let error = |lines: &str| {
            let text = format!(
                "[material version=2]\nshader = asset(\"v.slang\", 7d1c0e9a2b3f4c5d8e6f7a8b9c0d1e2f)\ntype = \"V\"\n{lines}\n"
            );
            let (file, report) = MaterialFile::parse(&text).unwrap();
            file.typed(&report.assets, Path::new(""))
                .unwrap()
                .unwrap()
                .build_params(&layout())
                .unwrap_err()
                .message
        };
        let unknown = error("numbers[\"colour\"] = 1");
        assert!(
            unknown.contains("`colour`") && unknown.contains("color"),
            "{unknown}"
        );
        let shape = error("vectors[\"sheen_color\"] = [1, 0.5]");
        assert!(
            shape.contains("`sheen_color`") && shape.contains("3 numbers"),
            "{shape}"
        );
        let not_texture = error("numbers[\"pattern\"] = 1");
        assert!(not_texture.contains("TextureRef"), "{not_texture}");
        let uint = error("vectors[\"layers\"] = [1.5, 2]");
        assert!(uint.contains("not a uint"), "{uint}");
        let array = error("numbers[\"layers\"] = 3");
        assert!(array.contains("an array of 2"), "{array}");
        // Set twice.
        let text = "[material version=2]\nshader = asset(\"v.slang\", 7d1c0e9a2b3f4c5d8e6f7a8b9c0d1e2f)\ntype = \"V\"\nnumbers[\"a\"] = 1\nvectors[\"a\"] = [1]\n";
        let (file, report) = MaterialFile::parse(text).unwrap();
        let twice = file
            .typed(&report.assets, Path::new(""))
            .unwrap_err()
            .message;
        assert!(twice.contains("`a` is set twice"), "{twice}");
        // A bad line names its number.
        let bad = MaterialFile::parse("[material version=2]\ntype = \n").unwrap_err();
        assert!(bad.to_string().contains('2'), "{bad}");
    }

    #[test]
    fn a_material_map_resolves_against_its_folder() {
        let map = MaterialMap::parse(
            r#"{ "fabric_red": "materials/red_velvet.material" }"#,
            Path::new("scenes"),
        )
        .unwrap();
        assert_eq!(
            map.files["fabric_red"],
            Path::new("scenes").join("materials/red_velvet.material")
        );
        assert_eq!(
            MaterialMap::path_for(Path::new("scenes/Sponza.gltf")),
            Path::new("scenes/Sponza.materials.ron")
        );
    }
}
