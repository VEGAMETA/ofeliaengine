//! A glTF file's outline (ADR 0107 E4-3): its node tree, its meshes with their
//! primitives' materials and bounds, and its materials' names and own looks (their factors
//! and their maps' image files, ADR 0123 E10-3d-2), read from the file's JSON alone (no
//! buffers, no images). The editor makes a dropped model's entities from it, and a new
//! material graph drawing a model's material.

use crate::gltf_import::ImportError;
use std::io::Read;
use std::path::Path;

/// What a glTF file holds, without its data.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GltfOutline {
    /// Every node, in the file's order.
    pub nodes: Vec<OutlineNode>,
    /// The roots of the default scene, as [`crate::import_gltf`] takes them: the first
    /// scene's without a default one, the nodes nobody holds without scenes.
    pub roots: Vec<usize>,
    /// Every mesh, in the file's order.
    pub meshes: Vec<OutlineMesh>,
    /// The materials' names, in the file's order (empty for unnamed ones).
    pub materials: Vec<String>,
    /// The materials' own looks, in the same order.
    pub maps: Vec<OutlineMaps>,
    /// Each skin's joints, as nodes (ADR 0124's E12-1c): what its skeleton moves (ones past
    /// the nodes left out).
    pub skins: Vec<Vec<usize>>,
}

/// A material's own look as its JSON gives it: its factors, and its maps' images where they
/// are files beside the glTF (an image inside the file is none).
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineMaps {
    /// Linear RGBA base color factor.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Roughness factor.
    pub roughness: f32,
    /// Linear emissive color (with `KHR_materials_emissive_strength` applied).
    pub emissive: [f32; 3],
    /// The base color map's image: its path from the glTF's folder, percent-decoded.
    pub base_color_image: Option<String>,
    /// The metallic (blue) and roughness (green) map's.
    pub metallic_roughness_image: Option<String>,
    /// The tangent-space normal map's.
    pub normal_image: Option<String>,
    /// The ambient occlusion map's (red).
    pub occlusion_image: Option<String>,
    /// The emissive map's.
    pub emissive_image: Option<String>,
}

impl Default for OutlineMaps {
    /// glTF's defaults: white, metallic and rough factors of 1, no emission, no maps.
    fn default() -> Self {
        Self {
            base_color: [1.0; 4],
            metallic: 1.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            base_color_image: None,
            metallic_roughness_image: None,
            normal_image: None,
            occlusion_image: None,
            emissive_image: None,
        }
    }
}

/// A node: its name, its transform in its parent's space, its mesh (and whether it draws it
/// skinned) and its children.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineNode {
    /// Its name (empty when the file gives none).
    pub name: String,
    /// Translation.
    pub translation: [f32; 3],
    /// Rotation (x, y, z, w).
    pub rotation: [f32; 4],
    /// Scale.
    pub scale: [f32; 3],
    /// Index into [`GltfOutline::meshes`].
    pub mesh: Option<usize>,
    /// The skin its mesh is drawn with, an index into the file's skins (ADR 0124's E12-1c);
    /// none for a mesh drawn as it is.
    pub skin: Option<usize>,
    /// Indices into [`GltfOutline::nodes`].
    pub children: Vec<usize>,
}

/// A mesh: its name and its primitives.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineMesh {
    /// Its name (empty when the file gives none).
    pub name: String,
    /// Its primitives.
    pub primitives: Vec<OutlinePrimitive>,
}

/// A primitive: its material and its positions' bounds in the mesh's space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlinePrimitive {
    /// Index into [`GltfOutline::materials`] (none: the default material).
    pub material: Option<usize>,
    /// The `POSITION` accessor's `min` and `max`, which the format requires (none when a
    /// file leaves them out).
    pub bounds: Option<([f32; 3], [f32; 3])>,
}

impl GltfOutline {
    /// The outline of the glTF file at `path`. Of a `.glb`, only the head and the JSON
    /// chunk are read.
    pub fn read(path: &Path) -> Result<Self, ImportError> {
        let io = |e: std::io::Error| ImportError::Buffer(format!("{}: {e}", path.display()));
        let mut file = std::fs::File::open(path).map_err(io)?;
        let size = file.metadata().map_err(io)?.len();
        // A GLB's head: magic, version, length, then its first chunk's length and type.
        let mut head = [0u8; 20];
        let mut read = 0;
        while read < head.len() {
            match file.read(&mut head[read..]).map_err(io)? {
                0 => break,
                n => read += n,
            }
        }
        if read == head.len() && head[..4] == *b"glTF" {
            if head[16..20] != *b"JSON" {
                return Err(ImportError::Parse(String::from(
                    "the GLB's first chunk is not its JSON",
                )));
            }
            let length = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
            if u64::from(length) > size.saturating_sub(20) {
                return Err(ImportError::Parse(String::from(
                    "the GLB's JSON chunk runs past the file's end",
                )));
            }
            let mut json = vec![0; length as usize];
            file.read_exact(&mut json).map_err(io)?;
            return Self::from_json(&json);
        }
        let mut json = head[..read].to_vec();
        file.read_to_end(&mut json).map_err(io)?;
        Self::from_json(&json)
    }

