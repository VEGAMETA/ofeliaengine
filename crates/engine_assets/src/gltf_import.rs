//! glTF 2.0 import into memory (ADR 0033).
//!
//! - **Input:** `.gltf` (JSON with external or data-URI buffers) or `.glb`.
//! - **Output:** an [`ImportedScene`]:
//!   - meshes as indexed triangle lists with positions, normals (computed when
//!     missing), UVs and tangents;
//!   - metallic-roughness PBR materials;
//!   - the node hierarchy with local transforms;
//!   - textures with their wrap modes: file names, or the bytes of images embedded in
//!     the file. [`read_texture`] loads a texture's encoded image; decoding and mip
//!     levels are [`crate::decode_image`] and [`crate::build_mips`].
//! - [`spawn_scene`] turns the hierarchy into ECS entities with `Transform`,
//!   `GlobalTransform`, parent links and a [`MeshInstance`] per mesh-bearing node.

use engine_core::math::{Quat, Vec3};
use engine_ecs::{Entity, GlobalTransform, Transform, World, pod_component};
use std::path::Path;

/// One drawable part of a mesh: an indexed triangle list with one material.
#[derive(Debug, Clone, PartialEq)]
pub struct Primitive {
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Vertex normals (computed as area-weighted smooth normals when the file has none).
    pub normals: Vec<[f32; 3]>,
    /// First texture coordinate set (empty if absent).
    pub uvs: Vec<[f32; 2]>,
    /// Tangents with handedness in w (empty if absent).
    pub tangents: Vec<[f32; 4]>,
    /// Triangle indices (generated when the file has none).
    pub indices: Vec<u32>,
    /// Index into [`ImportedScene::materials`].
    pub material: Option<usize>,
    /// Bounding box: minimum corner.
    pub min: [f32; 3],
    /// Bounding box: maximum corner.
    pub max: [f32; 3],
}

impl Primitive {
    /// The triangles as (position, normal) triples, unindexed (for renderers that pull
    /// vertices without an index buffer).
    pub fn triangle_vertices(&self) -> impl Iterator<Item = ([f32; 3], [f32; 3])> + '_ {
        self.indices
            .iter()
            .map(|&i| (self.positions[i as usize], self.normals[i as usize]))
    }
}

/// A mesh: its primitives.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshData {
    /// Name from the file (may be empty).
    pub name: String,
    /// Parts, one per material.
    pub primitives: Vec<Primitive>,
}

/// How a material's alpha is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaMode {
    /// Fully opaque.
    Opaque,
    /// Cut out below the threshold.
    Mask(u32),
    /// Blended.
    Blend,
}

/// A metallic-roughness PBR material. Textures are indices into
/// [`ImportedScene::textures`]; all of them use the first set of texture coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialData {
    /// Name from the file.
    pub name: String,
    /// Linear RGBA base color factor.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Roughness factor.
    pub roughness: f32,
    /// Linear emissive color (with `KHR_materials_emissive_strength` applied).
    pub emissive: [f32; 3],
    /// Base color texture (sRGB color, alpha).
    pub base_color_texture: Option<usize>,
    /// Metallic (blue) and roughness (green) texture.
    pub metallic_roughness_texture: Option<usize>,
    /// Tangent-space normal map.
    pub normal_texture: Option<usize>,
    /// Scale of the normal map's X and Y.
    pub normal_scale: f32,
    /// Ambient occlusion texture (red).
    pub occlusion_texture: Option<usize>,
    /// How much of the occlusion applies, 0 … 1.
    pub occlusion_strength: f32,
    /// Emissive texture (sRGB color), multiplied by `emissive`.
    pub emissive_texture: Option<usize>,
    /// Alpha usage (the mask cutoff is stored as `f32::to_bits`).
    pub alpha: AlphaMode,
    /// Render both faces.
    pub double_sided: bool,
    /// Thickness of the volume under the surface in the mesh's units
    /// (`KHR_materials_volume`'s `thicknessFactor`; 0 without the extension), for
    /// transmission through subsurface-scattering materials (ADR 0059).
    pub thickness: f32,
    /// Thickness texture (green), multiplied by `thickness` (`thicknessTexture`).
    pub thickness_texture: Option<usize>,
}

/// Where a texture's encoded image is (decoded later by the texture pipeline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextureSource {
    /// A file next to the glTF: its relative path, percent-decoded.
    Uri(String),
    /// An image inside the file (a buffer view or a data URI).
    Embedded {
        /// The encoded image.
        bytes: Vec<u8>,
        /// MIME type (`image/png`, `image/jpeg`, `image/ktx2`...), empty if the file
        /// does not say.
        mime: String,
    },
}

