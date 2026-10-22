//! Colliders cooked from a scene (ADR 0101).
//!
//! **Where bodies come from**, in this order:
//!
//! 1. **`KHR_physics_rigid_bodies`.** A file that uses it says what collides: a node with a
//!    `motion` is a body with the colliders of its subtree (down to nodes with motions of
//!    their own); a collider elsewhere is a static body; a trigger is a sensor. Nothing
//!    else in such a file collides.
//! 2. **Authored collision meshes** (`UCX_`, `UBX_`, `USP_`, `UCP_` + a node's or mesh's
//!    name, Unreal's convention): the named node's collider, made of their hulls and fitted
//!    primitives. They are not drawn. One that names nothing is a static collider of its
//!    own.
//! 3. **The rules** ([`ColliderRules`], `<scene>.colliders.ron` beside the glTF) for every
//!    other mesh node: by default static level geometry, its glTF mesh's triangles exactly
//!    as drawn (all primitives in one shape, each triangle's surface its primitive's
//!    material); a dynamic prop gets a fitted box, sphere, capsule or cylinder within 10 %
//!    of its hull's volume, else its hull (at most 64 points), when it is convex enough; a
//!    concave one gets convex parts. Skinned meshes get nothing. A mesh's own rules
//!    ([`ColliderRules::meshes`]) say where it differs.
//!
//! **Surfaces** (what a ray hit is made of, `PhysicsBackend::surface_of`) are glTF
//! material indices: a level mesh's triangles carry their primitives' materials; a prop's
//! shapes, and authored colliders, the main material of their mesh (the one with the most
//! triangles). **Layers** are names ([`CookedBody::layer`]): the rules', or a glTF
//! collider's collision filter's first system; the game's physics settings number them.
//!
//! **Convex parts** (`engine_collision::decompose`, CoACD's algorithm): a simulated body
//! cannot be a triangle mesh, so a dynamic prop's concave mesh, and a dynamic glTF physics
//! body's triangle mesh collider, become convex parts. They are computed before the
//! colliders are built, for every such mesh in parallel, and kept in the derived data cache
//! under a key of the mesh and the settings: a second cook loads them.
//!
//! **Sharing and placement.** A collider is in its body's object space and shared by every
//! body using the same mesh; the body keeps its node's full transform and the runtime
//! places it at any scale (`engine_physics::colliders::placed`). A transform with shear
//! (a non-uniform scale above a rotation) is baked into a collider of its own here, so the
//! runtime never has to rebuild a mesh.

use crate::ddc::DerivedDataCache;
use crate::gltf_import::ImportedScene;
use crate::gltf_physics::{AuthoredKind, Geometry, ImplicitShape, collision_name};
use crate::pack::{EntryData, EntryKind, Pack, PackError, invalid};
use engine_collision::decompose::{self, DecomposeSettings};
use engine_collision::fit;
use engine_collision::hull::{Hull, HullSettings};
use engine_collision::mesh::weld_vertices;
use engine_collision::{Collider, ColliderShape, Origin, Shape};
use engine_core::hash::{StableHasher, content_hash128};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat3, Mat4, Quat, Vec3};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What moves a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Motion {
    /// Never moves (level geometry).
    #[default]
    Static,
    /// Moved by the game; pushes dynamic bodies.
    Kinematic,
    /// Moved by the simulation.
    Dynamic,
}

/// What a mesh node without glTF physics becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultBodies {
    /// A static body: level geometry.
    #[default]
    Static,
    /// A dynamic body: a prop.
    Dynamic,
    /// Nothing.
    None,
}

/// A static mesh's collider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaticShape {
    /// Its triangles, exactly as drawn.
    #[default]
    Mesh,
    /// None: it does not collide.
    None,
}

/// A dynamic mesh's collider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DynamicShape {
    /// A concave mesh: its convex parts ([`ColliderRules::parts`]). One convex enough to
    /// be one part: a box, sphere, capsule or cylinder within 10 % of its hull's volume,
    /// else its hull.
    #[default]
    Auto,
    /// A fitted box.
    Box,
    /// A fitted sphere.
    Sphere,
    /// A fitted capsule.
    Capsule,
    /// Its convex hull.
    Hull,
    /// Its convex parts ([`ColliderRules::parts`]), even where a primitive would do.
    Parts,
}

/// How a scene's meshes collide, where the file does not say (ADR 0101).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ColliderRules {
    /// What a mesh node without glTF physics becomes.
    pub bodies: DefaultBodies,
    /// A static mesh's collider.
    pub static_shape: StaticShape,
    /// A dynamic mesh's collider.
    pub dynamic_shape: DynamicShape,
    /// glTF materials (by name) whose triangles collide with nothing: foliage cards,
    /// decals. Nothing is guessed: a masked material may be a grating one walks on.
    pub no_collision: Vec<String>,
    /// How concave meshes of simulated bodies are split into convex parts: CoACD's
    /// concavity threshold (0.05 of the mesh's size), as many parts as it needs.
    pub parts: DecomposeSettings,
    /// The collision layer of the bodies these rules make, by name (the game's
    /// `PhysicsSettings::layer_names`); none: by their motion.
    pub layer: Option<String>,
    /// Per glTF mesh, by its name or `#index` (`#3`: the fourth): where its rules differ.
    pub meshes: BTreeMap<String, MeshRules>,
}

/// One glTF mesh's rules: each field it sets replaces [`ColliderRules`]'s.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MeshRules {
    /// What its nodes become.
    pub bodies: Option<DefaultBodies>,
    /// Its collider as level geometry.
    pub static_shape: Option<StaticShape>,
    /// Its collider as a prop.
    pub dynamic_shape: Option<DynamicShape>,
    /// Its bodies' collision layer, by name.
    pub layer: Option<String>,
}

/// One mesh's rules, resolved.
struct Resolved<'a> {
    bodies: DefaultBodies,
    static_shape: StaticShape,
    dynamic_shape: DynamicShape,
    layer: Option<&'a str>,
}

impl ColliderRules {
    /// The rules' path for a scene: `scene.gltf` → `scene.colliders.ron`.
    pub fn path_for(scene: &Path) -> PathBuf {
        scene.with_extension("colliders.ron")
    }

    /// Parses a rules file (see the dev guide's physics page): every field optional.
    pub fn parse(text: &str) -> Result<Self, ColliderRulesError> {
        let file: rules_file::File = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(text)
            .map_err(|e| ColliderRulesError {
                path: None,
                message: format!("{}:{}: {}", e.span.start.line, e.span.start.col, e.code),
            })?;
        Ok(file.into_rules())
    }

    /// The rules next to `scene`, or the defaults when there are none.
    pub fn for_scene(scene: &Path) -> Result<Self, ColliderRulesError> {
        let path = Self::path_for(scene);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(ColliderRulesError {
                    path: Some(path),
                    message: e.to_string(),
                });
            }
        };
        Self::parse(&text).map_err(|e| ColliderRulesError {
            path: Some(path),
            message: e.message,
        })
    }

    /// Mesh `mesh`'s rules: its own where it has them, by name, then by `#index`.
    fn resolve(&self, scene: &ImportedScene, mesh: usize) -> Resolved<'_> {
        let own = scene
            .meshes
            .get(mesh)
            .and_then(|m| self.meshes.get(&m.name).filter(|_| !m.name.is_empty()))
            .or_else(|| self.meshes.get(&format!("#{mesh}")));
        Resolved {
            bodies: own.and_then(|o| o.bodies).unwrap_or(self.bodies),
            static_shape: own
                .and_then(|o| o.static_shape)
                .unwrap_or(self.static_shape),
            dynamic_shape: own
                .and_then(|o| o.dynamic_shape)
                .unwrap_or(self.dynamic_shape),
            layer: own
                .and_then(|o| o.layer.as_deref())
                .or(self.layer.as_deref()),
        }
    }
}

/// A rules file that does not parse or cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColliderRulesError {
    /// The file, when it was read from one.
    pub path: Option<PathBuf>,
    /// What is wrong (with its line and column).
    pub message: String,
}

impl core::fmt::Display for ColliderRulesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ColliderRulesError {}

/// `<scene>.colliders.ron`'s form: what an artist writes. "No collision" is `collides:
/// false` rather than a variant named `None` (which RON reads as a missing value).
mod rules_file {
    use super::{ColliderRules, DefaultBodies, DynamicShape, MeshRules, StaticShape};
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, Copy, Deserialize)]
    pub(super) enum Bodies {
        Static,
        Dynamic,
    }

    #[derive(Debug, Clone, Copy, Deserialize)]
    pub(super) enum Dynamic {
        Auto,
        Box,
        Sphere,
        Capsule,
        Hull,
        Parts,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub(super) struct Parts {
        threshold: Option<f64>,
        max_parts: Option<usize>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub(super) struct Mesh {
        bodies: Option<Bodies>,
        collides: Option<bool>,
        dynamic_shape: Option<Dynamic>,
        layer: Option<String>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub(super) struct File {
        bodies: Option<Bodies>,
        collides: Option<bool>,
        dynamic_shape: Option<Dynamic>,
        parts: Option<Parts>,
        no_collision: Vec<String>,
        layer: Option<String>,
        meshes: BTreeMap<String, Mesh>,
    }

    fn bodies(b: Option<Bodies>, collides: Option<bool>) -> Option<DefaultBodies> {
        match (b, collides) {
            (_, Some(false)) => Some(DefaultBodies::None),
            (Some(Bodies::Static), _) => Some(DefaultBodies::Static),
            (Some(Bodies::Dynamic), _) => Some(DefaultBodies::Dynamic),
            (None, _) => None,
        }
    }

    fn dynamic(d: Dynamic) -> DynamicShape {
        match d {
            Dynamic::Auto => DynamicShape::Auto,
            Dynamic::Box => DynamicShape::Box,
            Dynamic::Sphere => DynamicShape::Sphere,
            Dynamic::Capsule => DynamicShape::Capsule,
            Dynamic::Hull => DynamicShape::Hull,
            Dynamic::Parts => DynamicShape::Parts,
        }
    }

    impl File {
        pub(super) fn into_rules(self) -> ColliderRules {
            let mut rules = ColliderRules {
                no_collision: self.no_collision,
                layer: self.layer,
                ..ColliderRules::default()
            };
            if let Some(b) = bodies(self.bodies, self.collides) {
                rules.bodies = b;
            } else if self.collides == Some(true) {
                rules.bodies = DefaultBodies::Static;
            }
            if let Some(d) = self.dynamic_shape {
                rules.dynamic_shape = dynamic(d);
            }
            if let Some(p) = self.parts {
                if let Some(t) = p.threshold {
                    rules.parts.threshold = t;
                }
                rules.parts.max_hulls = p.max_parts.or(rules.parts.max_hulls);
            }
            rules.meshes = self
                .meshes
                .into_iter()
                .map(|(name, m)| {
                    // A mesh told to collide without saying how: as level geometry.
                    let bodies = bodies(m.bodies, m.collides)
                        .or_else(|| (m.collides == Some(true)).then_some(DefaultBodies::Static));
                    (
                        name,
                        MeshRules {
                            bodies,
                            static_shape: bodies
                                .filter(|&b| b == DefaultBodies::Static)
                                .map(|_| StaticShape::Mesh),
                            dynamic_shape: m.dynamic_shape.map(dynamic),
                            layer: m.layer,
                        },
                    )
                })
                .collect();
            rules
        }
    }
}

/// What [`cook_colliders`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColliderStats {
    /// Meshes split into convex parts.
    pub decomposed: usize,
    /// Of those, the ones whose parts came from the derived data cache.
    pub cached: usize,
}

/// A collider in a physics backend's binary form (`engine_physics::colliders::bake`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakedCollider {
    /// The backend's format key (`PhysicsBackend::baked_shape_format`).
    pub format: u64,
    /// Its bytes.
    pub bytes: Vec<u8>,
    /// Whether it can be swept in shape casts (no triangle mesh in it).
    pub castable: bool,
}

