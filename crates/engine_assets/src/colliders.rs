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
//! 3. **The rules** ([`ColliderRules`]) for every other mesh node: by default static level
//!    geometry, its glTF mesh's triangles exactly as drawn (all primitives in one shape, each
//!    triangle's surface its primitive's material); a dynamic prop gets a fitted box,
//!    sphere, capsule or cylinder within 10 % of its hull's volume, else its hull (at most
//!    64 points). Skinned meshes get nothing.
//!
//! **Sharing and placement.** A collider is in its body's object space and shared by every
//! body using the same mesh; the body keeps its node's full transform and the runtime
//! places it at any scale (`engine_physics::colliders::placed`). A transform with shear
//! (a non-uniform scale above a rotation) is baked into a collider of its own here, so the
//! runtime never has to rebuild a mesh.

use crate::gltf_import::ImportedScene;
use crate::gltf_physics::{AuthoredKind, Geometry, ImplicitShape, collision_name};
use crate::pack::{EntryData, EntryKind, Pack, PackError, invalid};
use engine_collision::fit;
use engine_collision::hull::{Hull, HullSettings};
use engine_collision::mesh::weld_vertices;
use engine_collision::{Collider, ColliderShape, Origin, Shape};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat3, Mat4, Quat, Vec3};
use std::collections::HashMap;

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
    /// A box, sphere, capsule or cylinder within 10 % of its hull's volume, else the hull.
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
        DynamicShape::Hull => None,
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
    /// Shared colliders: (mesh, dynamic) → index.
    shared: HashMap<(usize, bool), Option<u32>>,
}