/// How a texture repeats along one axis (glTF's sampler wrap modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextureWrap {
    /// Tiles.
    #[default]
    Repeat,
    /// Tiles, every other copy mirrored.
    MirroredRepeat,
    /// Stretches the edge texels.
    ClampToEdge,
}

/// A texture: an image and how it repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureData {
    /// The encoded image.
    pub source: TextureSource,
    /// Wrap along u and v.
    pub wrap: [TextureWrap; 2],
}

/// A node: a local transform, an optional mesh and children.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeData {
    /// Name from the file.
    pub name: String,
    /// Local translation.
    pub translation: [f32; 3],
    /// Local rotation (x, y, z, w).
    pub rotation: [f32; 4],
    /// Local scale.
    pub scale: [f32; 3],
    /// Index into [`ImportedScene::meshes`].
    pub mesh: Option<usize>,
    /// Indices into [`ImportedScene::nodes`].
    pub children: Vec<usize>,
}

/// Everything imported from one glTF file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportedScene {
    /// Meshes.
    pub meshes: Vec<MeshData>,
    /// Materials.
    pub materials: Vec<MaterialData>,
    /// Textures.
    pub textures: Vec<TextureData>,
    /// All nodes.
    pub nodes: Vec<NodeData>,
    /// Root nodes of the default scene (or of the first scene).
    pub roots: Vec<usize>,
}

impl ImportedScene {
    /// Total triangles over all meshes.
    pub fn triangle_count(&self) -> usize {
        self.meshes
            .iter()
            .flat_map(|m| &m.primitives)
            .map(|p| p.indices.len() / 3)
            .sum()
    }
}

/// Errors from importing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// Not valid glTF or GLB.
    Parse(String),
    /// A buffer could not be loaded.
    Buffer(String),
    /// Data that violates the spec (out-of-range index, missing positions...).
    Invalid(String),
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse(m) => write!(f, "not a valid glTF file: {m}"),
            Self::Buffer(m) => write!(f, "cannot load a buffer: {m}"),
            Self::Invalid(m) => write!(f, "invalid glTF data: {m}"),
        }
    }
}

impl std::error::Error for ImportError {}

/// Decodes standard base64 (as used by data URIs), ignoring nothing: any invalid
/// character fails.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let bytes = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let n = chunk.len();
        if n < 2 {
            return None;
        }
        out.push((acc >> 16) as u8);
        if n > 2 {
            out.push((acc >> 8) as u8);
        }
        if n > 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

/// Decodes `%XX` escapes of a URI (glTF URIs are RFC 3986: a space is `%20`). Invalid
/// escapes stay as they are.
fn percent_decode(uri: &str) -> String {
    let bytes = uri.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| uri.to_owned())
}

/// The bytes of a base64 data URI (`data:<mime>;base64,<data>`) and its MIME type.
fn data_uri(uri: &str) -> Option<Result<(Vec<u8>, String), ImportError>> {
    let rest = uri.strip_prefix("data:")?;
    Some(match rest.split_once(";base64,") {
        Some((mime, b64)) => base64_decode(b64)
            .map(|bytes| (bytes, mime.to_owned()))
            .ok_or_else(|| ImportError::Buffer("bad base64 in data URI".into())),
        None => Err(ImportError::Buffer("only base64 data URIs".into())),
    })
}

/// A file next to the asset: `uri` must stay inside `dir`.
fn read_relative(dir: Option<&Path>, uri: &str) -> Result<Vec<u8>, ImportError> {
    let dir = dir.ok_or_else(|| {
        ImportError::Buffer(format!("`{uri}` is external but no folder was given"))
    })?;
    if uri.contains("..") || Path::new(uri).is_absolute() {
        return Err(ImportError::Buffer(format!(
            "`{uri}` points outside the asset's folder"
        )));
    }
    std::fs::read(dir.join(uri)).map_err(|e| ImportError::Buffer(format!("`{uri}`: {e}")))
}

/// The encoded image of a texture: embedded bytes, or the file next to the glTF
/// (`base_dir` is the glTF's folder; paths leaving it are refused).
pub fn read_texture(
    source: &TextureSource,
    base_dir: Option<&Path>,
) -> Result<Vec<u8>, ImportError> {
    match source {
        TextureSource::Embedded { bytes, .. } => Ok(bytes.clone()),
        TextureSource::Uri(uri) => read_relative(base_dir, uri),
    }
}