/// A collider of a scene, shared by its bodies.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CookedCollider {
    /// The engine's own form.
    pub collider: Collider,
    /// The backend's form, once baked (the cook tool bakes before writing the pack).
    pub baked: Option<BakedCollider>,
}

/// A body of a scene.
#[derive(Debug, Clone, PartialEq)]
pub struct CookedBody {
    /// Index into [`crate::CookedScene::colliders`].
    pub collider: u32,
    /// Its node's object to world (column major), scale and all: the collider is in this
    /// object space.
    pub world_from_object: [f32; 16],
    /// The render instances it moves ([`crate::CookedScene::instances`]).
    pub instances: Vec<u32>,
    /// What moves it.
    pub motion: Motion,
    /// Reports overlaps but does not collide.
    pub sensor: bool,
    /// Friction.
    pub friction: f32,
    /// Restitution.
    pub restitution: f32,
    /// Mass in kg; none: from the colliders' volume.
    pub mass: Option<f32>,
    /// Initial velocity (world space, m/s).
    pub linear_velocity: [f32; 3],
    /// Initial angular velocity (world space, rad/s).
    pub angular_velocity: [f32; 3],
    /// How much gravity acts on it.
    pub gravity_factor: f32,
    /// Its collision layer by name (the game's `PhysicsSettings::layer_names`); none: by
    /// its motion.
    pub layer: Option<String>,
}

impl Default for CookedBody {
    fn default() -> Self {
        Self {
            collider: 0,
            world_from_object: Mat4::IDENTITY.to_cols_array(),
            instances: Vec::new(),
            motion: Motion::Static,
            sensor: false,
            friction: 0.5,
            restitution: 0.0,
            mass: None,
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            gravity_factor: 1.0,
            layer: None,
        }
    }
}

/// Whether a node is an authored collision mesh (not drawn).
pub fn is_collision_node(name: &str) -> bool {
    collision_name(name).is_some()
}

/// Points sampled on round shapes that become hulls (tapered capsules and cylinders),
/// relative to their size.
const ROUND_TOLERANCE: f32 = 0.005;
/// A plane's thickness below its surface, and the extent of an infinite one.
const PLANE_THICKNESS: f32 = 0.1;
const PLANE_EXTENT: f32 = 2000.0;
/// Hulls are limited to this many points (Jolt takes at most 256).
const HULL_POINTS: usize = 64;

/// The world transform of every node (column major), with its parent.
fn world_transforms(scene: &ImportedScene) -> (Vec<Mat4>, Vec<Option<usize>>) {
    let n = scene.nodes.len();
    let mut world = vec![Mat4::IDENTITY; n];
    let mut parent = vec![None; n];
    let mut stack: Vec<(usize, Mat4)> = scene.roots.iter().map(|&r| (r, Mat4::IDENTITY)).collect();
    let mut seen = vec![false; n];
    while let Some((i, p)) = stack.pop() {
        if i >= n || seen[i] {
            continue;
        }
        seen[i] = true;
        let node = &scene.nodes[i];
        world[i] = p * Mat4::from_scale_rotation_translation(
            Vec3::from(node.scale),
            Quat::from_array(node.rotation),
            Vec3::from(node.translation),
        );
        for &c in &node.children {
            if c < n {
                parent[c] = Some(i);
                stack.push((c, world[i]));
            }
        }
    }
    (world, parent)
}

/// A node's local transform (for nodes outside the scene, referenced as geometry).
fn local(scene: &ImportedScene, i: usize) -> Mat4 {
    let node = &scene.nodes[i];
    Mat4::from_scale_rotation_translation(
        Vec3::from(node.scale),
        Quat::from_array(node.rotation),
        Vec3::from(node.translation),
    )
}

fn affine(m: Mat4) -> Affine3A {
    Affine3A::from_mat4(m)
}