    /// The outline of a glTF file's JSON.
    pub fn from_json(json: &[u8]) -> Result<Self, ImportError> {
        let root =
            gltf::json::Root::from_slice(json).map_err(|e| ImportError::Parse(e.to_string()))?;
        let document = gltf::Document::from_json_without_validation(root);
        let materials = document
            .materials()
            .map(|m| m.name().unwrap_or_default().to_owned())
            .collect();
        // The maps' images through the JSON's indices, each checked: the document is not
        // validated, and its own accessors would index past a bad one.
        let json = document.as_json();
        let image = |texture: usize| -> Option<String> {
            let source = json.textures.get(texture)?.source.value();
            let uri = json.images.get(source)?.uri.as_deref()?;
            (!uri.starts_with("data:")).then(|| crate::gltf_import::percent_decode(uri))
        };
        let maps = document
            .materials()
            .zip(&json.materials)
            .map(|(m, j)| {
                let pbr = m.pbr_metallic_roughness();
                let strength = m.emissive_strength().unwrap_or(1.0);
                let info = |t: &Option<gltf::json::texture::Info>| {
                    t.as_ref().and_then(|t| image(t.index.value()))
                };
                OutlineMaps {
                    base_color: pbr.base_color_factor(),
                    metallic: pbr.metallic_factor(),
                    roughness: pbr.roughness_factor(),
                    emissive: m.emissive_factor().map(|e| e * strength),
                    base_color_image: info(&j.pbr_metallic_roughness.base_color_texture),
                    metallic_roughness_image: info(
                        &j.pbr_metallic_roughness.metallic_roughness_texture,
                    ),
                    normal_image: j
                        .normal_texture
                        .as_ref()
                        .and_then(|t| image(t.index.value())),
                    occlusion_image: j
                        .occlusion_texture
                        .as_ref()
                        .and_then(|t| image(t.index.value())),
                    emissive_image: info(&j.emissive_texture),
                }
            })
            .collect();
        let meshes = document
            .meshes()
            .map(|mesh| OutlineMesh {
                name: mesh.name().unwrap_or_default().to_owned(),
                primitives: mesh
                    .primitives()
                    .map(|p| OutlinePrimitive {
                        material: p.material().index(),
                        bounds: p
                            .get(&gltf::Semantic::Positions)
                            .and_then(|a| Some((corner(&a.min()?)?, corner(&a.max()?)?))),
                    })
                    .collect(),
            })
            .collect();
        let count = document.nodes().count();
        let skins: Vec<Vec<usize>> = json
            .skins
            .iter()
            .map(|s| {
                s.joints
                    .iter()
                    .map(|j| j.value())
                    .filter(|&j| j < count)
                    .collect()
            })
            .collect();
        let nodes: Vec<OutlineNode> = document
            .nodes()
            .map(|node| {
                let (translation, rotation, scale) = node.transform().decomposed();
                OutlineNode {
                    name: node.name().unwrap_or_default().to_owned(),
                    translation,
                    rotation,
                    scale,
                    mesh: node.mesh().map(|m| m.index()),
                    skin: json
                        .nodes
                        .get(node.index())
                        .and_then(|n| n.skin)
                        .map(|s| s.value())
                        .filter(|&s| s < skins.len()),
                    children: node
                        .children()
                        .map(|c| c.index())
                        .filter(|&c| c < count)
                        .collect(),
                }
            })
            .collect();
        let roots = match document
            .default_scene()
            .or_else(|| document.scenes().next())
        {
            Some(s) => s
                .nodes()
                .map(|n| n.index())
                .filter(|&n| n < count)
                .collect(),
            None => {
                let mut held = vec![false; count];
                for n in &nodes {
                    for &c in &n.children {
                        held[c] = true;
                    }
                }
                (0..count).filter(|&i| !held[i]).collect()
            }
        };
        Ok(Self {
            nodes,
            roots,
            meshes,
            materials,
            maps,
            skins,
        })
    }
}

