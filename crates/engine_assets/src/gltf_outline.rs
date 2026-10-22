//! A glTF file's outline (ADR 0107 E4-3): its node tree, its meshes with their
//! primitives' materials and bounds, and its materials' names, read from the file's JSON
//! alone (no buffers, no images). The editor makes a dropped model's entities from it.

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
}

/// A node: its name, its transform in its parent's space, its mesh and its children.
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
        "materials": [{"name": "Wood"}, {}],
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
    fn without_scenes_the_roots_are_the_nodes_nobody_holds() {
        let json = r#"{"asset": {"version": "2.0"},
            "nodes": [{"children": [2]}, {}, {}]}"#;
        let outline = GltfOutline::from_json(json.as_bytes()).unwrap();
        assert_eq!(outline.roots, [0, 1]);
    }
}