/// The triangles of a glTF mesh, all primitives together, welded, with each triangle's
/// surface its material (`u16::MAX`: none), without the materials that do not collide.
fn mesh_triangles(
    scene: &ImportedScene,
    mesh: usize,
    rules: &ColliderRules,
    into: Mat4,
) -> Option<Shape> {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    let mut surfaces = Vec::new();
    let flip = Mat3::from_mat4(into).determinant() < 0.0;
    for p in &scene.meshes[mesh].primitives {
        let excluded = p
            .material
            .and_then(|m| scene.materials.get(m))
            .is_some_and(|m| rules.no_collision.contains(&m.name));
        if excluded || p.positions.is_empty() {
            continue;
        }
        let base = vertices.len() as u32;
        vertices.extend(
            p.positions
                .iter()
                .map(|&q| into.transform_point3(Vec3::from(q))),
        );
        let surface = p
            .material
            .map_or(u16::MAX, |m| m.min(u16::MAX as usize - 1) as u16);
        for t in p.indices.chunks_exact(3) {
            let [a, b, c] = [t[0] + base, t[1] + base, t[2] + base];
            triangles.push(if flip { [a, c, b] } else { [a, b, c] });
            surfaces.push(surface);
        }
    }
    if triangles.is_empty() {
        return None;
    }
    let (lo, hi) = vertices.iter().fold(
        (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
        |(lo, hi), &v| (lo.min(v), hi.max(v)),
    );
    let size = (hi - lo).max_element().max(1e-6);
    // Render meshes split vertices at seams; colliders join them. Triangles that collapse
    // go, and their surfaces with them.
    let (vertices, remap) = weld_vertices(&vertices, size * 1e-6);
    let mut kept_triangles = Vec::with_capacity(triangles.len());
    let mut kept_surfaces = Vec::with_capacity(triangles.len());
    for (t, surface) in triangles.iter().zip(surfaces) {
        let [a, b, c] = t.map(|i| remap[i as usize]);
        if a != b && b != c && a != c {
            kept_triangles.push([a, b, c]);
            kept_surfaces.push(surface);
        }
    }
    (!kept_triangles.is_empty()).then_some(Shape::Mesh {
        vertices,
        triangles: kept_triangles,
        surfaces: kept_surfaces,
    })
}

/// What a mesh's convex shapes are made of: the material of its primitive with the most
/// triangles (`u16::MAX`: none).
fn main_material(scene: &ImportedScene, mesh: usize) -> u16 {
    scene
        .meshes
        .get(mesh)
        .and_then(|m| m.primitives.iter().max_by_key(|p| p.indices.len()))
        .and_then(|p| p.material)
        .map_or(u16::MAX, |m| m.min(u16::MAX as usize - 1) as u16)
}

/// Every vertex of a glTF mesh, mapped by `into`.
fn mesh_points(scene: &ImportedScene, mesh: usize, into: Mat4) -> Vec<Vec3> {
    scene.meshes[mesh]
        .primitives
        .iter()
        .flat_map(|p| {
            p.positions
                .iter()
                .map(move |&q| into.transform_point3(Vec3::from(q)))
        })
        .collect()
}

/// Whether a dynamic mesh is decomposed by `kind`: under [`DynamicShape::Auto`] too, since
/// only the decomposition tells a concave mesh from one convex enough for a primitive (a
/// U's hull is a box).
fn needs_parts(kind: DynamicShape) -> bool {
    matches!(kind, DynamicShape::Auto | DynamicShape::Parts)
}

/// Derived data cache entries of convex parts.
const PARTS_KIND: &str = "parts";
/// First bytes of a parts entry; the digits are the entry's version.
const PARTS_MAGIC: &[u8; 8] = b"ENGPRT01";

/// The cache key of a mesh's convex parts under `settings`.
fn parts_key(vertices: &[Vec3], triangles: &[[u32; 3]], settings: &DecomposeSettings) -> u128 {
    let mut h = StableHasher::new();
    // 2: the mesh's bytes by `content_hash128` (ADR 0113).
    h.write_str("engine collider parts 2");
    h.write_u64(u64::from(decompose::REVISION));
    // Every setting, in a fixed order.
    h.write_u64(settings.threshold.to_bits());
    for v in [
        settings.iterations,
        settings.depth,
        settings.candidates,
        settings.resolution,
        settings.search_triangles,
        settings.samples,
        usize::from(settings.merge),
        settings.hull_points,
        settings.max_hulls.map_or(usize::MAX, |m| m),
    ] {
        h.write_u64(v as u64);
    }
    let mut bytes = Vec::with_capacity(vertices.len() * 12 + triangles.len() * 12);
    for v in vertices {
        for c in v.to_array() {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    for t in triangles {
        for i in t {
            bytes.extend_from_slice(&i.to_le_bytes());
        }
    }
    h.write(&content_hash128(&bytes).to_le_bytes());
    h.finish()
}

fn parts_to_bytes(parts: &[Vec<Vec3>]) -> Vec<u8> {
    let points: usize = parts.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(12 + 4 * parts.len() + 12 * points);
    out.extend_from_slice(PARTS_MAGIC);
    out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
    for part in parts {
        out.extend_from_slice(&(part.len() as u32).to_le_bytes());
        for p in part {
            for c in p.to_array() {
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
    }
    out
}

fn parts_from_bytes(bytes: &[u8]) -> Option<Vec<Vec<Vec3>>> {
    let mut rest = bytes.strip_prefix(PARTS_MAGIC)?;
    let mut word = || -> Option<u32> {
        let (w, tail) = rest.split_first_chunk::<4>()?;
        rest = tail;
        Some(u32::from_le_bytes(*w))
    };
    let count = word()? as usize;
    let mut parts = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let n = word()? as usize;
        let mut part = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let [x, y, z] = [word()?, word()?, word()?].map(f32::from_bits);
            part.push(Vec3::new(x, y, z));
        }
        parts.push(part);
    }
    word().is_none().then_some(parts)
}

/// A glTF mesh's convex parts (in mesh space), from the cache when it has them; whether
/// they came from it.
fn parts_of(
    scene: &ImportedScene,
    mesh: usize,
    rules: &ColliderRules,
    cache: Option<&DerivedDataCache>,
    jobs: Option<&ThreadPool>,
) -> (Vec<Vec<Vec3>>, bool) {
    let Some(Shape::Mesh {
        vertices,
        triangles,
        ..
    }) = mesh_triangles(scene, mesh, rules, Mat4::IDENTITY)
    else {
        return (Vec::new(), false);
    };
    let key = parts_key(&vertices, &triangles, &rules.parts);
    if let Some(parts) = cache
        .and_then(|c| c.get(key, PARTS_KIND))
        .and_then(|bytes| parts_from_bytes(&bytes))
    {
        return (parts, true);
    }
    let (parts, _) = decompose::decompose(&vertices, &triangles, &rules.parts, jobs);
    if let Some(cache) = cache {
        // A cache that cannot be written only costs the next cook the work again.
        let _ = cache.put(key, PARTS_KIND, &parts_to_bytes(&parts));
    }
    (parts, false)
}

/// The hull of `points`, at most [`HULL_POINTS`] of them; a thin box for flat sets.
fn hull_shape(points: &[Vec3]) -> Option<Shape> {
    let settings = HullSettings {
        max_vertices: HULL_POINTS,
        ..HullSettings::default()
    };
    match Hull::with_settings(points, &settings) {
        Ok(hull) => Some(Shape::Hull {
            points: hull.points,
        }),
        Err(_) if points.len() >= 2 => Some(fit::flat_box(points, 0.01).into_shape()),
        Err(_) => None,
    }
}

trait IntoShape {
    fn into_shape(self) -> Shape;
}

impl IntoShape for fit::OrientedBox {
    fn into_shape(self) -> Shape {
        Shape::Box {
            center: self.center,
            rotation: self.rotation,
            half_extents: self.half_extents,
        }
    }
}

/// A prop's shape from its points by `kind`.
fn convex_shape(points: &[Vec3], kind: DynamicShape) -> Option<Shape> {
    let Ok(hull) = Hull::new(points) else {
        return hull_shape(points);
    };
    let primitive = match kind {
        DynamicShape::Auto => fit::fit(&hull, 1.1),
        DynamicShape::Box => Some(fit::Primitive::Box(fit::oriented_box(&hull))),
        DynamicShape::Sphere => Some(fit::Primitive::Sphere(fit::bounding_sphere(&hull.points))),
        DynamicShape::Capsule => Some(fit::Primitive::Capsule(fit::capsule(&hull))),
        // Parts that could not be made (a degenerate mesh): its hull.
        DynamicShape::Hull | DynamicShape::Parts => None,
    };
    primitive
        .map(Shape::from)
        .or_else(|| hull_shape(&hull.points))
}

/// An implicit shape in its node's space, mapped by `into`.
fn implicit(shape: &ImplicitShape, into: Mat4) -> Option<Shape> {
    let local = match *shape {
        ImplicitShape::Sphere { radius } => Shape::Sphere {
            center: Vec3::ZERO,
            radius,
        },
        ImplicitShape::Box { size } => Shape::Box {
            center: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_extents: Vec3::from(size) * 0.5,
        },
        ImplicitShape::Capsule {
            height,
            radius_top,
            radius_bottom,
        } if (radius_top - radius_bottom).abs() <= 1e-6 * radius_top.max(radius_bottom) => {
            Shape::Capsule {
                center: Vec3::ZERO,
                rotation: Quat::IDENTITY,
                half_height: 0.5 * height,
                radius: radius_top,
            }
        }
        ImplicitShape::Cylinder {
            height,
            radius_top,
            radius_bottom,
        } if (radius_top - radius_bottom).abs() <= 1e-6 * radius_top.max(radius_bottom) => {
            Shape::Cylinder {
                center: Vec3::ZERO,
                rotation: Quat::IDENTITY,
                half_height: 0.5 * height,
                radius: radius_top,
            }
        }
        // Tapered: the hull of the two ends (spheres or disks), sampled.
        ImplicitShape::Capsule {
            height,
            radius_top,
            radius_bottom,
        } => {
            let tolerance = ROUND_TOLERANCE * (height + radius_top + radius_bottom);
            let mut points = Vec::new();
            for (y, r) in [(0.5 * height, radius_top), (-0.5 * height, radius_bottom)] {
                points.extend(
                    Shape::Sphere {
                        center: Vec3::new(0.0, y, 0.0),
                        radius: r.max(1e-6),
                    }
                    .hull_points(tolerance),
                );
            }
            Shape::Hull { points }
        }
        ImplicitShape::Cylinder {
            height,
            radius_top,
            radius_bottom,
        } => {
            let tolerance = ROUND_TOLERANCE * (height + radius_top + radius_bottom);
            let mut points = Vec::new();
            // Each end's disk: a flat cylinder's points at that height.
            for (y, r) in [(0.5 * height, radius_top), (-0.5 * height, radius_bottom)] {
                let disk = Shape::Cylinder {
                    center: Vec3::new(0.0, y, 0.0),
                    rotation: Quat::IDENTITY,
                    half_height: 0.0,
                    radius: r.max(1e-6),
                };
                points.extend(disk.hull_points(tolerance));
            }
            Shape::Hull { points }
        }
        ImplicitShape::Plane { size_x, size_z, .. } => Shape::Box {
            center: Vec3::new(0.0, -0.5 * PLANE_THICKNESS, 0.0),
            rotation: Quat::IDENTITY,
            half_extents: Vec3::new(
                0.5 * size_x.unwrap_or(2.0 * PLANE_EXTENT),
                0.5 * PLANE_THICKNESS,
                0.5 * size_z.unwrap_or(2.0 * PLANE_EXTENT),
            ),
        },
        ImplicitShape::Unknown => return None,
    };
    let size = match &local {
        Shape::Sphere { radius, .. } => *radius,
        Shape::Box { half_extents, .. } => half_extents.max_element(),
        Shape::Capsule {
            half_height,
            radius,
            ..
        }
        | Shape::Cylinder {
            half_height,
            radius,
            ..
        } => half_height + radius,
        _ => 1.0,
    };
    let scale = Mat3::from_mat4(into);
    let size = size
        * scale
            .x_axis
            .length()
            .max(scale.y_axis.length())
            .max(scale.z_axis.length());
    Some(local.transformed(affine(into), ROUND_TOLERANCE * size))
}

/// Whether `m` is translation × rotation × scale (no shear).
fn is_trs(m: Mat4) -> bool {
    let (s, r, t) = m.to_scale_rotation_translation();
    let again = Mat4::from_scale_rotation_translation(s, r, t);
    let size = m
        .to_cols_array()
        .iter()
        .fold(0.0f32, |a, x| a.max(x.abs()))
        .max(1e-30);
    (again - m)
        .to_cols_array()
        .iter()
        .all(|d| d.abs() <= 1e-5 * size)
}

struct Builder<'a> {
    scene: &'a ImportedScene,
    rules: &'a ColliderRules,
    world: Vec<Mat4>,
    colliders: Vec<CookedCollider>,
    bodies: Vec<CookedBody>,
    /// Each body's node and its collider in the node's object space.
    nodes: Vec<BodyNode>,
    /// Shared colliders: (mesh, dynamic) → index.
    shared: HashMap<(usize, bool), Option<u32>>,
    cache: Option<&'a DerivedDataCache>,
    jobs: Option<&'a ThreadPool>,
    /// Convex parts of glTF meshes (in mesh space), by mesh.
    parts: HashMap<usize, Vec<Vec<Vec3>>>,
    stats: ColliderStats,
}

impl Builder<'_> {
    /// What was made.
    fn finish(self) -> NodeColliders {
        NodeColliders {
            colliders: self.colliders,
            bodies: self.bodies,
            nodes: self.nodes,
            stats: self.stats,
        }
    }

    /// Splits `meshes` into convex parts, all at once in parallel (each from the cache when
    /// it has them).
    fn decompose_meshes(&mut self, mut meshes: Vec<usize>) {
        meshes.sort_unstable();
        meshes.dedup();
        meshes.retain(|m| !self.parts.contains_key(m));
        if meshes.is_empty() {
            return;
        }
        engine_profiler::zone!("assets::decompose_meshes");
        let results: Vec<OnceLock<(Vec<Vec<Vec3>>, bool)>> =
            meshes.iter().map(|_| OnceLock::new()).collect();
        let work = |k: usize| {
            let _ = results[k].set(parts_of(
                self.scene, meshes[k], self.rules, self.cache, self.jobs,
            ));
        };
        match self.jobs {
            Some(pool) if meshes.len() > 1 => pool.parallel_for(meshes.len(), 1, |_, range| {
                range.for_each(&work);
            }),
            _ => (0..meshes.len()).for_each(work),
        }
        for (mesh, result) in meshes.into_iter().zip(results) {
            if let Some((parts, hit)) = result.into_inner() {
                self.stats.decomposed += 1;
                self.stats.cached += usize::from(hit);
                self.parts.insert(mesh, parts);
            }
        }
    }

    /// A mesh's convex parts as hulls, mapped by `into`; none if it was not decomposed or is
    /// convex enough to be one part (its own hull, or a primitive, is then more exact).
    fn part_shapes(&self, mesh: usize, into: Mat4) -> Option<Vec<Shape>> {
        let parts = self.parts.get(&mesh).filter(|p| p.len() > 1)?;
        let shapes: Vec<Shape> = parts
            .iter()
            .filter_map(|p| {
                let mapped: Vec<Vec3> = p.iter().map(|&q| into.transform_point3(q)).collect();
                hull_shape(&mapped)
            })
            .collect();
        (!shapes.is_empty()).then_some(shapes)
    }
    /// A collider of `shapes`; the convex ones made of `surface` (meshes' triangles carry
    /// their own).
    fn add(&mut self, shapes: Vec<Shape>, origin: Origin, surface: u16) -> Option<u32> {
        if shapes.is_empty() {
            return None;
        }
        self.colliders.push(CookedCollider {
            collider: Collider {
                shapes: shapes
                    .into_iter()
                    .enumerate()
                    .map(|(id, shape)| ColliderShape {
                        id: id as u32,
                        surface: if matches!(shape, Shape::Mesh { .. }) {
                            u16::MAX
                        } else {
                            surface
                        },
                        shape,
                    })
                    .collect(),
                origin,
            },
            baked: None,
        });
        Some((self.colliders.len() - 1) as u32)
    }

    /// A body made for node `node`; a transform with shear gets a collider of its own with
    /// the shear baked in.
    fn body(&mut self, node: usize, collider: u32, world: Mat4, body: CookedBody) {
        self.nodes.push(BodyNode {
            node: node as u32,
            collider,
        });
        let (collider, world) = if is_trs(world) {
            (collider, world)
        } else {
            let linear = Mat4::from_mat3(Mat3::from_mat4(world));
            let mapped = self.colliders[collider as usize]
                .collider
                .transformed(affine(linear), 1e-3);
            self.colliders.push(CookedCollider {
                collider: mapped,
                baked: None,
            });
            (
                (self.colliders.len() - 1) as u32,
                Mat4::from_translation(world.w_axis.truncate()),
            )
        };
        self.bodies.push(CookedBody {
            collider,
            world_from_object: world.to_cols_array(),
            ..body
        });
    }
}

/// The scene's colliders and bodies (see the module's documentation), and what it took.
/// `node_instances`: each node's render instances (first, count), as the cook laid them
/// out. Convex parts come from `cache` when it has them, and are computed on `jobs`.
pub fn cook_colliders(
    scene: &ImportedScene,
    node_instances: &[(u32, u32)],
    rules: &ColliderRules,
    cache: Option<&DerivedDataCache>,
    jobs: Option<&ThreadPool>,
) -> (Vec<CookedCollider>, Vec<CookedBody>, ColliderStats) {
    let cooked = cook_node_colliders(scene, node_instances, rules, cache, jobs);
    (cooked.colliders, cooked.bodies, cooked.stats)
}

/// A body's node: what [`cook_node_colliders`] made it for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyNode {
    /// The node (`ImportedScene::nodes`).
    pub node: u32,
    /// The body's collider in the node's object space: the body's own, or the one a
    /// transform's shear was baked from.
    pub collider: u32,
}

/// What [`cook_node_colliders`] made.
#[derive(Debug, Clone, Default)]
pub struct NodeColliders {
    /// The colliders, as [`cook_colliders`] makes them.
    pub colliders: Vec<CookedCollider>,
    /// The bodies, as [`cook_colliders`] makes them.
    pub bodies: Vec<CookedBody>,
    /// Each body's node, in the bodies' order.
    pub nodes: Vec<BodyNode>,
    /// What it took.
    pub stats: ColliderStats,
}

