//! A blockout group as a binary glTF (ADR 0110 E2b-6; `docs/research/ux/blockout.md` §8):
//! its visible surface, so Blender opens the greybox and the player draws it baked.
//!
//! - **A mesh per tint:** the brushes of one tint share a mesh `"<name> <k>"` and its
//!   material `"<name> <k> grid"`, its base color the tint over the blockout's grid
//!   (embedded as a PNG, the texture coordinates metres), so the file looks as the editor
//!   draws it, and each mesh is a [`crate::Model`] of one material.
//! - **The group's space:** metres, y up, as glTF wants; the group's own placement stays in
//!   the scene.

use crate::BakedBrush;
use serde_json::json;

/// glTF's component types and targets.
const FLOAT: u32 = 5126;
const UNSIGNED_INT: u32 = 5125;
const ARRAY_BUFFER: u32 = 34962;
const ELEMENT_ARRAY_BUFFER: u32 = 34963;

/// A baked group: the file's bytes, and each of its meshes' name with its material's.
#[derive(Debug, Clone, PartialEq)]
pub struct Baked {
    /// The binary glTF.
    pub bytes: Vec<u8>,
    /// A mesh's name and its material's, per tint, in the order the tints first come.
    pub parts: Vec<(String, String)>,
}

/// The brushes of one tint, merged.
#[derive(Debug, Default)]
struct Part {
    tint: [f32; 4],
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

/// The brushes' parts merged by tint, in the order the tints first come.
fn by_tint(brushes: &[BakedBrush]) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    for b in brushes {
        let key = b.tint.map(f32::to_bits);
        let k = match parts.iter().position(|p| p.tint.map(f32::to_bits) == key) {
            Some(k) => k,
            None => {
                parts.push(Part {
                    tint: b.tint,
                    ..Part::default()
                });
                parts.len() - 1
            }
        };
        let p = &mut parts[k];
        let first = p.positions.len() as u32;
        p.positions.extend_from_slice(&b.positions);
        p.normals.extend_from_slice(&b.normals);
        p.uvs.extend_from_slice(&b.uvs);
        p.indices.extend(b.indices.iter().map(|i| i + first));
    }
    parts
}

