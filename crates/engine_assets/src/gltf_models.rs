//! glTF's material extensions on the shading models (ADR 0092,
//! `docs/research/gltf-material-extensions.md`): the materials whose glTF uses
//! `KHR_materials_unlit`, `KHR_materials_clearcoat` or `KHR_materials_sheen`
//! ([`MaterialLayers`]) become materials of the engine's built-in types (the engine's
//! `shaders/materials/gltf.slang`), their glTF record the fallback, as material files make
//! materials of their own types.
//!
//! | Extensions | Type | Shading model |
//! |---|---|---|
//! | unlit | `GltfUnlit` | unlit |
//! | clear coat | `GltfClearcoat` | clear coat |
//! | sheen | `GltfSheen` | lit under a sheen layer |
//! | sheen and clear coat | `GltfSheenClearcoat` | the sheen layer under a coat |

use crate::gltf_import::{
    AlphaMode, ImportedScene, MaterialData, MaterialLayers, MaterialTypeData, TypedMaterialData,
};
use crate::material_file::{MaterialFileError, MaterialTypeBuilder, TextureEncoding};
use engine_shader::{FieldKind, StructLayout};
use std::collections::HashSet;
use std::path::Path;

/// The built-in types' module, relative to the engine's shader folder.
pub const GLTF_MODELS_MODULE: &str = "materials/gltf.slang";

/// The built-in type of a material with these extensions, if they set a model.
pub fn gltf_model_type(layers: &MaterialLayers) -> Option<&'static str> {
    match (
        layers.unlit,
        layers.clearcoat.is_some(),
        layers.sheen.is_some(),
    ) {
        (true, _, _) => Some("GltfUnlit"),
        (false, true, true) => Some("GltfSheenClearcoat"),
        (false, true, false) => Some("GltfClearcoat"),
        (false, false, true) => Some("GltfSheen"),
        (false, false, false) => None,
    }
}

/// A built-in type's parameter.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// `float` to `float4`.
    Floats(Vec<f32>),
    /// A `TextureRef`: one of the scene's textures, or none (white).
    Texture(Option<usize>, TextureEncoding),
}

/// A material's extension values by the built-in types' field names (`gltf.slang`), and
/// its glTF alpha cutoff (the masked depth passes' `alpha_cutoff` has no other way to it).
fn values(m: &MaterialData) -> Vec<(&'static str, Value)> {
    let cutoff = match m.alpha {
        AlphaMode::Mask(bits) => f32::from_bits(bits),
        AlphaMode::Opaque | AlphaMode::Blend => 0.5,
    };
    let mut values = vec![("alpha_cutoff", Value::Floats(vec![cutoff]))];
    if let Some(c) = m.layers.clearcoat {
        values.extend([
            ("clearcoat", Value::Floats(vec![c.factor])),
            ("clearcoat_roughness", Value::Floats(vec![c.roughness])),
            (
                "clearcoat_normal_scale",
                Value::Floats(vec![c.normal_scale]),
            ),
            (
                "clearcoat_texture",
                Value::Texture(c.texture, TextureEncoding::Data),
            ),
            (
                "clearcoat_roughness_texture",
                Value::Texture(c.roughness_texture, TextureEncoding::Data),
            ),
            (
                "clearcoat_normal_texture",
                Value::Texture(c.normal_texture, TextureEncoding::Normal),
            ),
        ]);
    }
    if let Some(s) = m.layers.sheen {
        values.extend([
            ("sheen_color", Value::Floats(s.color.to_vec())),
            ("sheen_roughness", Value::Floats(vec![s.roughness])),
            (
                "sheen_color_texture",
                Value::Texture(s.color_texture, TextureEncoding::Color),
            ),
            (
                "sheen_roughness_texture",
                Value::Texture(s.roughness_texture, TextureEncoding::Data),
            ),
        ]);
    }
    values
}

/// What `material_params` reads for a texture field without a texture: `NO_TEXTURE`, white.
const NO_TEXTURE: u32 = u32::MAX;

/// A typed material's texture fields: byte offset, scene texture and encoding (as
/// [`TypedMaterialData::textures`]).
type TextureFields = Vec<(u32, usize, TextureEncoding)>;