/// [`cook_colliders`], with each body's node: where a scene's model of a mesh finds its
/// collider (ADR 0117).
pub fn cook_node_colliders(
    scene: &ImportedScene,
    node_instances: &[(u32, u32)],
    rules: &ColliderRules,
    cache: Option<&DerivedDataCache>,
    jobs: Option<&ThreadPool>,
) -> NodeColliders {
    engine_profiler::zone!("assets::cook_colliders");
    let (world, parent) = world_transforms(scene);
    let mut b = Builder {
        scene,
        rules,
        world,
        colliders: Vec::new(),
        bodies: Vec::new(),
        nodes: Vec::new(),
        shared: HashMap::new(),
        cache,
        jobs,
        parts: HashMap::new(),
        stats: ColliderStats::default(),
    };
    let instances_of = |i: usize| -> Vec<u32> {
        node_instances
            .get(i)
            .map(|&(first, count)| (first..first + count).collect())
            .unwrap_or_default()
    };
    let uses_khr = scene.nodes.iter().any(|n| n.physics.is_some());
    if uses_khr {
        khr_bodies(&mut b, &parent, &instances_of);
        return b.finish();
    }

    // Authored collision meshes, by the node (or mesh) they name.
    let mut authored: HashMap<usize, Vec<(usize, AuthoredKind)>> = HashMap::new();
    let mut orphans = Vec::new();
    for (i, node) in scene.nodes.iter().enumerate() {
        let Some((kind, base)) = collision_name(&node.name) else {
            continue;
        };
        let target = scene
            .nodes
            .iter()
            .position(|n| n.name == base && !is_collision_node(&n.name))
            .or_else(|| {
                scene.nodes.iter().position(|n| {
                    n.mesh.is_some_and(|m| scene.meshes[m].name == base)
                        && !is_collision_node(&n.name)
                })
            });
        match target {
            Some(t) => authored.entry(t).or_default().push((i, kind)),
            None => orphans.push((i, kind)),
        }
    }
    let shape_of =
        |b: &Builder<'_>, node: usize, kind: AuthoredKind, into: Mat4| -> Option<Shape> {
            let mesh = b.scene.nodes[node].mesh?;
            let points = mesh_points(b.scene, mesh, into);
            let hull = Hull::new(&points).ok();
            match (kind, hull) {
                (AuthoredKind::Hull, _) => hull_shape(&points),
                (AuthoredKind::Box, Some(h)) => Some(fit::oriented_box(&h).into_shape()),
                (AuthoredKind::Sphere, Some(h)) => Some(Shape::from(fit::Primitive::Sphere(
                    fit::bounding_sphere(&h.points),
                ))),
                (AuthoredKind::Capsule, Some(h)) => {
                    Some(Shape::from(fit::Primitive::Capsule(fit::capsule(&h))))
                }
                (_, None) => hull_shape(&points),
            }
        };
    // Props' meshes decomposed up front.
    let wanted = scene
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, n)| {
            !is_collision_node(&n.name) && n.skin.is_none() && !authored.contains_key(i)
        })
        .filter_map(|(_, n)| n.mesh)
        .filter(|&m| {
            let r = rules.resolve(scene, m);
            r.bodies == DefaultBodies::Dynamic && needs_parts(r.dynamic_shape)
        })
        .collect();
    b.decompose_meshes(wanted);
    for i in 0..scene.nodes.len() {
        let node = &scene.nodes[i];
        if is_collision_node(&node.name) {
            continue;
        }
        if let Some(parts) = authored.get(&i) {
            // In the named node's object space.
            let into_object = b.world[i].inverse();
            let shapes: Vec<Shape> = parts
                .iter()
                .filter_map(|&(c, kind)| shape_of(&b, c, kind, into_object * b.world[c]))
                .collect();
            let own = node.mesh.map(|m| rules.resolve(scene, m));
            let motion = match own.as_ref().map_or(rules.bodies, |r| r.bodies) {
                DefaultBodies::Dynamic => Motion::Dynamic,
                _ => Motion::Static,
            };
            let surface = node.mesh.map_or(u16::MAX, |m| main_material(scene, m));
            if let Some(collider) = b.add(shapes, Origin::Authored, surface) {
                let world = b.world[i];
                b.body(
                    i,
                    collider,
                    world,
                    CookedBody {
                        motion,
                        instances: if motion == Motion::Static {
                            Vec::new()
                        } else {
                            instances_of(i)
                        },
                        layer: own
                            .and_then(|r| r.layer)
                            .or(rules.layer.as_deref())
                            .map(str::to_owned),
                        ..CookedBody::default()
                    },
                );
            }
            continue;
        }
        let Some(mesh) = node.mesh else {
            continue;
        };
        let own = rules.resolve(scene, mesh);
        if node.skin.is_some() || own.bodies == DefaultBodies::None {
            continue;
        }
        let dynamic = own.bodies == DefaultBodies::Dynamic;
        let collider = match b.shared.get(&(mesh, dynamic)) {
            Some(&c) => c,
            None => {
                let shapes: Vec<Shape> = if dynamic {
                    let parts = if needs_parts(own.dynamic_shape) {
                        b.part_shapes(mesh, Mat4::IDENTITY)
                    } else {
                        None
                    };
                    parts.unwrap_or_else(|| {
                        // Convex enough for one part: as a prop always was.
                        let kind = match own.dynamic_shape {
                            DynamicShape::Parts => DynamicShape::Hull,
                            kind => kind,
                        };
                        convex_shape(&mesh_points(scene, mesh, Mat4::IDENTITY), kind)
                            .into_iter()
                            .collect()
                    })
                } else if own.static_shape == StaticShape::Mesh {
                    mesh_triangles(scene, mesh, rules, Mat4::IDENTITY)
                        .into_iter()
                        .collect()
                } else {
                    Vec::new()
                };
                let c = b.add(shapes, Origin::Generated, main_material(scene, mesh));
                b.shared.insert((mesh, dynamic), c);
                c
            }
        };
        if let Some(collider) = collider {
            let world = b.world[i];
            b.body(
                i,
                collider,
                world,
                CookedBody {
                    motion: if dynamic {
                        Motion::Dynamic
                    } else {
                        Motion::Static
                    },
                    instances: if dynamic { instances_of(i) } else { Vec::new() },
                    layer: own.layer.map(str::to_owned),
                    ..CookedBody::default()
                },
            );
        }
    }
    // Collision meshes that name nothing: static colliders of their own.
    for (c, kind) in orphans {
        let world = b.world[c];
        if let Some(shape) = shape_of(&b, c, kind, Mat4::IDENTITY)
            && let Some(collider) = b.add(vec![shape], Origin::Authored, u16::MAX)
        {
            b.body(
                c,
                collider,
                world,
                CookedBody {
                    layer: rules.layer.clone(),
                    ..CookedBody::default()
                },
            );
        }
    }
    b.finish()
}

/// Bodies from `KHR_physics_rigid_bodies`.
fn khr_bodies(
    b: &mut Builder<'_>,
    parent: &[Option<usize>],
    instances_of: &dyn Fn(usize) -> Vec<u32>,
) {
    let scene = b.scene;
    // The body each node belongs to: its nearest ancestor (or itself) with a motion.
    let owner = |mut i: usize| -> Option<usize> {
        loop {
            if scene.nodes[i]
                .physics
                .as_ref()
                .is_some_and(|p| p.motion.is_some())
            {
                return Some(i);
            }
            i = parent[i]?;
        }
    };
    let dynamic = |root: usize| {
        scene.nodes[root]
            .physics
            .as_ref()
            .and_then(|p| p.motion)
            .is_some_and(|m| !m.kinematic)
    };
    // A simulated body cannot be a triangle mesh: the meshes of dynamic bodies' mesh
    // colliders are split into convex parts, up front.
    let mut wanted = Vec::new();
    for (i, node) in scene.nodes.iter().enumerate() {
        let Some(c) = node.physics.as_ref().and_then(|p| p.collider.as_ref()) else {
            continue;
        };
        let Some(Geometry::Node {
            node: g,
            convex_hull: false,
        }) = c.geometry
        else {
            continue;
        };
        if !owner(i).is_some_and(dynamic) {
            continue;
        }
        let mut stack = vec![g];
        while let Some(n) = stack.pop() {
            let Some(nd) = scene.nodes.get(n) else {
                continue;
            };
            wanted.extend(nd.mesh);
            stack.extend(&nd.children);
        }
    }
    b.decompose_meshes(wanted);
    // The shapes of a geometry, in `into` (the body's object space); a dynamic body's
    // meshes as their convex parts.
    let geometry = |b: &Builder<'_>, g: &Geometry, into: Mat4, dynamic: bool| -> Vec<Shape> {
        match *g {
            Geometry::Shape(s) => scene
                .physics
                .shapes
                .get(s)
                .and_then(|shape| implicit(shape, into))
                .into_iter()
                .collect(),
            Geometry::Node { node, convex_hull } => {
                // The node's mesh and its children's, in the collider node's space.
                let mut out = Vec::new();
                let mut stack = vec![(node, Mat4::IDENTITY)];
                let mut points = Vec::new();
                while let Some((n, m)) = stack.pop() {
                    let Some(nd) = scene.nodes.get(n) else {
                        continue;
                    };
                    if let Some(mesh) = nd.mesh {
                        if convex_hull {
                            points.extend(mesh_points(scene, mesh, into * m));
                        } else if dynamic {
                            // Its convex parts, or its hull when it is convex enough.
                            match b.part_shapes(mesh, into * m) {
                                Some(parts) => out.extend(parts),
                                None => out.extend(hull_shape(&mesh_points(scene, mesh, into * m))),
                            }
                        } else if let Some(s) = mesh_triangles(scene, mesh, b.rules, into * m) {
                            out.push(s);
                        }
                    }
                    for &c in &nd.children {
                        stack.push((c, m * local(scene, c)));
                    }
                }
                if convex_hull {
                    out.extend(hull_shape(&points));
                }
                out
            }
        }
    };
    let material = |m: Option<usize>| m.and_then(|m| scene.physics.materials.get(m));
    // A collider's layer: its collision filter's first system.
    let layer = |f: Option<usize>| {
        f.and_then(|f| scene.physics.filters.get(f))
            .and_then(|f| f.systems.first().cloned())
    };
    // What a geometry's convex shapes are made of: its node's mesh's main material.
    let surface_of = |g: &Geometry| match *g {
        Geometry::Node { node, .. } => scene
            .nodes
            .get(node)
            .and_then(|n| n.mesh)
            .map_or(u16::MAX, |m| main_material(scene, m)),
        Geometry::Shape(_) => u16::MAX,
    };
    // Motion bodies: the colliders of their subtrees (their shapes, physics material,
    // surface and layer: the first collider's that has one).
    type Group = (Vec<Shape>, Option<usize>, Option<u16>, Option<String>);
    let mut grouped: HashMap<usize, Group> = HashMap::new();
    let mut statics = Vec::new();
    for (i, node) in scene.nodes.iter().enumerate() {
        let Some(p) = &node.physics else {
            continue;
        };
        for (data, sensor) in [(&p.collider, false), (&p.trigger, true)] {
            let Some(c) = data else {
                continue;
            };
            let Some(g) = &c.geometry else {
                continue;
            };
            match owner(i).filter(|_| !sensor) {
                Some(root) => {
                    let into = b.world[root].inverse() * b.world[i];
                    let shapes = geometry(b, g, into, dynamic(root));
                    let entry = grouped.entry(root).or_default();
                    entry.0.extend(shapes);
                    entry.1 = entry.1.or(c.material);
                    let surface = surface_of(g);
                    if entry.2.is_none() && surface != u16::MAX {
                        entry.2 = Some(surface);
                    }
                    if entry.3.is_none() {
                        entry.3 = layer(c.filter);
                    }
                }
                None => statics.push((i, *g, c.material, sensor, c.filter)),
            }
        }
    }
    let mut roots: Vec<usize> = grouped.keys().copied().collect();
    roots.sort_unstable();
    for root in roots {
        let (shapes, mat, surface, body_layer) = grouped.remove(&root).unwrap_or_default();
        let Some(motion) = scene.nodes[root].physics.as_ref().and_then(|p| p.motion) else {
            continue;
        };
        let Some(collider) = b.add(shapes, Origin::Authored, surface.unwrap_or(u16::MAX)) else {
            continue;
        };
        let world = b.world[root];
        let rotation = Mat3::from_mat4(world);
        let m = material(mat);
        // The subtree's render instances move with the body (down to other bodies).
        let mut instances = Vec::new();
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            instances.extend(instances_of(n));
            for &c in &scene.nodes[n].children {
                if scene.nodes[c]
                    .physics
                    .as_ref()
                    .is_none_or(|p| p.motion.is_none())
                {
                    stack.push(c);
                }
            }
        }
        b.body(
            root,
            collider,
            world,
            CookedBody {
                instances,
                motion: if motion.kinematic {
                    Motion::Kinematic
                } else {
                    Motion::Dynamic
                },
                friction: m.map_or(0.5, |m| m.dynamic_friction),
                restitution: m.map_or(0.0, |m| m.restitution),
                mass: motion.mass,
                linear_velocity: (rotation * Vec3::from(motion.linear_velocity)).to_array(),
                angular_velocity: (rotation * Vec3::from(motion.angular_velocity)).to_array(),
                gravity_factor: motion.gravity_factor,
                layer: body_layer,
                ..CookedBody::default()
            },
        );
    }
    for (i, g, mat, sensor, filter) in statics {
        let shapes = geometry(b, &g, Mat4::IDENTITY, false);
        let Some(collider) = b.add(shapes, Origin::Authored, surface_of(&g)) else {
            continue;
        };
        let m = material(mat);
        let world = b.world[i];
        b.body(
            i,
            collider,
            world,
            CookedBody {
                sensor,
                friction: m.map_or(0.5, |m| m.dynamic_friction),
                restitution: m.map_or(0.0, |m| m.restitution),
                layer: layer(filter),
                ..CookedBody::default()
            },
        );
    }
}

