//! Material files (ADR 0092): a material type's instance as text, `*.material.ron`.
//!
//! ```ron
//! // red_velvet.material.ron
//! (
//!     shader: "velvet.slang",          // the module defining the type
//!     type: "VelvetMaterial",          // the type implementing `IMaterial`
//!     params: {                        // its `Params` fields, by name
//!         "color": (0.40, 0.05, 0.05, 1.0),
//!         "sheen_color": (0.9, 0.6, 0.6),
//!         "sheen_roughness": 0.6,
//!         "pattern": Texture("../textures/velvet.png", Color),
//!     },
//!     // The glTF look while its shaders compile, and in GI's rays (default: the glTF
//!     // material it replaces, or grey).
//!     fallback: (base_color: (0.40, 0.05, 0.05, 1.0), roughness: 0.9),
//!     alpha: Opaque,                   // Opaque | Mask(0.5) | Blend
//!     double_sided: false,
//! )
//! ```
//!
//! - Paths are relative to the file's folder.
//! - A parameter is a number, a vector `(x, y, z)`, an array `[a, b, c]` (of numbers or
//!   vectors) or a `Texture(path, Color | Data | Normal)` for a `TextureRef` field; nested
//!   structs' fields are named `outer.inner`, arrays of structs' `name[i].inner`.
//! - [`MaterialFile::build_params`] lays the values out as the type's `Params` struct
//!   (from Slang's reflection, `engine_shader::StructLayout`). A field the file does not set
//!   is zero (a texture none: white) and listed in [`BuiltParams::unset`]; a name the
//!   struct does not have, or a value of the wrong shape, is an error naming it.
//!
//! A scene's glTF materials are replaced by material files through a file next to it,
//! `<scene>.materials.ron` ([`MaterialMap`]): `{ "glTF material name": "file.material.ron" }`.

use engine_shader::{FieldKind, StructLayout};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The extension of material files.
pub const MATERIAL_FILE_EXTENSION: &str = "material.ron";

/// How a texture parameter's image is encoded (the texture pipeline's usage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum TextureEncoding {
    /// sRGB color (BC7).
    Color,
    /// Linear data, one value per channel (BC7).
    Data,
    /// A tangent-space normal map (BC5).
    Normal,
}

/// A parameter's value.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub enum ParamValue {
    /// `Texture("path", Color)`: an image for a `TextureRef` field.
    Texture(PathBuf, TextureEncoding),
    /// A number.
    #[serde(untagged)]
    Number(f64),
    /// A vector `(x, y, z)` or an array `[a, b, c]`.
    #[serde(untagged)]
    List(Vec<ParamValue>),
}

/// How a material's alpha is used.
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
pub enum Alpha {
    /// Fully opaque.
    #[default]
    Opaque,
    /// Cut out below this coverage.
    Mask(f32),
    /// Blended over what is behind.
    Blend,
}

/// The glTF look of a material of a type: its first frames (before its shaders are made)
/// and what GI's rays see (ADR 0092).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
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

/// A material file (see the module documentation), its paths resolved.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialFile {
    /// The Slang module defining the type.
    pub shader: PathBuf,
    /// The type's name in it.
    #[serde(rename = "type")]
    pub type_name: String,
    /// Its parameters by field name.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
    /// Its glTF look; `None`: the replaced glTF material's, or [`Fallback::default`].
    #[serde(default)]
    pub fallback: Option<Fallback>,
    /// Alpha.
    #[serde(default)]
    pub alpha: Alpha,
    /// Drawn without back-face culling.
    #[serde(default)]
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

fn resolve_values(dir: &Path, value: &mut ParamValue) {
    match value {
        ParamValue::Texture(path, _) => *path = resolve(dir, path),
        ParamValue::List(values) => values.iter_mut().for_each(|v| resolve_values(dir, v)),
        ParamValue::Number(_) => {}
    }
}

