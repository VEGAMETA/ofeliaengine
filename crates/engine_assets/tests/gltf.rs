//! glTF import: values, GLB, generated normals and indices, the node hierarchy in the ECS,
//! external buffers, and errors. Test files are built here (no third-party assets).

#![allow(clippy::unwrap_used, clippy::float_cmp)]

use engine_assets::{AlphaMode, ImportError, MeshInstance, import_gltf, spawn_scene};
use engine_ecs::{GlobalTransform, QueryState, TransformPropagation, World};

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = c.len();
        let v = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= n {
                s.push(T[(v >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// A quad (4 vertices, 6 u16 indices) with normals and UVs, in one buffer.
fn quad_buffer() -> Vec<u8> {
    let mut b = Vec::new();
    let pos: [[f32; 3]; 4] = [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 0.0],
    ];
    for p in pos.iter().flatten() {
        b.extend_from_slice(&p.to_le_bytes());
    } // 48 bytes
    for _ in 0..4 {
        for v in [0.0f32, 0.0, 1.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    } // +48 = 96
    for uv in [[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
        .iter()
        .flatten()
    {
        b.extend_from_slice(&uv.to_le_bytes());
    } // +32 = 128
    for i in [0u16, 1, 2, 0, 2, 3] {
        b.extend_from_slice(&i.to_le_bytes());
    } // +12 = 140
    b
}

fn quad_json(buffer_uri: Option<&str>, byte_length: usize) -> String {
    let uri = buffer_uri.map_or(String::new(), |u| format!(r#""uri": "{u}", "#));
    format!(
        r#"{{
  "asset": {{"version": "2.0"}},
  "extensionsUsed": ["KHR_materials_emissive_strength"],
  "scene": 0,
  "scenes": [{{"nodes": [0]}}],
  "nodes": [
    {{"name": "root", "translation": [1, 0, 0], "children": [1]}},
    {{"name": "quad", "mesh": 0, "scale": [2, 2, 2]}}
  ],
  "meshes": [{{"name": "Quad", "primitives": [{{
    "attributes": {{"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}},
    "indices": 3, "material": 0
  }}]}}],
  "materials": [{{
    "name": "Brass",
    "pbrMetallicRoughness": {{"baseColorFactor": [0.9, 0.7, 0.3, 1], "metallicFactor": 1, "roughnessFactor": 0.35}},
    "emissiveFactor": [1, 0.5, 0],
    "extensions": {{"KHR_materials_emissive_strength": {{"emissiveStrength": 4}}}},
    "alphaMode": "MASK", "alphaCutoff": 0.25, "doubleSided": true
  }}],
  "buffers": [{{{uri}"byteLength": {byte_length}}}],
  "bufferViews": [
    {{"buffer": 0, "byteOffset": 0, "byteLength": 48}},
    {{"buffer": 0, "byteOffset": 48, "byteLength": 48}},
    {{"buffer": 0, "byteOffset": 96, "byteLength": 32}},
    {{"buffer": 0, "byteOffset": 128, "byteLength": 12}}
  ],
  "accessors": [
    {{"bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [0,0,0], "max": [1,1,0]}},
    {{"bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3"}},
    {{"bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC2"}},
    {{"bufferView": 3, "componentType": 5123, "count": 6, "type": "SCALAR"}}
  ]
}}"#
    )
}

fn check_quad(scene: &engine_assets::ImportedScene) {
    assert_eq!(scene.meshes.len(), 1);
    let p = &scene.meshes[0].primitives[0];
    assert_eq!(p.indices, [0, 1, 2, 0, 2, 3]);
    assert_eq!(p.normals[2], [0.0, 0.0, 1.0]);
    assert_eq!(p.uvs[2], [1.0, 1.0]);
    assert_eq!((p.min, p.max), ([0.0; 3], [1.0, 1.0, 0.0]));
    assert_eq!(scene.triangle_count(), 2);
    let m = &scene.materials[0];
    assert_eq!(m.name, "Brass");
    assert_eq!(m.base_color, [0.9, 0.7, 0.3, 1.0]);
    assert_eq!((m.metallic, m.roughness), (1.0, 0.35));
    assert_eq!(m.emissive, [4.0, 2.0, 0.0], "emissive strength applied");
    assert_eq!(m.alpha, AlphaMode::Mask(0.25f32.to_bits()));
    assert!(m.double_sided);
    assert_eq!(scene.roots, [0]);
    assert_eq!(scene.nodes[0].children, [1]);
}

#[test]
fn gltf_with_a_data_uri() {
    let buf = quad_buffer();
    let json = quad_json(
        Some(&format!(
            "data:application/octet-stream;base64,{}",
            b64(&buf)
        )),
        buf.len(),
    );
    check_quad(&import_gltf(json.as_bytes(), None).unwrap());
}

#[test]
fn glb_container() {
    let buf = quad_buffer();
    let mut json = quad_json(None, buf.len()).into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut bin = buf.clone();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
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
    check_quad(&import_gltf(&glb, None).unwrap());
}

#[test]
fn external_buffer_and_path_safety() {
    let dir = std::env::temp_dir().join(format!("engine_assets_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let buf = quad_buffer();
    std::fs::write(dir.join("quad.bin"), &buf).unwrap();
    let json = quad_json(Some("quad.bin"), buf.len());
    check_quad(&import_gltf(json.as_bytes(), Some(&dir)).unwrap());
    assert!(matches!(
        import_gltf(json.as_bytes(), None),
        Err(ImportError::Buffer(_))
    ));
    let evil = quad_json(Some("../quad.bin"), buf.len());
    assert!(matches!(
        import_gltf(evil.as_bytes(), Some(&dir)),
        Err(ImportError::Buffer(_))
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_normals_and_indices_are_generated() {
    let mut buf = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
        .iter()
        .flatten()
    {
        buf.extend_from_slice(&p.to_le_bytes());
    }
    let json = format!(
        r#"{{"asset": {{"version": "2.0"}},
  "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}}}]}}],
  "nodes": [{{"mesh": 0}}],
  "buffers": [{{"uri": "data:application/octet-stream;base64,{}", "byteLength": 36}}],
  "bufferViews": [{{"buffer": 0, "byteLength": 36}}],
  "accessors": [{{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0,0,0], "max": [1,1,0]}}]}}"#,
        b64(&buf)
    );
    let scene = import_gltf(json.as_bytes(), None).unwrap();
    let p = &scene.meshes[0].primitives[0];
    assert_eq!(p.indices, [0, 1, 2]);
    assert!(p.normals.iter().all(|n| *n == [0.0, 0.0, 1.0]));
    assert_eq!(scene.roots, [0], "no scene: parentless nodes are roots");
    assert_eq!(p.triangle_vertices().count(), 3);
}

#[test]
fn out_of_range_indices_and_garbage_are_errors() {
    assert!(matches!(
        import_gltf(b"not gltf", None),
        Err(ImportError::Parse(_))
    ));
    let mut buf = quad_buffer();
    // Index 9 in a 4-vertex mesh.
    buf[128] = 9;
    let json = quad_json(
        Some(&format!(
            "data:application/octet-stream;base64,{}",
            b64(&buf)
        )),
        buf.len(),
    );
    assert!(matches!(
        import_gltf(json.as_bytes(), None),
        Err(ImportError::Invalid(_))
    ));
}

#[test]
fn hierarchy_spawns_into_the_ecs() {
    let buf = quad_buffer();
    let json = quad_json(
        Some(&format!(
            "data:application/octet-stream;base64,{}",
            b64(&buf)
        )),
        buf.len(),
    );
    let scene = import_gltf(json.as_bytes(), None).unwrap();
    let mut world = World::new();
    let mut prop = TransformPropagation::new(&mut world);
    let entities = spawn_scene(&mut world, &scene, None);
    prop.run(&mut world);
    assert_eq!(world.parent(entities[1]), Some(entities[0]));
    let g = world.get::<GlobalTransform>(entities[1]).unwrap();
    assert_eq!(g.translation().to_array(), [1.0, 0.0, 0.0]);
    let corner =
        g.0.transform_point3(engine_core::math::Vec3::new(1.0, 1.0, 0.0));
    assert_eq!(
        corner.to_array(),
        [3.0, 2.0, 0.0],
        "scale 2 then translate 1"
    );
    let mut q = QueryState::<&MeshInstance>::new(&mut world);
    assert_eq!(q.iter(&mut world).count(), 1);
}

#[test]
fn textures_samplers_and_material_maps() {
    use engine_assets::{TextureSource, TextureWrap, read_texture};
    // The "images" are opaque bytes here: import does not decode them.
    let embedded = b"embedded image".to_vec();
    let inline = b"inline image".to_vec();
    let json = format!(
        r#"{{
  "asset": {{"version": "2.0"}},
  "images": [
    {{"uri": "my%20image.png"}},
    {{"bufferView": 0, "mimeType": "image/png"}},
    {{"uri": "data:image/jpeg;base64,{}"}}
  ],
  "samplers": [{{"wrapS": 33071, "wrapT": 33648}}],
  "textures": [
    {{"source": 0}},
    {{"source": 1, "sampler": 0}},
    {{"source": 2}}
  ],
  "materials": [{{
    "pbrMetallicRoughness": {{"baseColorTexture": {{"index": 0}}, "metallicRoughnessTexture": {{"index": 1}}}},
    "normalTexture": {{"index": 2, "scale": 0.5}},
    "occlusionTexture": {{"index": 1, "strength": 0.7}},
    "emissiveTexture": {{"index": 0}}
  }}, {{}}],
  "buffers": [{{"uri": "data:application/octet-stream;base64,{}", "byteLength": {}}}],
  "bufferViews": [{{"buffer": 0, "byteLength": {}}}]
}}"#,
        b64(&inline),
        b64(&embedded),
        embedded.len(),
        embedded.len()
    );
    let dir = std::env::temp_dir().join(format!("engine_assets_tex_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("my image.png"), b"file image").unwrap();
    let scene = import_gltf(json.as_bytes(), Some(&dir)).unwrap();

    assert_eq!(scene.textures.len(), 3);
    assert_eq!(
        scene.textures[0].source,
        TextureSource::Uri(String::from("my image.png")),
        "URIs are percent-decoded"
    );
    assert_eq!(
        scene.textures[0].wrap,
        [TextureWrap::Repeat; 2],
        "default sampler"
    );
    assert_eq!(
        scene.textures[1].wrap,
        [TextureWrap::ClampToEdge, TextureWrap::MirroredRepeat]
    );
    assert_eq!(
        scene.textures[1].source,
        TextureSource::Embedded {
            bytes: embedded.clone(),
            mime: String::from("image/png")
        }
    );
    assert_eq!(
        scene.textures[2].source,
        TextureSource::Embedded {
            bytes: inline.clone(),
            mime: String::from("image/jpeg")
        }
    );
    let read = |i: usize| read_texture(&scene.textures[i].source, Some(&dir)).unwrap();
    assert_eq!(read(0), b"file image");
    assert_eq!(read(1), embedded);
    assert_eq!(read(2), inline);
    assert!(matches!(
        read_texture(&TextureSource::Uri(String::from("../x.png")), Some(&dir)),
        Err(ImportError::Buffer(_))
    ));

    let m = &scene.materials[0];
    assert_eq!(
        (
            m.base_color_texture,
            m.metallic_roughness_texture,
            m.normal_texture
        ),
        (Some(0), Some(1), Some(2))
    );
    assert_eq!((m.normal_scale, m.occlusion_strength), (0.5, 0.7));
    assert_eq!(
        (m.occlusion_texture, m.emissive_texture),
        (Some(1), Some(0))
    );
    // glTF defaults.
    let d = &scene.materials[1];
    assert_eq!(d.base_color, [1.0; 4]);
    assert_eq!((d.metallic, d.roughness), (1.0, 1.0));
    assert_eq!((d.normal_scale, d.occlusion_strength), (1.0, 1.0));
    assert_eq!((d.alpha, d.double_sided), (AlphaMode::Opaque, false));
    std::fs::remove_dir_all(&dir).unwrap();
}