fn load_buffers(
    doc: &gltf::Document,
    blob: Option<Vec<u8>>,
    base_dir: Option<&Path>,
) -> Result<Vec<Vec<u8>>, ImportError> {
    let mut blob = blob;
    let mut out = Vec::new();
    for buffer in doc.buffers() {
        let data = match buffer.source() {
            gltf::buffer::Source::Bin => blob
                .take()
                .ok_or_else(|| ImportError::Buffer("GLB binary chunk missing".into()))?,
            gltf::buffer::Source::Uri(uri) => match data_uri(uri) {
                Some(data) => data?.0,
                None => read_relative(base_dir, &percent_decode(uri))?,
            },
        };
        if data.len() < buffer.length() {
            return Err(ImportError::Buffer(format!(
                "buffer {} has {} bytes, the file says {}",
                buffer.index(),
                data.len(),
                buffer.length()
            )));
        }
        out.push(data);
    }
    Ok(out)
}

/// Area-weighted smooth normals for an indexed triangle list.
fn smooth_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![Vec3::ZERO; positions.len()];
    for t in indices.chunks_exact(3) {
        let [a, b, c] = [t[0] as usize, t[1] as usize, t[2] as usize];
        let (pa, pb, pc) = (
            Vec3::from(positions[a]),
            Vec3::from(positions[b]),
            Vec3::from(positions[c]),
        );
        let n = (pb - pa).cross(pc - pa); // length = 2 × area
        acc[a] += n;
        acc[b] += n;
        acc[c] += n;
    }
    acc.into_iter()
        .map(|n| n.try_normalize().unwrap_or(Vec3::Y).to_array())
        .collect()
}

/// Imports a glTF 2.0 file from memory. `base_dir` resolves external buffer files.
pub fn import_gltf(bytes: &[u8], base_dir: Option<&Path>) -> Result<ImportedScene, ImportError> {
    engine_profiler::zone!("assets::import_gltf");
    let gltf::Gltf { document, blob } =
        gltf::Gltf::from_slice(bytes).map_err(|e| ImportError::Parse(e.to_string()))?;
    let buffers = load_buffers(&document, blob, base_dir)?;
    let mut scene = ImportedScene::default();

    for texture in document.textures() {
        let source = match texture.source().source() {
            gltf::image::Source::Uri { uri, mime_type } => match data_uri(uri) {
                Some(data) => {
                    let (bytes, mime) = data?;
                    TextureSource::Embedded {
                        bytes,
                        mime: mime_type.map_or(mime, str::to_owned),
                    }
                }
                None => TextureSource::Uri(percent_decode(uri)),
            },
            gltf::image::Source::View { view, mime_type } => {
                let buffer = &buffers[view.buffer().index()];
                let bytes = buffer
                    .get(view.offset()..view.offset() + view.length())
                    .ok_or_else(|| {
                        ImportError::Invalid(format!(
                            "image view {} lies outside its buffer",
                            view.index()
                        ))
                    })?;
                TextureSource::Embedded {
                    bytes: bytes.to_vec(),
                    mime: mime_type.to_owned(),
                }
            }
        };
        let wrap = |mode| match mode {
            gltf::texture::WrappingMode::Repeat => TextureWrap::Repeat,
            gltf::texture::WrappingMode::MirroredRepeat => TextureWrap::MirroredRepeat,
            gltf::texture::WrappingMode::ClampToEdge => TextureWrap::ClampToEdge,
        };
        let sampler = texture.sampler();
        scene.textures.push(TextureData {
            source,
            wrap: [wrap(sampler.wrap_s()), wrap(sampler.wrap_t())],
        });
    }

    for m in document.materials() {
        let pbr = m.pbr_metallic_roughness();
        let strength = m.emissive_strength().unwrap_or(1.0);
        let e = m.emissive_factor();
        scene.materials.push(MaterialData {
            name: m.name().unwrap_or_default().to_owned(),
            base_color: pbr.base_color_factor(),
            metallic: pbr.metallic_factor(),
            roughness: pbr.roughness_factor(),
            emissive: [e[0] * strength, e[1] * strength, e[2] * strength],
            base_color_texture: pbr.base_color_texture().map(|t| t.texture().index()),
            metallic_roughness_texture: pbr
                .metallic_roughness_texture()
                .map(|t| t.texture().index()),
            normal_texture: m.normal_texture().map(|t| t.texture().index()),
            normal_scale: m.normal_texture().map_or(1.0, |t| t.scale()),
            occlusion_texture: m.occlusion_texture().map(|t| t.texture().index()),
            occlusion_strength: m.occlusion_texture().map_or(1.0, |t| t.strength()),
            emissive_texture: m.emissive_texture().map(|t| t.texture().index()),
            alpha: match m.alpha_mode() {
                gltf::material::AlphaMode::Opaque => AlphaMode::Opaque,
                gltf::material::AlphaMode::Mask => {
                    AlphaMode::Mask(m.alpha_cutoff().unwrap_or(0.5).to_bits())
                }
                gltf::material::AlphaMode::Blend => AlphaMode::Blend,
            },
            double_sided: m.double_sided(),
            thickness: m.volume().map_or(0.0, |v| v.thickness_factor()),
            thickness_texture: m
                .volume()
                .and_then(|v| v.thickness_texture())
                .map(|t| t.texture().index()),
        });
    }

    for mesh in document.meshes() {
        let mut primitives = Vec::new();
        for p in mesh.primitives() {
            if p.mode() != gltf::mesh::Mode::Triangles {
                // Points, lines and strips are not rendered as meshes; skip them.
                continue;
            }
            let reader = p.reader(|b| buffers.get(b.index()).map(Vec::as_slice));
            let positions: Vec<[f32; 3]> = reader
                .read_positions()
                .ok_or_else(|| {
                    ImportError::Invalid(format!(
                        "mesh `{}` has a primitive without positions",
                        mesh.name().unwrap_or("")
                    ))
                })?
                .collect();
            let indices: Vec<u32> = match reader.read_indices() {
                Some(i) => i.into_u32().collect(),
                None => (0..positions.len() as u32).collect(),
            };
            if !indices.len().is_multiple_of(3) {
                return Err(ImportError::Invalid(
                    "index count not a multiple of 3".into(),
                ));
            }
            if let Some(&bad) = indices.iter().find(|&&i| i as usize >= positions.len()) {
                return Err(ImportError::Invalid(format!(
                    "index {bad} is out of range ({} vertices)",
                    positions.len()
                )));
            }
            let normals = match reader.read_normals() {
                Some(n) => n.collect::<Vec<_>>(),
                None => smooth_normals(&positions, &indices),
            };
            let uvs = reader
                .read_tex_coords(0)
                .map(|t| t.into_f32().collect())
                .unwrap_or_default();
            let tangents = reader
                .read_tangents()
                .map(Iterator::collect)
                .unwrap_or_default();
            let bounds = p.bounding_box();
            primitives.push(Primitive {
                positions,
                normals,
                uvs,
                tangents,
                indices,
                material: p.material().index(),
                min: bounds.min,
                max: bounds.max,
            });
        }
        scene.meshes.push(MeshData {
            name: mesh.name().unwrap_or_default().to_owned(),
            primitives,
        });
    }

    for node in document.nodes() {
        let (translation, rotation, scale) = node.transform().decomposed();
        scene.nodes.push(NodeData {
            name: node.name().unwrap_or_default().to_owned(),
            translation,
            rotation,
            scale,
            mesh: node.mesh().map(|m| m.index()),
            children: node.children().map(|c| c.index()).collect(),
        });
    }
    let root_scene = document
        .default_scene()
        .or_else(|| document.scenes().next());
    scene.roots = match root_scene {
        Some(s) => s.nodes().map(|n| n.index()).collect(),
        None => {
            // No scene: every node that is nobody's child is a root.
            let mut is_child = vec![false; scene.nodes.len()];
            for n in &scene.nodes {
                for &c in &n.children {
                    is_child[c] = true;
                }
            }
            (0..scene.nodes.len()).filter(|&i| !is_child[i]).collect()
        }
    };
    Ok(scene)
}