/// RON as material files are written: optional fields without `Some(...)`.
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
    /// Parses a material file's text, resolving its relative paths against `dir` (the
    /// file's folder).
    pub fn parse(text: &str, dir: &Path) -> Result<Self, MaterialFileError> {
        let mut file: Self = ron_options()
            .from_str(text)
            .map_err(|e| ron_error(None, &e))?;
        file.shader = resolve(dir, &file.shader);
        for value in file.params.values_mut() {
            resolve_values(dir, value);
        }
        Ok(file)
    }

    /// Reads and parses `path`.
    pub fn load(path: &Path) -> Result<Self, MaterialFileError> {
        let error = |message: String| MaterialFileError {
            path: Some(path.to_owned()),
            message,
        };
        let text = std::fs::read_to_string(path).map_err(|e| error(e.to_string()))?;
        let dir = path.parent().unwrap_or(Path::new(""));
        Self::parse(&text, dir).map_err(|e| MaterialFileError {
            path: Some(path.to_owned()),
            message: e.message,
        })
    }

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
            let values: Vec<&ParamValue> = match (field.count, value) {
                (1, value) => vec![value],
                (count, ParamValue::List(list)) if list.len() == count as usize => {
                    list.iter().collect()
                }
                (count, _) => {
                    return Err(error(format!(
                        "`{}`: an array of {count}, written `[...]` with {count} values",
                        field.name
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

#[cfg(test)]
mod tests {
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
            size: 48,
            fields: vec![
                field("color", 0, FieldKind::Float(4), 1, 16),
                field("sheen_color", 16, FieldKind::Float(3), 1, 12),
                field("sheen_roughness", 28, FieldKind::Float(1), 1, 4),
                field("pattern", 32, FieldKind::Texture, 1, 4),
                field("layers", 36, FieldKind::Uint(1), 2, 4),
                field("detail", 44, FieldKind::Texture, 1, 4),
            ],
        }
    }

    const VELVET: &str = r#"
        // A comment.
        (
            shader: "velvet.slang",
            type: "VelvetMaterial",
            params: {
                "color": (0.40, 0.05, 0.05, 1.0),
                "sheen_color": (0.9, 0.6, 0.6),
                "sheen_roughness": 0.6,
                "pattern": Texture("../textures/velvet.png", Color),
                "layers": [3, 7],
            },
            fallback: (base_color: (0.40, 0.05, 0.05, 1.0), roughness: 0.9),
            alpha: Mask(0.5),
            double_sided: true,
        )
    "#;

    fn f32_at(bytes: &[u8], offset: usize) -> f32 {
        f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn parses_resolves_and_lays_out_a_material_file() {
        let dir = Path::new("assets/materials");
        let file = MaterialFile::parse(VELVET, dir).unwrap();
        assert_eq!(file.shader, dir.join("velvet.slang"));
        assert_eq!(file.type_name, "VelvetMaterial");
        assert_eq!(file.alpha, Alpha::Mask(0.5));
        assert!(file.double_sided);
        let fallback = file.fallback.unwrap();
        assert_eq!(fallback.base_color, [0.40, 0.05, 0.05, 1.0]);
        assert_eq!(fallback.roughness, 0.9);
        assert_eq!(
            fallback.metallic, 0.0,
            "unset fallback fields keep the default"
        );

        let built = file.build_params(&layout()).unwrap();
        assert_eq!(built.bytes.len(), 48);
        assert_eq!(f32_at(&built.bytes, 0), 0.40);
        assert_eq!(f32_at(&built.bytes, 12), 1.0);
        assert_eq!(f32_at(&built.bytes, 24), 0.6);
        assert_eq!(f32_at(&built.bytes, 28), 0.6);
        assert_eq!(u32_at(&built.bytes, 36), 3);
        assert_eq!(u32_at(&built.bytes, 40), 7);
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
                path: dir.join("../textures/velvet.png"),
                encoding: TextureEncoding::Color,
            }]
        );
        assert_eq!(built.unset, vec![String::from("detail")]);
    }

    #[test]
    fn errors_name_the_field() {
        let error = |params: &str| {
            let text = format!("(shader: \"v.slang\", type: \"V\", params: {{ {params} }})");
            MaterialFile::parse(&text, Path::new(""))
                .unwrap()
                .build_params(&layout())
                .unwrap_err()
                .message
        };
        let unknown = error("\"colour\": 1.0");
        assert!(
            unknown.contains("`colour`") && unknown.contains("color"),
            "{unknown}"
        );
        let shape = error("\"sheen_color\": (1.0, 0.5)");
        assert!(
            shape.contains("`sheen_color`") && shape.contains("3 numbers"),
            "{shape}"
        );
        let texture = error("\"sheen_roughness\": Texture(\"a.png\", Data)");
        assert!(texture.contains("not a texture"), "{texture}");
        let not_texture = error("\"pattern\": 1.0");
        assert!(not_texture.contains("TextureRef"), "{not_texture}");
        let uint = error("\"layers\": [1.5, 2]");
        assert!(uint.contains("not a uint"), "{uint}");
        let array = error("\"layers\": 3");
        assert!(array.contains("an array of 2"), "{array}");

        let syntax = MaterialFile::parse("(shader: \"v.slang\", type: )", Path::new(""))
            .unwrap_err()
            .message;
        assert!(syntax.starts_with("1:"), "line and column: {syntax}");
        let unknown_key = MaterialFile::parse(
            "(shader: \"v.slang\", type: \"V\", colour: 1)",
            Path::new(""),
        )
        .unwrap_err()
        .message;
        assert!(unknown_key.contains("colour"), "{unknown_key}");
    }

    #[test]
    fn a_material_map_resolves_against_its_folder() {
        let map = MaterialMap::parse(
            r#"{ "fabric_red": "materials/red_velvet.material.ron" }"#,
            Path::new("scenes"),
        )
        .unwrap();
        assert_eq!(
            map.files["fabric_red"],
            Path::new("scenes").join("materials/red_velvet.material.ron")
        );
        assert_eq!(
            MaterialMap::path_for(Path::new("scenes/Sponza.gltf")),
            Path::new("scenes/Sponza.materials.ron")
        );
    }
}