impl Builder<'_> {
    fn add(&mut self, shapes: Vec<Shape>, origin: Origin) -> Option<u32> {
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
                        shape,
                        surface: 0,
                    })
                    .collect(),
                origin,
            },
            baked: None,
        });
        Some((self.colliders.len() - 1) as u32)
    }

    /// A body; a transform with shear gets a collider of its own with the shear baked in.
    fn body(&mut self, collider: u32, world: Mat4, body: CookedBody) {
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

/// The scene's colliders and bodies (see the module's documentation). `node_instances`:
/// each node's render instances (first, count), as the cook laid them out.
pub fn cook_colliders(
    scene: &ImportedScene,
    node_instances: &[(u32, u32)],
    rules: &ColliderRules,
) -> (Vec<CookedCollider>, Vec<CookedBody>) {
    engine_profiler::zone!("assets::cook_colliders");
    let (world, parent) = world_transforms(scene);
    let mut b = Builder {
        scene,
        rules,
        world,
        colliders: Vec::new(),
        bodies: Vec::new(),
        shared: HashMap::new(),
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
        return (b.colliders, b.bodies);
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
            let motion = match rules.bodies {
                DefaultBodies::Dynamic => Motion::Dynamic,
                _ => Motion::Static,
            };
            if let Some(collider) = b.add(shapes, Origin::Authored) {
                let world = b.world[i];
                b.body(
                    collider,
                    world,
                    CookedBody {
                        motion,
                        instances: if motion == Motion::Static {
                            Vec::new()
                        } else {
                            instances_of(i)
                        },
                        ..CookedBody::default()
                    },
                );
            }
            continue;
        }
        let Some(mesh) = node.mesh else {
            continue;
        };
        if node.skin.is_some() || rules.bodies == DefaultBodies::None {
            continue;
        }
        let dynamic = rules.bodies == DefaultBodies::Dynamic;
        let collider = match b.shared.get(&(mesh, dynamic)) {
            Some(&c) => c,
            None => {
                let shape = if dynamic {
                    convex_shape(
                        &mesh_points(scene, mesh, Mat4::IDENTITY),
                        rules.dynamic_shape,
                    )
                } else if rules.static_shape == StaticShape::Mesh {
                    mesh_triangles(scene, mesh, rules, Mat4::IDENTITY)
                } else {
                    None
                };
                let c = b.add(shape.into_iter().collect(), Origin::Generated);
                b.shared.insert((mesh, dynamic), c);
                c
            }
        };
        if let Some(collider) = collider {
            let world = b.world[i];
            b.body(
                collider,
                world,
                CookedBody {
                    motion: if dynamic {
                        Motion::Dynamic
                    } else {
                        Motion::Static
                    },
                    instances: if dynamic { instances_of(i) } else { Vec::new() },
                    ..CookedBody::default()
                },
            );
        }
    }
    // Collision meshes that name nothing: static colliders of their own.
    for (c, kind) in orphans {
        let world = b.world[c];
        if let Some(shape) = shape_of(&b, c, kind, Mat4::IDENTITY)
            && let Some(collider) = b.add(vec![shape], Origin::Authored)
        {
            b.body(collider, world, CookedBody::default());
        }
    }
    (b.colliders, b.bodies)
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
    // The shapes of a geometry, in `into` (the body's object space).
    let geometry = |b: &Builder<'_>, g: &Geometry, into: Mat4| -> Vec<Shape> {
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
    // Motion bodies: the colliders of their subtrees.
    let mut grouped: HashMap<usize, (Vec<Shape>, Option<usize>)> = HashMap::new();
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
                    let shapes = geometry(b, g, into);
                    let entry = grouped.entry(root).or_default();
                    entry.0.extend(shapes);
                    entry.1 = entry.1.or(c.material);
                }
                None => statics.push((i, *g, c.material, sensor)),
            }
        }
    }
    let mut roots: Vec<usize> = grouped.keys().copied().collect();
    roots.sort_unstable();
    for root in roots {
        let (shapes, mat) = grouped.remove(&root).unwrap_or_default();
        let Some(motion) = scene.nodes[root].physics.as_ref().and_then(|p| p.motion) else {
            continue;
        };
        let Some(collider) = b.add(shapes, Origin::Authored) else {
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
                ..CookedBody::default()
            },
        );
    }
    for (i, g, mat, sensor) in statics {
        let shapes = geometry(b, &g, Mat4::IDENTITY);
        let Some(collider) = b.add(shapes, Origin::Authored) else {
            continue;
        };
        let m = material(mat);
        let world = b.world[i];
        b.body(
            collider,
            world,
            CookedBody {
                sensor,
                friction: m.map_or(0.5, |m| m.dynamic_friction),
                restitution: m.map_or(0.0, |m| m.restitution),
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
    /// Reserved, 0.
    pub reserved: [u32; 2],
    /// Its node's object to world, column major.
    pub world_from_object: [f32; 16],
}

const _: () = assert!(size_of::<BodyRecord>() == 128);

/// The bodies entry: `meta = [bodies, instance indices]`; the [`BodyRecord`]s, then the
/// instance indices they list.
pub(crate) fn bodies_entry(bodies: &[CookedBody]) -> ([u32; 7], Vec<u8>) {
    let mut records = Vec::with_capacity(bodies.len());
    let mut instances: Vec<u32> = Vec::new();
    for b in bodies {
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
            reserved: [0; 2],
            world_from_object: b.world_from_object,
        });
        instances.extend_from_slice(&b.instances);
    }
    let mut data = bytemuck::cast_slice::<BodyRecord, u8>(&records).to_vec();
    data.extend_from_slice(bytemuck::cast_slice(&instances));
    let meta = [records.len() as u32, instances.len() as u32, 0, 0, 0, 0, 0];
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
        let (colliders, bodies) =
            cook_colliders(&scene, &instances(&scene), &ColliderRules::default());
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
        let (colliders, _) = cook_colliders(&scene, &instances(&scene), &rules);
        let Shape::Mesh { triangles, .. } = &colliders[0].collider.shapes[0].shape else {
            panic!("not a mesh");
        };
        assert_eq!(triangles.len(), 12);
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
        let (colliders, bodies) = cook_colliders(&scene, &instances(&scene), &rules);
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
        let (colliders, bodies) =
            cook_colliders(&scene, &instances(&scene), &ColliderRules::default());
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
        let (colliders, bodies) =
            cook_colliders(&scene, &instances(&scene), &ColliderRules::default());
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
                nodes: Vec::new(),
            }),
            ..NodePhysics::default()
        });
        let mut zone = node("zone", None, Vec3::new(0.0, 1.0, 0.0), Vec::new());
        zone.physics = Some(NodePhysics {
            trigger: Some(ColliderData {
                geometry: Some(Geometry::Shape(2)),
                material: None,
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
            },
            ..ImportedScene::default()
        };
        let (colliders, bodies) =
            cook_colliders(&scene, &instances(&scene), &ColliderRules::default());
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
}