// ---------------------------------------------------------------------------------------
// In packs (version 6)

/// The engine's form's revision in collider entries.
const SHAPES_REVISION: u32 = 1;
/// [`collider_entry`]'s flags.
const BAKED: u32 = 1;
const CASTABLE: u32 = 2;
const AUTHORED: u32 = 4;
const EDITED: u32 = 8;

const BOX: u8 = 1;
const SPHERE: u8 = 2;
const CAPSULE: u8 = 3;
const CYLINDER: u8 = 4;
const HULL: u8 = 5;
const MESH: u8 = 6;

fn put(out: &mut Vec<u8>, values: &[f32]) {
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A collider entry: `meta = [format key low, high, flags, shapes, shape bytes, baked
/// bytes, revision]`; the shapes in the engine's form, then the baked bytes (from a 16-byte
/// boundary). A shape is `[id, surface | kind << 16 | flags << 24]` then its kind's fields;
/// a mesh carries its vertices, triangles and surfaces only when the collider is not baked
/// (the backend's form is what the runtime loads, and meshes take every scale as they are).
pub(crate) fn collider_entry(c: &CookedCollider) -> ([u32; 7], Vec<u8>) {
    let mut data = Vec::new();
    let baked = c.baked.as_ref();
    for s in &c.collider.shapes {
        put_u32(&mut data, s.id);
        let (kind, mesh_included) = match &s.shape {
            Shape::Box { .. } => (BOX, false),
            Shape::Sphere { .. } => (SPHERE, false),
            Shape::Capsule { .. } => (CAPSULE, false),
            Shape::Cylinder { .. } => (CYLINDER, false),
            Shape::Hull { .. } => (HULL, false),
            Shape::Mesh { .. } => (MESH, baked.is_none()),
        };
        put_u32(
            &mut data,
            u32::from(s.surface) | u32::from(kind) << 16 | u32::from(mesh_included) << 24,
        );
        match &s.shape {
            Shape::Box {
                center,
                rotation,
                half_extents,
            } => {
                put(&mut data, &center.to_array());
                put(&mut data, &rotation.to_array());
                put(&mut data, &half_extents.to_array());
            }
            Shape::Sphere { center, radius } => {
                put(&mut data, &center.to_array());
                put(&mut data, &[*radius]);
            }
            Shape::Capsule {
                center,
                rotation,
                half_height,
                radius,
            }
            | Shape::Cylinder {
                center,
                rotation,
                half_height,
                radius,
            } => {
                put(&mut data, &center.to_array());
                put(&mut data, &rotation.to_array());
                put(&mut data, &[*half_height, *radius]);
            }
            Shape::Hull { points } => {
                put_u32(&mut data, points.len() as u32);
                for p in points {
                    put(&mut data, &p.to_array());
                }
            }
            Shape::Mesh {
                vertices,
                triangles,
                surfaces,
            } => {
                put_u32(&mut data, vertices.len() as u32);
                put_u32(&mut data, triangles.len() as u32);
                if mesh_included {
                    for v in vertices {
                        put(&mut data, &v.to_array());
                    }
                    for t in triangles {
                        for &i in t {
                            put_u32(&mut data, i);
                        }
                    }
                    put_u32(&mut data, surfaces.len() as u32);
                    for &surface in surfaces {
                        data.extend_from_slice(&surface.to_le_bytes());
                    }
                    data.resize(data.len().next_multiple_of(4), 0);
                }
            }
        }
    }
    let shape_bytes = data.len().next_multiple_of(16);
    data.resize(shape_bytes, 0);
    let mut flags = match c.collider.origin {
        Origin::Generated => 0,
        Origin::Authored => AUTHORED,
        Origin::Edited => EDITED,
    };
    let (format, baked_bytes) = match baked {
        Some(b) => {
            flags |= BAKED | if b.castable { CASTABLE } else { 0 };
            data.extend_from_slice(&b.bytes);
            (b.format, b.bytes.len())
        }
        None => (0, 0),
    };
    let meta = [
        format as u32,
        (format >> 32) as u32,
        flags,
        c.collider.shapes.len() as u32,
        shape_bytes as u32,
        baked_bytes as u32,
        SHAPES_REVISION,
    ];
    (meta, data)
}

/// Reads little-endian values off an entry, checking bounds.
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], PackError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| invalid("collider entry truncated"))?;
        let out = &self.data[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, PackError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f32s<const N: usize>(&mut self) -> Result<[f32; N], PackError> {
        let mut out = [0.0; N];
        for o in &mut out {
            *o = f32::from_bits(self.u32()?);
        }
        Ok(out)
    }

    fn vec3(&mut self) -> Result<Vec3, PackError> {
        Ok(Vec3::from_array(self.f32s::<3>()?))
    }

    fn quat(&mut self) -> Result<Quat, PackError> {
        Ok(Quat::from_array(self.f32s::<4>()?))
    }

    /// A count of items of `size` bytes that must fit in what is left.
    fn count(&mut self, size: usize) -> Result<usize, PackError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(size) > self.data.len() - self.at {
            return Err(invalid("collider entry: a count past its end"));
        }
        Ok(n)
    }
}

/// A collider entry's collider (see [`collider_entry`]).
pub fn read_collider_entry(meta: [u32; 7], data: &[u8]) -> Result<CookedCollider, PackError> {
    if meta[6] != SHAPES_REVISION {
        return Err(invalid(format!("collider entry revision {}", meta[6])));
    }
    let shape_bytes = meta[4] as usize;
    let baked_bytes = meta[5] as usize;
    if shape_bytes
        .checked_add(baked_bytes)
        .is_none_or(|n| n > data.len())
    {
        return Err(invalid("collider entry truncated"));
    }
    let mut c = Cursor {
        data: &data[..shape_bytes],
        at: 0,
    };
    let mut shapes = Vec::with_capacity((meta[3] as usize).min(1 << 16));
    for _ in 0..meta[3] {
        let id = c.u32()?;
        let word = c.u32()?;
        let surface = (word & 0xFFFF) as u16;
        let kind = (word >> 16) as u8;
        let mesh_included = (word >> 24) & 1 != 0;
        let shape = match kind {
            BOX => Shape::Box {
                center: c.vec3()?,
                rotation: c.quat()?,
                half_extents: c.vec3()?,
            },
            SPHERE => Shape::Sphere {
                center: c.vec3()?,
                radius: c.f32s::<1>()?[0],
            },
            CAPSULE | CYLINDER => {
                let center = c.vec3()?;
                let rotation = c.quat()?;
                let [half_height, radius] = c.f32s::<2>()?;
                if kind == CAPSULE {
                    Shape::Capsule {
                        center,
                        rotation,
                        half_height,
                        radius,
                    }
                } else {
                    Shape::Cylinder {
                        center,
                        rotation,
                        half_height,
                        radius,
                    }
                }
            }
            HULL => {
                let n = c.count(12)?;
                Shape::Hull {
                    points: (0..n).map(|_| c.vec3()).collect::<Result<_, _>>()?,
                }
            }
            MESH => {
                let vertex_count = c.u32()? as usize;
                let triangle_count = c.u32()? as usize;
                if mesh_included {
                    if vertex_count.saturating_mul(12) > c.data.len() - c.at {
                        return Err(invalid("collider entry: a count past its end"));
                    }
                    let vertices: Vec<Vec3> = (0..vertex_count)
                        .map(|_| c.vec3())
                        .collect::<Result<_, _>>()?;
                    if triangle_count.saturating_mul(12) > c.data.len() - c.at {
                        return Err(invalid("collider entry: a count past its end"));
                    }
                    let mut triangles = Vec::with_capacity(triangle_count);
                    for _ in 0..triangle_count {
                        let t = [c.u32()?, c.u32()?, c.u32()?];
                        if t.iter().any(|&i| i as usize >= vertex_count) {
                            return Err(invalid("collider entry: a triangle past its vertices"));
                        }
                        triangles.push(t);
                    }
                    let n = c.count(2)?;
                    let raw = c.bytes(n * 2)?;
                    let surfaces = raw
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect();
                    c.bytes((n * 2).next_multiple_of(4) - n * 2)?;
                    Shape::Mesh {
                        vertices,
                        triangles,
                        surfaces,
                    }
                } else {
                    // In the baked form only.
                    Shape::Mesh {
                        vertices: Vec::new(),
                        triangles: Vec::new(),
                        surfaces: Vec::new(),
                    }
                }
            }
            other => return Err(invalid(format!("collider entry: shape kind {other}"))),
        };
        shapes.push(ColliderShape { id, shape, surface });
    }
    let flags = meta[2];
    let origin = if flags & EDITED != 0 {
        Origin::Edited
    } else if flags & AUTHORED != 0 {
        Origin::Authored
    } else {
        Origin::Generated
    };
    let baked = (flags & BAKED != 0).then(|| BakedCollider {
        format: u64::from(meta[0]) | u64::from(meta[1]) << 32,
        bytes: data[shape_bytes..shape_bytes + baked_bytes].to_vec(),
        castable: flags & CASTABLE != 0,
    });
    Ok(CookedCollider {
        collider: Collider { shapes, origin },
        baked,
    })
}

/// A body in a bodies entry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct BodyRecord {
    /// Its collider: the index among the pack's collider entries, in file order.
    pub collider: u32,
    /// 0 static, 1 kinematic, 2 dynamic; `| 1 << 8`: a sensor.
    pub motion: u32,
    /// Its instances in the entry's instance list.
    pub first_instance: u32,
    /// How many.
    pub instance_count: u32,
    /// Friction.
    pub friction: f32,
    /// Restitution.
    pub restitution: f32,
    /// Mass in kg; 0: from the colliders' volume.
    pub mass: f32,
    /// How much gravity acts on it.
    pub gravity_factor: f32,
    /// Initial velocity (world space).
    pub linear_velocity: [f32; 3],
    /// Initial angular velocity (world space).
    pub angular_velocity: [f32; 3],
    /// Its collision layer's name: 1 + an index into the entry's names; 0: by its motion.
    pub layer: u32,
    /// Reserved, 0.
    pub reserved: u32,
    /// Its node's object to world, column major.
    pub world_from_object: [f32; 16],
}

const _: () = assert!(size_of::<BodyRecord>() == 128);