/// The binary glTF of a group called `name` made of `brushes`, its materials over the grid
/// image `grid_png` (a PNG; empty: plain colors).
pub fn glb(name: &str, brushes: &[BakedBrush], grid_png: &[u8]) -> Baked {
    let parts = by_tint(brushes);
    let mut bin: Vec<u8> = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    // A view of `bytes` in the buffer, four-byte aligned.
    let mut view = |bin: &mut Vec<u8>, bytes: &[u8], target: Option<u32>| {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let mut v = json!({ "buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len() });
        if let Some(t) = target {
            v["target"] = json!(t);
        }
        bin.extend_from_slice(bytes);
        views.push(v);
        views.len() - 1
    };
    let mut meshes = Vec::new();
    let mut materials = Vec::new();
    let mut nodes = Vec::new();
    let mut names = Vec::new();
    let texture = !grid_png.is_empty();
    for (k, p) in parts.iter().enumerate() {
        let floats = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for q in &p.positions {
            for i in 0..3 {
                lo[i] = lo[i].min(q[i]);
                hi[i] = hi[i].max(q[i]);
            }
        }
        let count = p.positions.len();
        let positions = view(
            &mut bin,
            &floats(p.positions.as_flattened()),
            Some(ARRAY_BUFFER),
        );
        let normals = view(
            &mut bin,
            &floats(p.normals.as_flattened()),
            Some(ARRAY_BUFFER),
        );
        let uvs = view(&mut bin, &floats(p.uvs.as_flattened()), Some(ARRAY_BUFFER));
        let indices: Vec<u8> = p.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        let indices = view(&mut bin, &indices, Some(ELEMENT_ARRAY_BUFFER));
        let first = accessors.len();
        accessors.push(json!({ "bufferView": positions, "componentType": FLOAT, "count": count, "type": "VEC3", "min": lo, "max": hi }));
        accessors.push(json!({ "bufferView": normals, "componentType": FLOAT, "count": count, "type": "VEC3" }));
        accessors.push(
            json!({ "bufferView": uvs, "componentType": FLOAT, "count": count, "type": "VEC2" }),
        );
        accessors.push(json!({ "bufferView": indices, "componentType": UNSIGNED_INT, "count": p.indices.len(), "type": "SCALAR" }));
        let (mesh, material) = (
            format!("{name} {}", k + 1),
            format!("{name} {} grid", k + 1),
        );
        meshes.push(json!({
            "name": mesh,
            "primitives": [{
                "attributes": { "POSITION": first, "NORMAL": first + 1, "TEXCOORD_0": first + 2 },
                "indices": first + 3,
                "material": k,
            }],
        }));
        let mut pbr =
            json!({ "baseColorFactor": p.tint, "metallicFactor": 0.0, "roughnessFactor": 0.85 });
        if texture {
            pbr["baseColorTexture"] = json!({ "index": 0 });
        }
        materials.push(json!({ "name": material, "pbrMetallicRoughness": pbr }));
        nodes.push(json!({ "name": mesh, "mesh": k }));
        names.push((mesh, material));
    }
    let mut root = json!({
        "asset": { "version": "2.0", "generator": "ofeliaengine blockout" },
        "scene": 0,
        "scenes": [{ "name": name, "nodes": (0..nodes.len()).collect::<Vec<_>>() }],
        "nodes": nodes,
        "meshes": meshes,
        "materials": materials,
    });
    if texture {
        let image = view(&mut bin, grid_png, None);
        root["images"] = json!([{ "bufferView": image, "mimeType": "image/png", "name": "grid" }]);
        // Linear, with mipmaps, repeating: the grid every metre.
        root["samplers"] =
            json!([{ "magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497 }]);
        root["textures"] = json!([{ "sampler": 0, "source": 0 }]);
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    root["bufferViews"] = json!(views);
    root["accessors"] = json!(accessors);
    root["buffers"] = json!([{ "byteLength": bin.len() }]);
    let mut text = root.to_string().into_bytes();
    while !text.len().is_multiple_of(4) {
        text.push(b' ');
    }
    // The container: its header, the JSON chunk, the binary chunk.
    let length = 12 + 8 + text.len() + 8 + bin.len();
    let mut bytes = Vec::with_capacity(length);
    bytes.extend_from_slice(b"glTF");
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&(length as u32).to_le_bytes());
    bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"JSON");
    bytes.extend_from_slice(&text);
    bytes.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"BIN\0");
    bytes.extend_from_slice(&bin);
    Baked {
        bytes,
        parts: names,
    }
}

/// A file name for a group called `name`: letters, digits and `-`, `_` kept, the rest `-`.
pub fn file_name(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches('-');
    format!("{}.glb", if stem.is_empty() { "blockout" } else { stem })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A quad of tint `tint` at height `y`, facing up.
    pub(crate) fn quad(tint: [f32; 4], y: f32) -> BakedBrush {
        BakedBrush {
            tint,
            positions: vec![[0.0, y, 0.0], [0.0, y, 1.0], [1.0, y, 1.0], [1.0, y, 0.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
        }
    }

    #[test]
    fn a_group_bakes_to_a_glb_our_importer_reads_back_a_mesh_per_tint() {
        let grey = [0.72, 0.72, 0.72, 1.0];
        let red = [0.8, 0.1, 0.1, 1.0];
        let brushes = [quad(grey, 0.0), quad(red, 1.0), quad(grey, 2.0)];
        let png = crate::grid_png();
        assert!(png.starts_with(b"\x89PNG"));
        let baked = glb("Blockout", &brushes, &png);
        assert_eq!(&baked.bytes[..4], b"glTF");
        assert_eq!(
            baked.parts,
            [
                (String::from("Blockout 1"), String::from("Blockout 1 grid")),
                (String::from("Blockout 2"), String::from("Blockout 2 grid")),
            ]
        );
        let scene = engine_assets::import_gltf(&baked.bytes, None).unwrap();
        // Grey's two quads in one mesh, red's in another, each one part.
        assert_eq!(scene.meshes.len(), 2);
        assert_eq!(scene.meshes[0].name, "Blockout 1");
        assert_eq!(scene.meshes[0].primitives.len(), 1);
        assert_eq!(scene.meshes[0].primitives[0].indices.len(), 12);
        assert_eq!(scene.meshes[0].primitives[0].positions[4], [0.0, 2.0, 0.0]);
        assert_eq!(scene.meshes[1].primitives[0].uvs[2], [1.0, 1.0]);
        let red_material = &scene.materials[scene.meshes[1].primitives[0].material.unwrap()];
        assert_eq!(red_material.name, "Blockout 2 grid");
        assert_eq!(red_material.base_color, red);
        assert!(red_material.base_color_texture.is_some(), "over the grid");
        assert_eq!(scene.textures.len(), 1);
    }

    #[test]
    fn file_names_keep_letters_and_digits() {
        assert_eq!(file_name("Blockout"), "blockout.glb");
        assert_eq!(file_name("Level 2: the yard"), "level-2--the-yard.glb");
        assert_eq!(file_name("***"), "blockout.glb");
    }
}