/// A type's parameter bytes laid out as `layout`, with its texture fields. Every field of
/// the struct must be in `values`, with its shape: otherwise `gltf.slang` and this file
/// disagree.
fn lay_out(
    layout: &StructLayout,
    values: &[(&str, Value)],
) -> Result<(Vec<u8>, TextureFields), String> {
    let mut bytes = vec![0u8; layout.size as usize];
    let mut textures = Vec::new();
    for field in &layout.fields {
        let Some((_, value)) = values.iter().find(|(name, _)| *name == field.name) else {
            return Err(format!("no value for `{}`", field.name));
        };
        let at = field.offset as usize;
        match (field.kind, value) {
            (FieldKind::Float(n), Value::Floats(v))
                if field.count == 1 && v.len() == usize::from(n) =>
            {
                for (i, x) in v.iter().enumerate() {
                    bytes[at + 4 * i..at + 4 * i + 4].copy_from_slice(&x.to_le_bytes());
                }
            }
            (FieldKind::Texture, Value::Texture(texture, encoding)) if field.count == 1 => {
                bytes[at..at + 4].copy_from_slice(&NO_TEXTURE.to_le_bytes());
                if let Some(texture) = texture {
                    textures.push((field.offset, *texture, *encoding));
                }
            }
            (kind, value) => {
                return Err(format!(
                    "`{}` is {kind:?} × {}, the value {value:?}",
                    field.name, field.count
                ));
            }
        }
    }
    Ok((bytes, textures))
}