/// The bodies entry: `meta = [bodies, instance indices, layer names, their bytes]`; the
/// [`BodyRecord`]s, the instance indices they list, then the layers' names (each a `u32`
/// length and its UTF-8 bytes). Entries written before layers had names have none.
pub(crate) fn bodies_entry(bodies: &[CookedBody]) -> ([u32; 7], Vec<u8>) {
    let mut records = Vec::with_capacity(bodies.len());
    let mut instances: Vec<u32> = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    for b in bodies {
        let layer = b.layer.as_deref().map_or(0, |name| {
            let at = names.iter().position(|&n| n == name).unwrap_or_else(|| {
                names.push(name);
                names.len() - 1
            });
            at as u32 + 1
        });
        records.push(BodyRecord {
            collider: b.collider,
            motion: match b.motion {
                Motion::Static => 0,
                Motion::Kinematic => 1,
                Motion::Dynamic => 2,
            } | u32::from(b.sensor) << 8,
            first_instance: instances.len() as u32,
            instance_count: b.instances.len() as u32,
            friction: b.friction,
            restitution: b.restitution,
            mass: b.mass.unwrap_or(0.0),
            gravity_factor: b.gravity_factor,
            linear_velocity: b.linear_velocity,
            angular_velocity: b.angular_velocity,
            layer,
            reserved: 0,
            world_from_object: b.world_from_object,
        });
        instances.extend_from_slice(&b.instances);
    }
    let mut data = bytemuck::cast_slice::<BodyRecord, u8>(&records).to_vec();
    data.extend_from_slice(bytemuck::cast_slice(&instances));
    let names_start = data.len();
    for name in &names {
        data.extend_from_slice(&(name.len() as u32).to_le_bytes());
        data.extend_from_slice(name.as_bytes());
    }
    let meta = [
        records.len() as u32,
        instances.len() as u32,
        names.len() as u32,
        (data.len() - names_start) as u32,
        0,
        0,
        0,
    ];
    (meta, data)
}

/// The bodies of a bodies entry (see [`bodies_entry`]).
pub fn read_bodies_entry(meta: [u32; 7], data: &[u8]) -> Result<Vec<CookedBody>, PackError> {
    let (count, refs) = (meta[0] as usize, meta[1] as usize);
    let record_bytes = count.saturating_mul(size_of::<BodyRecord>());
    if record_bytes.saturating_add(refs.saturating_mul(4)) > data.len() {
        return Err(invalid("bodies entry truncated"));
    }
    let records: Vec<BodyRecord> = data[..record_bytes]
        .chunks_exact(size_of::<BodyRecord>())
        .map(bytemuck::pod_read_unaligned)
        .collect();
    let instances: Vec<u32> = data[record_bytes..record_bytes + refs * 4]
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    // The layers' names.
    let (name_count, name_bytes) = (meta[2] as usize, meta[3] as usize);
    let start = record_bytes + refs * 4;
    let mut names = Vec::with_capacity(name_count);
    let mut table = data
        .get(start..start.saturating_add(name_bytes))
        .ok_or_else(|| invalid("bodies entry: layer names truncated"))?;
    for _ in 0..name_count {
        let (len, rest) = table
            .split_first_chunk::<4>()
            .ok_or_else(|| invalid("bodies entry: layer names truncated"))?;
        let len = u32::from_le_bytes(*len) as usize;
        let name = rest
            .get(..len)
            .and_then(|b| core::str::from_utf8(b).ok())
            .ok_or_else(|| invalid("bodies entry: a layer name"))?;
        names.push(name.to_owned());
        table = &rest[len..];
    }
    records
        .iter()
        .map(|r| {
            let first = r.first_instance as usize;
            let list = instances
                .get(first..first + r.instance_count as usize)
                .ok_or_else(|| invalid("bodies entry: instances past its list"))?;
            Ok(CookedBody {
                collider: r.collider,
                world_from_object: r.world_from_object,
                instances: list.to_vec(),
                motion: match r.motion & 0xFF {
                    1 => Motion::Kinematic,
                    2 => Motion::Dynamic,
                    _ => Motion::Static,
                },
                sensor: r.motion >> 8 & 1 != 0,
                friction: r.friction,
                restitution: r.restitution,
                mass: (r.mass > 0.0).then_some(r.mass),
                linear_velocity: r.linear_velocity,
                angular_velocity: r.angular_velocity,
                gravity_factor: r.gravity_factor,
                layer: match r.layer {
                    0 => None,
                    n => Some(
                        names
                            .get(n as usize - 1)
                            .cloned()
                            .ok_or_else(|| invalid("bodies entry: a layer past its names"))?,
                    ),
                },
            })
        })
        .collect()
}