/// Three numbers of an accessor's `min` or `max`.
fn corner(value: &gltf::json::Value) -> Option<[f32; 3]> {
    let a = value.as_array()?;
    let get = |i: usize| {
        a.get(i)
            .and_then(gltf::json::Value::as_f64)
            .map(|v| v as f32)
    };
    Some([get(0)?, get(1)?, get(2)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two nodes, the second the first's child, with a mesh of two primitives (one
    /// material each) and a mesh without bounds.
    const JSON: &str = r#"{
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [
            {"name": "Chair", "children": [1], "translation": [1, 2, 3]},
            {"name": "Seat", "mesh": 0, "scale": [2, 2, 2]},
            {"name": "Loose", "mesh": 1}
        ],
        "meshes": [
            {"name": "Seat", "primitives": [
                {"attributes": {"POSITION": 0}, "material": 0},
                {"attributes": {"POSITION": 0}, "material": 1}
            ]},
            {"primitives": [{"attributes": {"POSITION": 1}}]}
        ],
        "materials": [
            {"name": "Wood", "pbrMetallicRoughness": {
                "baseColorFactor": [0.5, 0.4, 0.3, 1],
                "baseColorTexture": {"index": 0},
                "metallicFactor": 0,
                "metallicRoughnessTexture": {"index": 1}},
             "normalTexture": {"index": 2},
             "emissiveFactor": [1, 0.5, 0],
             "extensions": {"KHR_materials_emissive_strength": {"emissiveStrength": 2}}},
            {"occlusionTexture": {"index": 7}}
        ],
        "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
        "images": [
            {"uri": "textures/wood%20grain.png"},
            {"uri": "data:image/png;base64,AAAA"},
            {"uri": "textures/wood_n.png"}
        ],
        "extensionsUsed": ["KHR_materials_emissive_strength"],
        "accessors": [
            {"componentType": 5126, "count": 3, "type": "VEC3",
             "min": [-0.5, 0, -0.25], "max": [0.5, 1, 0.25]},
            {"componentType": 5126, "count": 3, "type": "VEC3"}
        ]
    }"#;

    #[test]
    fn the_outline_has_the_tree_the_meshes_and_the_materials() {
        let outline = GltfOutline::from_json(JSON.as_bytes()).unwrap();
        assert_eq!(outline.roots, [0], "the default scene's roots");
        assert_eq!(outline.nodes[0].name, "Chair");
        assert_eq!(outline.nodes[0].translation, [1.0, 2.0, 3.0]);
        assert_eq!(outline.nodes[0].children, [1]);
        assert_eq!(outline.nodes[1].mesh, Some(0));
        assert_eq!(outline.nodes[1].scale, [2.0, 2.0, 2.0]);
        assert_eq!(outline.meshes[0].name, "Seat");
        let seat = &outline.meshes[0].primitives;
        assert_eq!(
            seat.iter().map(|p| p.material).collect::<Vec<_>>(),
            [Some(0), Some(1)]
        );
        assert_eq!(seat[0].bounds, Some(([-0.5, 0.0, -0.25], [0.5, 1.0, 0.25])));
        assert_eq!(outline.meshes[1].primitives[0].bounds, None);
        assert_eq!(outline.materials, ["Wood", ""]);
        // The materials' own looks: factors, files beside the glTF, an embedded image and a
        // bad index none.
        let wood = &outline.maps[0];
        assert_eq!(wood.base_color, [0.5, 0.4, 0.3, 1.0]);
        assert_eq!((wood.metallic, wood.roughness), (0.0, 1.0));
        assert_eq!(wood.emissive, [2.0, 1.0, 0.0]);
        assert_eq!(
            wood.base_color_image.as_deref(),
            Some("textures/wood grain.png")
        );
        assert_eq!(wood.metallic_roughness_image, None, "inside the file");
        assert_eq!(wood.normal_image.as_deref(), Some("textures/wood_n.png"));
        assert_eq!(outline.maps[1].occlusion_image, None, "no texture 7");
        assert_eq!(outline.maps[1].base_color, [1.0; 4], "glTF's defaults");
    }

    #[test]
    fn a_glb_is_read_from_its_head_and_json_chunk() {
        // A GLB of the JSON above padded to 4 bytes, with a binary chunk after it that
        // the outline never reads.
        let mut json = JSON.as_bytes().to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let bin = [0u8; 64];
        let total = 12 + 8 + json.len() + 8 + bin.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        let dir = std::env::temp_dir().join(format!("engine-outline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chair.glb");
        std::fs::write(&path, &glb).unwrap();
        let outline = GltfOutline::read(&path).unwrap();
        assert_eq!(outline.nodes.len(), 3);
        assert_eq!(outline.materials, ["Wood", ""]);
        // A chunk longer than the file: refused, not allocated.
        glb[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, &glb).unwrap();
        assert!(GltfOutline::read(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_node_drawing_its_mesh_skinned_names_its_skin() {
        let json = r#"{"asset": {"version": "2.0"},
            "nodes": [{"children": [1]}, {}, {"mesh": 0, "skin": 0}, {"mesh": 0, "skin": 3}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "skins": [{"joints": [1]}],
            "accessors": [{"componentType": 5126, "count": 3, "type": "VEC3"}]}"#;
        let outline = GltfOutline::from_json(json.as_bytes()).unwrap();
        assert_eq!(outline.nodes[2].skin, Some(0));
        assert_eq!(outline.nodes[3].skin, None, "no skin 3");
        assert_eq!(outline.nodes[0].skin, None);
        assert_eq!(outline.skins, [vec![1]], "its joints");
    }

    #[test]
    fn without_scenes_the_roots_are_the_nodes_nobody_holds() {
        let json = r#"{"asset": {"version": "2.0"},
            "nodes": [{"children": [2]}, {}, {}]}"#;
        let outline = GltfOutline::from_json(json.as_bytes()).unwrap();
        assert_eq!(outline.roots, [0, 1]);
    }
}