/// Makes the materials of `scene` whose extensions set a shading model
/// ([`MaterialLayers`]) materials of the engine's built-in types, each type built once with
/// `builder` (on the engine's shader folder): its entry points compiled, its parameters
/// laid out from Slang's reflection. Materials that already have a type (material files
/// replace glTF's materials: run [`crate::material_file::apply_material_map`] first) keep
/// it. Returns how many materials it gave a type.
pub fn apply_gltf_models(
    scene: &mut ImportedScene,
    builder: &mut dyn MaterialTypeBuilder,
) -> Result<usize, MaterialFileError> {
    let module = Path::new(GLTF_MODELS_MODULE);
    let typed: HashSet<usize> = scene.typed_materials.iter().map(|t| t.material).collect();
    // By type: its index in the scene's types and its parameters' layout.
    let mut types: Vec<(&'static str, usize, StructLayout)> = Vec::new();
    let mut made = 0;
    for material in 0..scene.materials.len() {
        let Some(type_name) = gltf_model_type(&scene.materials[material].layers) else {
            continue;
        };
        if typed.contains(&material) {
            continue;
        }
        let error = |message: String| MaterialFileError {
            path: None,
            message: format!("{GLTF_MODELS_MODULE}, `{type_name}`: {message}"),
        };
        let k = match types.iter().position(|(name, _, _)| *name == type_name) {
            Some(k) => k,
            None => {
                let ty = match scene
                    .material_types
                    .iter()
                    .position(|t| t.module == GLTF_MODELS_MODULE && t.type_name == type_name)
                {
                    Some(ty) => ty,
                    None => {
                        let entries = builder.entries(module, type_name).map_err(error)?;
                        scene.material_types.push(MaterialTypeData {
                            module: GLTF_MODELS_MODULE.to_owned(),
                            type_name: type_name.to_owned(),
                            entries,
                        });
                        scene.material_types.len() - 1
                    }
                };
                let layout = builder.layout(module, type_name).map_err(error)?;
                types.push((type_name, ty, layout));
                types.len() - 1
            }
        };
        let (_, ty, layout) = &types[k];
        let m = &scene.materials[material];
        // Unlit reads its glTF record only: its slot is the record's size of zeros.
        let (params, textures) = if m.layers.unlit {
            (vec![0u8; layout.size as usize], Vec::new())
        } else {
            lay_out(layout, &values(m)).map_err(error)?
        };
        scene.typed_materials.push(TypedMaterialData {
            material,
            ty: *ty,
            params,
            textures,
        });
        made += 1;
    }
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gltf_import::{ClearcoatData, SheenData};
    use engine_shader::Field;

    fn field(name: &str, offset: u32, kind: FieldKind) -> Field {
        Field {
            name: name.to_owned(),
            offset,
            kind,
            count: 1,
            stride: kind.bytes(),
        }
    }

    /// `GltfClearcoatParams` as Slang lays it out (natural layout).
    fn clearcoat_layout() -> StructLayout {
        StructLayout {
            name: String::from("GltfClearcoatParams"),
            size: 28,
            fields: vec![
                field("clearcoat", 0, FieldKind::Float(1)),
                field("clearcoat_roughness", 4, FieldKind::Float(1)),
                field("clearcoat_normal_scale", 8, FieldKind::Float(1)),
                field("alpha_cutoff", 12, FieldKind::Float(1)),
                field("clearcoat_texture", 16, FieldKind::Texture),
                field("clearcoat_roughness_texture", 20, FieldKind::Texture),
                field("clearcoat_normal_texture", 24, FieldKind::Texture),
            ],
        }
    }

    /// A builder that compiles nothing: one entry point per type, and the clear coat's
    /// layout for every type.
    #[derive(Default)]
    struct FakeBuilder {
        built: Vec<String>,
    }

    impl MaterialTypeBuilder for FakeBuilder {
        fn layout(&mut self, _: &Path, _: &str) -> Result<StructLayout, String> {
            Ok(clearcoat_layout())
        }

        fn entries(
            &mut self,
            module: &Path,
            type_name: &str,
        ) -> Result<Vec<(String, Vec<u32>)>, String> {
            assert_eq!(module, Path::new(GLTF_MODELS_MODULE));
            self.built.push(type_name.to_owned());
            Ok(vec![(String::from("fragmentMain"), vec![0x0723_0203])])
        }
    }

    fn material(layers: MaterialLayers) -> MaterialData {
        MaterialData {
            name: String::new(),
            base_color: [1.0; 4],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            base_color_texture: None,
            metallic_roughness_texture: None,
            normal_texture: None,
            normal_scale: 1.0,
            occlusion_texture: None,
            occlusion_strength: 1.0,
            emissive_texture: None,
            alpha: AlphaMode::Mask(0.25f32.to_bits()),
            double_sided: false,
            thickness: 0.0,
            thickness_texture: None,
            layers,
        }
    }

    fn coat() -> ClearcoatData {
        ClearcoatData {
            factor: 1.0,
            texture: Some(3),
            roughness: 0.2,
            roughness_texture: None,
            normal_texture: Some(5),
            normal_scale: 0.5,
        }
    }

    #[test]
    fn extensions_pick_the_types() {
        let sheen = SheenData {
            color: [1.0, 0.5, 0.25],
            color_texture: None,
            roughness: 0.3,
            roughness_texture: None,
        };
        let types = [
            MaterialLayers::default(),
            MaterialLayers {
                unlit: true,
                ..MaterialLayers::default()
            },
            MaterialLayers {
                clearcoat: Some(coat()),
                ..MaterialLayers::default()
            },
            MaterialLayers {
                sheen: Some(sheen),
                ..MaterialLayers::default()
            },
            MaterialLayers {
                clearcoat: Some(coat()),
                sheen: Some(sheen),
                ..MaterialLayers::default()
            },
        ]
        .map(|l| gltf_model_type(&l));
        assert_eq!(
            types,
            [
                None,
                Some("GltfUnlit"),
                Some("GltfClearcoat"),
                Some("GltfSheen"),
                Some("GltfSheenClearcoat")
            ]
        );
    }

    #[test]
    fn a_clear_coat_is_laid_out_and_types_are_built_once() {
        let mut scene = ImportedScene::default();
        let layers = MaterialLayers {
            clearcoat: Some(coat()),
            ..MaterialLayers::default()
        };
        scene.materials = vec![
            material(layers),
            material(MaterialLayers::default()),
            material(layers),
            material(layers),
        ];
        // The fourth material has a type from a material file: it keeps it.
        scene.material_types.push(MaterialTypeData::default());
        scene.typed_materials.push(TypedMaterialData {
            material: 3,
            ..TypedMaterialData::default()
        });
        let mut builder = FakeBuilder::default();
        assert_eq!(apply_gltf_models(&mut scene, &mut builder).unwrap(), 2);
        assert_eq!(builder.built, ["GltfClearcoat"], "one type, built once");
        assert_eq!(scene.material_types.len(), 2);
        assert_eq!(scene.material_types[1].type_name, "GltfClearcoat");
        let made: Vec<usize> = scene.typed_materials.iter().map(|t| t.material).collect();
        assert_eq!(made, [3, 0, 2]);
        let t = &scene.typed_materials[1];
        assert_eq!(t.ty, 1);
        let float = |at: usize| f32::from_le_bytes(t.params[at..at + 4].try_into().unwrap());
        let word = |at: usize| u32::from_le_bytes(t.params[at..at + 4].try_into().unwrap());
        assert_eq!(
            (float(0), float(4), float(8), float(12)),
            (1.0, 0.2, 0.5, 0.25),
            "factors and the mask's cutoff"
        );
        assert_eq!(
            (word(16), word(20), word(24)),
            (NO_TEXTURE, NO_TEXTURE, NO_TEXTURE)
        );
        assert_eq!(
            t.textures,
            [
                (16, 3, TextureEncoding::Data),
                (24, 5, TextureEncoding::Normal)
            ],
            "the coat's texture and normal map; no roughness texture"
        );
    }

    #[test]
    fn a_field_without_a_value_is_an_error() {
        let mut layout = clearcoat_layout();
        layout
            .fields
            .push(field("clearcoat_tint", 28, FieldKind::Float(3)));
        let values = values(&material(MaterialLayers {
            clearcoat: Some(coat()),
            ..MaterialLayers::default()
        }));
        assert!(
            lay_out(&layout, &values)
                .unwrap_err()
                .contains("clearcoat_tint")
        );
        // The wrong shape too.
        let mut layout = clearcoat_layout();
        layout.fields[0].kind = FieldKind::Float(2);
        assert!(
            lay_out(&layout, &values)
                .unwrap_err()
                .contains("`clearcoat`")
        );
    }
}