/// The colliders and bodies of an open pack (version 6, ADR 0101): read only when a game
/// simulates physics, so a scene that is only drawn does not load them.
pub fn load_colliders(
    pack: &Pack,
    jobs: &ThreadPool,
    depth: usize,
) -> Result<(Vec<CookedCollider>, Vec<CookedBody>), PackError> {
    engine_profiler::zone!("assets::load_colliders");
    let entries = pack.entries();
    let indices: Vec<u32> = (0..entries.len() as u32)
        .filter(|&i| {
            matches!(
                entries[i as usize].kind(),
                EntryKind::Collider | EntryKind::Bodies
            )
        })
        .collect();
    let colliders: Vec<std::sync::Mutex<Option<CookedCollider>>> = indices
        .iter()
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    let bodies = std::sync::Mutex::new(None);
    let position = |index: u32| indices.binary_search(&index).unwrap_or(usize::MAX);
    pack.load(&indices, jobs, depth, |index, e, data: EntryData| {
        match e.kind() {
            EntryKind::Collider => {
                let c = read_collider_entry(e.meta, &data)?;
                if let Some(slot) = colliders.get(position(index))
                    && let Ok(mut slot) = slot.lock()
                {
                    *slot = Some(c);
                }
            }
            EntryKind::Bodies => {
                let b = read_bodies_entry(e.meta, &data)?;
                if let Ok(mut slot) = bodies.lock()
                    && slot.replace(b).is_some()
                {
                    return Err(invalid("more than one bodies entry"));
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    let colliders: Vec<CookedCollider> = colliders
        .into_iter()
        .filter_map(|c| c.into_inner().ok().flatten())
        .collect();
    let bodies = bodies.into_inner().ok().flatten().unwrap_or_default();
    if bodies
        .iter()
        .any(|b| b.collider as usize >= colliders.len())
    {
        return Err(invalid("a body of a missing collider"));
    }
    Ok((colliders, bodies))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gltf_import::{
        AlphaMode, MaterialData, MaterialLayers, MeshData, NodeData, Primitive,
    };
    use crate::gltf_physics::CollisionFilterData;
    use crate::gltf_physics::{
        ColliderData, MotionData, NodePhysics, PhysicsData, PhysicsMaterialData,
    };

    fn material(name: &str) -> MaterialData {
        MaterialData {
            name: name.to_owned(),
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
            alpha: AlphaMode::Opaque,
            double_sided: false,
            thickness: 0.0,
            thickness_texture: None,
            layers: MaterialLayers::default(),
        }
    }

    /// A box from `lo` to `hi` with its faces' vertices split (as render meshes have them).
    fn cuboid(lo: Vec3, hi: Vec3, material: Option<usize>) -> Primitive {
        let corner = |i: usize| {
            Vec3::new(
                if i & 1 == 0 { lo.x } else { hi.x },
                if i & 2 == 0 { lo.y } else { hi.y },
                if i & 4 == 0 { lo.z } else { hi.z },
            )
        };
        let faces = [
            [0, 2, 1, 1, 2, 3],
            [4, 5, 6, 5, 7, 6],
            [0, 1, 4, 1, 5, 4],
            [2, 6, 3, 3, 6, 7],
            [0, 4, 2, 2, 4, 6],
            [1, 3, 5, 3, 7, 5],
        ];
        let mut positions = Vec::new();
        let mut indices = Vec::new();
        for f in faces {
            for c in f {
                indices.push(positions.len() as u32);
                positions.push(corner(c).to_array());
            }
        }
        let n = positions.len();
        Primitive {
            positions,
            normals: vec![[0.0, 1.0, 0.0]; n],
            uvs: Vec::new(),
            tangents: Vec::new(),
            indices,
            material,
            min: lo.to_array(),
            max: hi.to_array(),
            joints: Vec::new(),
            weights: Vec::new(),
        }
    }

    fn node(name: &str, mesh: Option<usize>, translation: Vec3, children: Vec<usize>) -> NodeData {
        NodeData {
            name: name.to_owned(),
            translation: translation.to_array(),
            rotation: Quat::IDENTITY.to_array(),
            scale: [1.0; 3],
            mesh,
            children,
            skin: None,
            physics: None,
        }
    }

    fn mesh(name: &str, primitives: Vec<Primitive>) -> MeshData {
        MeshData {
            name: name.to_owned(),
            primitives,
        }
    }

    fn instances(scene: &ImportedScene) -> Vec<(u32, u32)> {
        (0..scene.nodes.len() as u32).map(|i| (i, 1)).collect()
    }

    #[test]
    fn level_meshes_are_shared_static_triangles_with_surfaces() {
        let scene = ImportedScene {
            meshes: vec![mesh(
                "wall",
                vec![
                    cuboid(Vec3::ZERO, Vec3::ONE, Some(0)),
                    cuboid(Vec3::new(1.0, 0.0, 0.0), Vec3::new(2.0, 1.0, 1.0), Some(1)),
                ],
            )],
            materials: vec![material("stone"), material("Leaves")],
            nodes: vec![
                node("a", Some(0), Vec3::ZERO, Vec::new()),
                node("b", Some(0), Vec3::new(5.0, 0.0, 0.0), Vec::new()),
            ],
            roots: vec![0, 1],
            ..ImportedScene::default()
        };
        let (colliders, bodies, _) = cook_colliders(
            &scene,
            &instances(&scene),
            &ColliderRules::default(),
            None,
            None,
        );
        assert_eq!(colliders.len(), 1);
        assert_eq!(bodies.len(), 2);
        assert!(
            bodies
                .iter()
                .all(|b| b.motion == Motion::Static && b.collider == 0 && b.instances.is_empty())
        );
        let Shape::Mesh {
            vertices,
            triangles,
            surfaces,
        } = &colliders[0].collider.shapes[0].shape
        else {
            panic!("not a mesh");
        };
        // Welded: 12 corners (two boxes sharing a face's 4), 24 triangles.
        assert_eq!(vertices.len(), 12);
        assert_eq!(triangles.len(), 24);
        assert_eq!(surfaces.iter().filter(|&&s| s == 1).count(), 12);
        assert_eq!(
            bodies[1].world_from_object,
            Mat4::from_translation(Vec3::new(5.0, 0.0, 0.0)).to_cols_array()
        );
        // Foliage excluded by name: only the stone box is left.
        let rules = ColliderRules {
            no_collision: vec!["Leaves".to_owned()],
            ..ColliderRules::default()
        };
        let (colliders, _, _) = cook_colliders(&scene, &instances(&scene), &rules, None, None);
        let Shape::Mesh { triangles, .. } = &colliders[0].collider.shapes[0].shape else {
            panic!("not a mesh");
        };
        assert_eq!(triangles.len(), 12);
    }

    /// A U-shaped prism, `height` tall: 3 × 2 across with a 1 × 1 notch, one closed mesh.
    fn u_prism(height: f32) -> Primitive {
        let outline = [
            (0.0, 0.0),
            (3.0, 0.0),
            (3.0, 2.0),
            (2.0, 2.0),
            (2.0, 1.0),
            (1.0, 1.0),
            (1.0, 2.0),
            (0.0, 2.0),
        ];
        let caps = [
            [0, 1, 4],
            [0, 4, 5],
            [1, 2, 3],
            [1, 3, 4],
            [0, 5, 6],
            [0, 6, 7],
        ];
        let n = outline.len() as u32;
        let mut positions: Vec<[f32; 3]> = outline.iter().map(|&(x, z)| [x, 0.0, z]).collect();
        positions.extend(outline.iter().map(|&(x, z)| [x, height, z]));
        let mut indices = Vec::new();
        for [a, b, c] in caps {
            // The bottom faces -y, the top +y.
            indices.extend([a, b, c]);
            indices.extend([a + n, c + n, b + n]);
        }
        for k in 0..n {
            let k1 = (k + 1) % n;
            indices.extend([k, k + n, k1, k1, k + n, k1 + n]);
        }
        let count = positions.len();
        Primitive {
            positions,
            normals: vec![[0.0, 1.0, 0.0]; count],
            uvs: Vec::new(),
            tangents: Vec::new(),
            indices,
            material: None,
            min: [0.0; 3],
            max: [3.0, height, 2.0],
            joints: Vec::new(),
            weights: Vec::new(),
        }
    }

    fn hull_volumes(shapes: &[ColliderShape]) -> f32 {
        shapes
            .iter()
            .map(|s| match &s.shape {
                Shape::Hull { points } => Hull::new(points).unwrap().volume(),
                other => panic!("not a hull: {other:?}"),
            })
            .sum()
    }

    #[test]
    fn concave_props_get_convex_parts_and_convex_ones_a_primitive() {
        let u = u_prism(1.0);
        let points: Vec<Vec3> = u.positions.iter().map(|&p| Vec3::from(p)).collect();
        let triangles: Vec<[u32; 3]> = u
            .indices
            .chunks_exact(3)
            .map(|t| [t[0], t[1], t[2]])
            .collect();
        assert!(engine_collision::mesh::volume(&points, &triangles) > 0.0);
        let scene = ImportedScene {
            meshes: vec![
                mesh("u", vec![u]),
                mesh(
                    "crate",
                    vec![cuboid(Vec3::splat(-0.5), Vec3::splat(0.5), None)],
                ),
            ],
            nodes: vec![
                node("u", Some(0), Vec3::ZERO, Vec::new()),
                node("crate", Some(1), Vec3::new(5.0, 0.0, 0.0), Vec::new()),
            ],
            roots: vec![0, 1],
            ..ImportedScene::default()
        };
        let rules = ColliderRules {
            bodies: DefaultBodies::Dynamic,
            ..ColliderRules::default()
        };
        let jobs = ThreadPool::builder().workers(4).build().unwrap();
        let (colliders, bodies, stats) =
            cook_colliders(&scene, &instances(&scene), &rules, None, Some(&jobs));
        assert_eq!(
            stats,
            ColliderStats {
                decomposed: 2,
                cached: 0
            }
        );
        assert_eq!(bodies.len(), 2);
        // The U (whose hull is a box): its two arms and its base.
        let u = &colliders[bodies[0].collider as usize].collider.shapes;
        assert_eq!(u.len(), 3, "{u:?}");
        let volume = hull_volumes(u);
        assert!((volume - 5.0).abs() < 0.05, "{volume}");
        // The crate, one part: its box, as before.
        let c = &colliders[bodies[1].collider as usize].collider.shapes;
        assert!(
            matches!(
                c[..],
                [ColliderShape {
                    shape: Shape::Box { .. },
                    ..
                }]
            ),
            "{c:?}"
        );
        // Asked for parts, the crate is its hull (one part).
        let rules = ColliderRules {
            bodies: DefaultBodies::Dynamic,
            dynamic_shape: DynamicShape::Parts,
            ..ColliderRules::default()
        };
        let (colliders, bodies, _) = cook_colliders(&scene, &instances(&scene), &rules, None, None);
        let c = &colliders[bodies[1].collider as usize].collider.shapes;
        assert!(
            matches!(
                c[..],
                [ColliderShape {
                    shape: Shape::Hull { .. },
                    ..
                }]
            ),
            "{c:?}"
        );
        // As a plain hull, the U fills its notch.
        let rules = ColliderRules {
            bodies: DefaultBodies::Dynamic,
            dynamic_shape: DynamicShape::Hull,
            ..ColliderRules::default()
        };
        let (colliders, bodies, stats) =
            cook_colliders(&scene, &instances(&scene), &rules, None, None);
        assert_eq!(stats.decomposed, 0);
        let u = &colliders[bodies[0].collider as usize].collider.shapes;
        assert!((hull_volumes(u) - 6.0).abs() < 1e-3);
    }

    #[test]
    fn convex_parts_come_from_the_cache_the_second_time() {
        let dir = std::env::temp_dir().join(format!("engine_parts_cache_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = DerivedDataCache::new(&dir).unwrap();
        let scene = ImportedScene {
            meshes: vec![mesh("u", vec![u_prism(0.5)])],
            nodes: vec![node("u", Some(0), Vec3::ZERO, Vec::new())],
            roots: vec![0],
            ..ImportedScene::default()
        };
        let rules = ColliderRules {
            bodies: DefaultBodies::Dynamic,
            ..ColliderRules::default()
        };
        let (first, _, stats) =
            cook_colliders(&scene, &instances(&scene), &rules, Some(&cache), None);
        assert_eq!(
            stats,
            ColliderStats {
                decomposed: 1,
                cached: 0
            }
        );
        let (second, _, stats) =
            cook_colliders(&scene, &instances(&scene), &rules, Some(&cache), None);
        assert_eq!(
            stats,
            ColliderStats {
                decomposed: 1,
                cached: 1
            }
        );
        assert_eq!(first, second);
        // Other settings, another entry.
        let finer = ColliderRules {
            parts: DecomposeSettings {
                threshold: 0.02,
                ..DecomposeSettings::default()
            },
            ..rules
        };
        let (_, _, stats) = cook_colliders(&scene, &instances(&scene), &finer, Some(&cache), None);
        assert_eq!(stats.cached, 0);
        // A torn entry is computed again, not trusted.
        let parts = parts_to_bytes(&[vec![Vec3::ZERO; 4]]);
        assert_eq!(parts_from_bytes(&parts).map(|p| p.len()), Some(1));
        assert_eq!(parts_from_bytes(&parts[..parts.len() - 1]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_gltf_bodies_take_convex_parts_of_their_mesh_colliders() {
        let collider = |node: usize| ColliderData {
            geometry: Some(Geometry::Node {
                node,
                convex_hull: false,
            }),
            material: None,
            filter: None,
            nodes: Vec::new(),
        };
        let mut body = node("body", Some(0), Vec3::new(0.0, 5.0, 0.0), Vec::new());
        body.physics = Some(NodePhysics {
            motion: Some(MotionData {
                kinematic: false,
                mass: None,
                linear_velocity: [0.0; 3],
                angular_velocity: [0.0; 3],
                gravity_factor: 1.0,
            }),
            collider: Some(collider(0)),
            ..NodePhysics::default()
        });
        // The same mesh as a static collider stays its triangles.
        let mut wall = node("wall", Some(0), Vec3::new(10.0, 0.0, 0.0), Vec::new());
        wall.physics = Some(NodePhysics {
            collider: Some(collider(1)),
            ..NodePhysics::default()
        });
        let scene = ImportedScene {
            meshes: vec![mesh("u", vec![u_prism(1.0)])],
            nodes: vec![body, wall],
            roots: vec![0, 1],
            ..ImportedScene::default()
        };
        let (colliders, bodies, stats) = cook_colliders(
            &scene,
            &instances(&scene),
            &ColliderRules::default(),
            None,
            None,
        );
        assert_eq!(stats.decomposed, 1);
        let dynamic = bodies.iter().find(|b| b.motion == Motion::Dynamic).unwrap();
        let shapes = &colliders[dynamic.collider as usize].collider.shapes;
        assert_eq!(shapes.len(), 3, "{shapes:?}");
        assert!((hull_volumes(shapes) - 5.0).abs() < 0.05);
        let fixed = bodies.iter().find(|b| b.motion == Motion::Static).unwrap();
        assert!(matches!(
            colliders[fixed.collider as usize].collider.shapes[..],
            [ColliderShape {
                shape: Shape::Mesh { .. },
                ..
            }]
        ));
    }

    #[test]
    fn props_get_a_fitted_primitive() {
        let scene = ImportedScene {
            meshes: vec![mesh(
                "crate",
                vec![cuboid(Vec3::splat(-0.5), Vec3::splat(0.5), None)],
            )],
            nodes: vec![node("crate", Some(0), Vec3::new(0.0, 3.0, 0.0), Vec::new())],
            roots: vec![0],
            ..ImportedScene::default()
        };
        let rules = ColliderRules {
            bodies: DefaultBodies::Dynamic,
            ..ColliderRules::default()
        };
        let (colliders, bodies, _) = cook_colliders(&scene, &instances(&scene), &rules, None, None);
        assert_eq!(bodies[0].motion, Motion::Dynamic);
        assert_eq!(bodies[0].instances, vec![0]);
        let Shape::Box { half_extents, .. } = colliders[0].collider.shapes[0].shape else {
            panic!("{:?}", colliders[0].collider.shapes[0].shape);
        };
        assert!(half_extents.distance(Vec3::splat(0.5)) < 1e-4);
    }

    #[test]
    fn authored_collision_meshes() {
        let scene = ImportedScene {
            meshes: vec![
                mesh(
                    "chair",
                    vec![cuboid(Vec3::ZERO, Vec3::new(1.0, 2.0, 1.0), None)],
                ),
                mesh("box", vec![cuboid(Vec3::ZERO, Vec3::ONE, None)]),
                mesh(
                    "ball",
                    vec![cuboid(Vec3::splat(-1.0), Vec3::splat(1.0), None)],
                ),
            ],
            nodes: vec![
                node("Chair", Some(0), Vec3::new(10.0, 0.0, 0.0), Vec::new()),
                // In world space beside the chair: its collider in the chair's space.
                node(
                    "UCX_Chair_01",
                    Some(1),
                    Vec3::new(10.0, 0.0, 0.0),
                    Vec::new(),
                ),
                node(
                    "UBX_Chair_02",
                    Some(1),
                    Vec3::new(10.0, 1.0, 0.0),
                    Vec::new(),
                ),
                // Names nothing: a collider of its own.
                node("USP_Rock", Some(2), Vec3::new(0.0, 0.0, 7.0), Vec::new()),
            ],
            roots: vec![0, 1, 2, 3],
            ..ImportedScene::default()
        };
        assert!(is_collision_node("UCX_Chair_01") && !is_collision_node("Chair"));
        let (colliders, bodies, _) = cook_colliders(
            &scene,
            &instances(&scene),
            &ColliderRules::default(),
            None,
            None,
        );
        assert_eq!(bodies.len(), 2);
        let chair = &colliders[bodies[0].collider as usize].collider;
        assert_eq!(chair.origin, Origin::Authored);
        assert_eq!(chair.shapes.len(), 2);
        assert!(matches!(&chair.shapes[0].shape, Shape::Hull { points } if points.len() == 8));
        let Shape::Box { center, .. } = chair.shapes[1].shape else {
            panic!("{:?}", chair.shapes[1].shape);
        };
        assert!(center.distance(Vec3::new(0.5, 1.5, 0.5)) < 1e-4, "{center}");
        let rock = &colliders[bodies[1].collider as usize].collider;
        assert!(
            matches!(rock.shapes[0].shape, Shape::Sphere { radius, .. } if (radius - 3f32.sqrt()).abs() < 1e-3)
        );
        assert_eq!(
            bodies[1].world_from_object,
            Mat4::from_translation(Vec3::new(0.0, 0.0, 7.0)).to_cols_array()
        );
    }

    #[test]
    fn a_shear_gets_a_collider_of_its_own() {
        let mut parent = node("parent", None, Vec3::new(0.0, 0.0, 1.0), vec![1]);
        parent.scale = [2.0, 1.0, 1.0];
        let mut child = node("child", Some(0), Vec3::ZERO, Vec::new());
        child.rotation = Quat::from_rotation_z(core::f32::consts::FRAC_PI_4).to_array();
        let scene = ImportedScene {
            meshes: vec![mesh(
                "cube",
                vec![cuboid(Vec3::splat(-0.5), Vec3::splat(0.5), None)],
            )],
            nodes: vec![parent, child],
            roots: vec![0],
            ..ImportedScene::default()
        };
        let (colliders, bodies, _) = cook_colliders(
            &scene,
            &instances(&scene),
            &ColliderRules::default(),
            None,
            None,
        );
        assert_eq!(bodies.len(), 1);
        // The shared collider and the sheared one.
        assert_eq!(colliders.len(), 2);
        assert_eq!(bodies[0].collider, 1);
        assert_eq!(
            bodies[0].world_from_object,
            Mat4::from_translation(Vec3::new(0.0, 0.0, 1.0)).to_cols_array()
        );
        let Shape::Mesh { vertices, .. } = &colliders[1].collider.shapes[0].shape else {
            panic!("not a mesh");
        };
        let reach = vertices.iter().map(|v| v.x).fold(f32::MIN, f32::max);
        assert!((reach - core::f32::consts::SQRT_2).abs() < 1e-4, "{reach}");
        // Made for the child, whose collider in its own space is the shared one: what a
        // model of the mesh takes (ADR 0117).
        let by_node = cook_node_colliders(&scene, &[], &ColliderRules::default(), None, None);
        assert_eq!(
            by_node.nodes,
            [BodyNode {
                node: 1,
                collider: 0
            }]
        );
    }

    #[test]
    fn gltf_physics_bodies_triggers_and_materials() {
        let mut body = node("body", Some(0), Vec3::new(0.0, 5.0, 0.0), vec![1]);
        body.physics = Some(NodePhysics {
            motion: Some(MotionData {
                kinematic: false,
                mass: Some(20.0),
                linear_velocity: [1.0, 0.0, 0.0],
                angular_velocity: [0.0; 3],
                gravity_factor: 1.0,
            }),
            ..NodePhysics::default()
        });
        // A child with an offset box: part of the body.
        let mut part = node("part", None, Vec3::new(1.0, 0.0, 0.0), Vec::new());
        part.physics = Some(NodePhysics {
            collider: Some(ColliderData {
                geometry: Some(Geometry::Shape(0)),
                material: Some(0),
                filter: None,
                nodes: Vec::new(),
            }),
            ..NodePhysics::default()
        });
        // A static floor and a trigger.
        let mut floor = node("floor", None, Vec3::ZERO, Vec::new());
        floor.physics = Some(NodePhysics {
            collider: Some(ColliderData {
                geometry: Some(Geometry::Shape(1)),
                material: None,
                filter: None,
                nodes: Vec::new(),
            }),
            ..NodePhysics::default()
        });
        let mut zone = node("zone", None, Vec3::new(0.0, 1.0, 0.0), Vec::new());
        zone.physics = Some(NodePhysics {
            trigger: Some(ColliderData {
                geometry: Some(Geometry::Shape(2)),
                material: None,
                filter: None,
                nodes: Vec::new(),
            }),
            ..NodePhysics::default()
        });
        let scene = ImportedScene {
            // Rendered, but in a file with physics only what it declares collides.
            meshes: vec![mesh("look", vec![cuboid(Vec3::ZERO, Vec3::ONE, None)])],
            nodes: vec![body, part, floor, zone],
            roots: vec![0, 2, 3],
            physics: PhysicsData {
                shapes: vec![
                    ImplicitShape::Box {
                        size: [0.5, 0.5, 0.5],
                    },
                    ImplicitShape::Plane {
                        size_x: Some(10.0),
                        size_z: Some(10.0),
                        double_sided: false,
                    },
                    ImplicitShape::Sphere { radius: 2.0 },
                ],
                materials: vec![PhysicsMaterialData {
                    static_friction: 0.9,
                    dynamic_friction: 0.8,
                    restitution: 0.3,
                }],
                filters: Vec::new(),
            },
            ..ImportedScene::default()
        };
        let (colliders, bodies, _) = cook_colliders(
            &scene,
            &instances(&scene),
            &ColliderRules::default(),
            None,
            None,
        );
        assert_eq!(bodies.len(), 3);
        let dynamic = &bodies[0];
        assert_eq!(dynamic.motion, Motion::Dynamic);
        assert_eq!(dynamic.mass, Some(20.0));
        assert_eq!(dynamic.friction, 0.8);
        assert_eq!(dynamic.restitution, 0.3);
        assert_eq!(dynamic.instances, vec![0, 1]);
        let Shape::Box {
            center,
            half_extents,
            ..
        } = colliders[dynamic.collider as usize].collider.shapes[0].shape
        else {
            panic!("not a box");
        };
        assert!(center.distance(Vec3::X) < 1e-5 && half_extents.distance(Vec3::splat(0.25)) < 1e-5);
        let floor = &bodies[1];
        assert_eq!(floor.motion, Motion::Static);
        assert!(!floor.sensor);
        let Shape::Box {
            center,
            half_extents,
            ..
        } = colliders[floor.collider as usize].collider.shapes[0].shape
        else {
            panic!("not a box");
        };
        // The plane's top face at its node's origin.
        assert!((center.y + half_extents.y).abs() < 1e-5 && (half_extents.x - 5.0).abs() < 1e-5);
        assert!(bodies[2].sensor);
    }

    #[test]
    fn the_rules_file_reads_every_field_and_each_meshs_own() {
        let rules = ColliderRules::parse(
            r##"// every field optional
(
    bodies: Dynamic,
    dynamic_shape: Hull,
    parts: (threshold: 0.03, max_parts: 16),
    no_collision: ["Leaves"],
    layer: "Props",
    meshes: {
        "Wall": (bodies: Static, layer: "Level"),
        "Banner": (collides: false),
        "#3": (dynamic_shape: Box),
    },
)"##,
        )
        .unwrap();
        assert_eq!(rules.bodies, DefaultBodies::Dynamic);
        assert_eq!(rules.dynamic_shape, DynamicShape::Hull);
        assert_eq!(rules.parts.threshold, 0.03);
        assert_eq!(rules.parts.max_hulls, Some(16));
        assert_eq!(rules.no_collision, ["Leaves"]);
        assert_eq!(rules.layer.as_deref(), Some("Props"));
        assert_eq!(rules.meshes["Wall"].bodies, Some(DefaultBodies::Static));
        assert_eq!(rules.meshes["Wall"].layer.as_deref(), Some("Level"));
        assert_eq!(rules.meshes["Banner"].bodies, Some(DefaultBodies::None));
        assert_eq!(rules.meshes["#3"].bodies, None);
        assert_eq!(rules.meshes["#3"].dynamic_shape, Some(DynamicShape::Box));
        // Nothing given: the defaults.
        assert_eq!(
            ColliderRules::parse("()").unwrap(),
            ColliderRules::default()
        );
        // A misspelled field is an error with its place.
        let e = ColliderRules::parse("(\n  bodys: Static,\n)").unwrap_err();
        assert!(e.message.starts_with("2:"), "{e}");
        assert!(e.message.contains("bodys"), "{e}");
    }

    #[test]
    fn a_meshs_own_rules_and_layers_reach_its_bodies() {
        let scene = ImportedScene {
            meshes: vec![
                mesh("Wall", vec![cuboid(Vec3::ZERO, Vec3::ONE, Some(0))]),
                mesh(
                    "Crate",
                    vec![cuboid(Vec3::splat(-0.5), Vec3::splat(0.5), Some(1))],
                ),
                mesh("Banner", vec![cuboid(Vec3::ZERO, Vec3::ONE, Some(0))]),
            ],
            materials: vec![material("stone"), material("wood")],
            nodes: vec![
                node("wall", Some(0), Vec3::ZERO, Vec::new()),
                node("crate", Some(1), Vec3::new(0.0, 3.0, 0.0), Vec::new()),
                node("banner", Some(2), Vec3::new(5.0, 0.0, 0.0), Vec::new()),
            ],
            roots: vec![0, 1, 2],
            ..ImportedScene::default()
        };
        let rules = ColliderRules::parse(
            r##"(
    layer: "Level",
    meshes: {
        "Crate": (bodies: Dynamic, dynamic_shape: Box, layer: "Props"),
        "#2": (collides: false),
    },
)"##,
        )
        .unwrap();
        let (colliders, bodies, _) = cook_colliders(&scene, &instances(&scene), &rules, None, None);
        assert_eq!(bodies.len(), 2, "the banner collides with nothing");
        let wall = &bodies[0];
        assert_eq!(wall.motion, Motion::Static);
        assert_eq!(wall.layer.as_deref(), Some("Level"));
        let shapes = &colliders[wall.collider as usize].collider.shapes;
        assert!(matches!(shapes[0].shape, Shape::Mesh { .. }));
        // A mesh's triangles carry their own surfaces, not the shape.
        assert_eq!(shapes[0].surface, u16::MAX);
        let crate_ = &bodies[1];
        assert_eq!(crate_.motion, Motion::Dynamic);
        assert_eq!(crate_.layer.as_deref(), Some("Props"));
        let shape = &colliders[crate_.collider as usize].collider.shapes[0];
        assert!(
            matches!(shape.shape, Shape::Box { .. }),
            "{:?}",
            shape.shape
        );
        // A prop is made of its mesh's material.
        assert_eq!(shape.surface, 1);
    }

    #[test]
    fn layer_names_go_through_the_bodies_entry() {
        let layers = [None, Some("Props"), Some("Level"), Some("Props")];
        let bodies: Vec<CookedBody> = layers
            .iter()
            .enumerate()
            .map(|(k, l)| CookedBody {
                collider: k as u32,
                instances: vec![k as u32],
                layer: l.map(str::to_owned),
                ..CookedBody::default()
            })
            .collect();
        let (meta, data) = bodies_entry(&bodies);
        assert_eq!(meta[2], 2, "each name once");
        let read = read_bodies_entry(meta, &data).unwrap();
        assert_eq!(read, bodies);
        // An entry written before layers had names: none.
        let (mut meta, mut data) = bodies_entry(&bodies[..1]);
        meta[2] = 0;
        meta[3] = 0;
        data.truncate(size_of::<BodyRecord>() + 4);
        assert_eq!(read_bodies_entry(meta, &data).unwrap()[0].layer, None);
        // A layer past the names is refused.
        let (meta, data) = bodies_entry(&bodies[1..2]);
        let mut broken = meta;
        broken[2] = 0;
        broken[3] = 0;
        assert!(read_bodies_entry(broken, &data).is_err());
    }

    #[test]
    fn a_gltf_collision_filter_names_its_bodys_layer() {
        let mut debris = node("debris", None, Vec3::new(0.0, 2.0, 0.0), Vec::new());
        debris.physics = Some(NodePhysics {
            motion: Some(MotionData {
                kinematic: false,
                mass: Some(1.0),
                linear_velocity: [0.0; 3],
                angular_velocity: [0.0; 3],
                gravity_factor: 1.0,
            }),
            collider: Some(ColliderData {
                geometry: Some(Geometry::Shape(0)),
                material: None,
                filter: Some(0),
                nodes: Vec::new(),
            }),
            ..NodePhysics::default()
        });
        let scene = ImportedScene {
            nodes: vec![debris],
            roots: vec![0],
            physics: PhysicsData {
                shapes: vec![ImplicitShape::Sphere { radius: 0.5 }],
                materials: Vec::new(),
                filters: vec![CollisionFilterData {
                    systems: vec!["Debris".to_owned(), "Small".to_owned()],
                }],
            },
            ..ImportedScene::default()
        };
        let (_, bodies, _) = cook_colliders(
            &scene,
            &instances(&scene),
            &ColliderRules::default(),
            None,
            None,
        );
        assert_eq!(bodies[0].layer.as_deref(), Some("Debris"));
    }
}