/// Marks an entity that draws a mesh of an imported scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct MeshInstance {
    /// Index into [`ImportedScene::meshes`] (resolved to GPU meshes by the renderer).
    pub mesh: u32,
}
pod_component!(MeshInstance);

/// Spawns the scene's node hierarchy under an optional `parent`: one entity per node with
/// `Transform`, `GlobalTransform`, parent links, and a [`MeshInstance`] when the node has
/// a mesh. Returns the entities in node order. Cycles in malformed files are cut.
pub fn spawn_scene(
    world: &mut World,
    scene: &ImportedScene,
    parent: Option<Entity>,
) -> Vec<Entity> {
    let entities: Vec<Entity> = scene
        .nodes
        .iter()
        .map(|n| {
            let t = Transform::from_translation(Vec3::from(n.translation))
                .with_rotation(Quat::from_array(n.rotation))
                .with_scale(Vec3::from(n.scale));
            let e = world.spawn((t, GlobalTransform::IDENTITY));
            if let Some(m) = n.mesh {
                world.insert(e, MeshInstance { mesh: m as u32 });
            }
            e
        })
        .collect();
    let mut visited = vec![false; scene.nodes.len()];
    let mut stack: Vec<(usize, Option<Entity>)> =
        scene.roots.iter().rev().map(|&r| (r, parent)).collect();
    while let Some((i, p)) = stack.pop() {
        if i >= entities.len() || core::mem::replace(&mut visited[i], true) {
            continue;
        }
        if let Some(p) = p {
            let _ = world.set_parent(entities[i], Some(p));
        }
        for &c in scene.nodes[i].children.iter().rev() {
            stack.push((c, Some(entities[i])));
        }
    }
    entities
}
