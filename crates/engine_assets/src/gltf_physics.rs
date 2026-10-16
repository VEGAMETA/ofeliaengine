//! glTF physics (ADR 0101): the Khronos drafts `KHR_physics_rigid_bodies` (bodies, colliders,
//! triggers, materials) and `KHR_implicit_shapes` (spheres, boxes, capsules, cylinders,
//! planes), read from the file's JSON. Defaults are the drafts' schemas' (2026-10-16).
//!
//! Collision filters and joints are not read yet: the physics API has layers, not named
//! systems, and no joints of the drafts' kind.

use gltf::json::Value;

/// A shape of `KHR_implicit_shapes`, in its node's space (centred on the origin, along Y).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImplicitShape {
    /// A sphere.
    Sphere {
        /// Radius.
        radius: f32,
    },
    /// A box.
    Box {
        /// Its full size along X, Y and Z.
        size: [f32; 3],
    },
    /// A capsule: two spheres along Y and what joins them.
    Capsule {
        /// From one sphere's centre to the other's.
        height: f32,
        /// The upper sphere's radius.
        radius_top: f32,
        /// The lower sphere's radius.
        radius_bottom: f32,
    },
    /// A cylinder (a cone with one radius 0) along Y.
    Cylinder {
        /// Height.
        height: f32,
        /// The top disk's radius.
        radius_top: f32,
        /// The bottom disk's radius.
        radius_bottom: f32,
    },
    /// A plane through the origin, normal +Y; infinite along X or Z without a size.
    Plane {
        /// Size along X.
        size_x: Option<f32>,
        /// Size along Z.
        size_z: Option<f32>,
        /// Whether both sides collide.
        double_sided: bool,
    },
    /// A shape of another extension.
    Unknown,
}

/// A physics material.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicsMaterialData {
    /// Friction while still.
    pub static_friction: f32,
    /// Friction while sliding (what the body's friction becomes).
    pub dynamic_friction: f32,
    /// Restitution.
    pub restitution: f32,
}

/// What a collider or trigger is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geometry {
    /// A shape of `KHR_implicit_shapes`, by index.
    Shape(usize),
    /// A node's mesh (and its children's), in the collider node's space.
    Node {
        /// The node.
        node: usize,
        /// Its convex hull instead of its triangles.
        convex_hull: bool,
    },
}

/// A node's motion: it is a rigid body, and its subtree moves with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionData {
    /// Moved by the game, not by forces.
    pub kinematic: bool,
    /// Mass in kg; none: from the colliders' volume.
    pub mass: Option<f32>,
    /// Initial velocity in the node's space (m/s).
    pub linear_velocity: [f32; 3],
    /// Initial angular velocity in the node's space (rad/s).
    pub angular_velocity: [f32; 3],
    /// How much gravity acts on it.
    pub gravity_factor: f32,
}

/// A node's collider or trigger.
#[derive(Debug, Clone, PartialEq)]
pub struct ColliderData {
    /// What it is made of; none for a trigger made of other nodes' triggers.
    pub geometry: Option<Geometry>,
    /// Index into [`PhysicsData::materials`].
    pub material: Option<usize>,
    /// A compound trigger's nodes (each with a trigger of its own).
    pub nodes: Vec<usize>,
}

/// A node's `KHR_physics_rigid_bodies`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodePhysics {
    /// It is a rigid body.
    pub motion: Option<MotionData>,
    /// It collides.
    pub collider: Option<ColliderData>,
    /// It reports overlaps without colliding.
    pub trigger: Option<ColliderData>,
}

/// A file's shapes and physics materials.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhysicsData {
    /// `KHR_implicit_shapes.shapes`.
    pub shapes: Vec<ImplicitShape>,
    /// `KHR_physics_rigid_bodies.physicsMaterials`.
    pub materials: Vec<PhysicsMaterialData>,
}

fn number(v: &Value, key: &str, default: f32) -> f32 {
    v.get(key)
        .and_then(Value::as_f64)
        .map_or(default, |x| x as f32)
}

fn vector(v: &Value, key: &str, default: [f32; 3]) -> [f32; 3] {
    let Some(a) = v.get(key).and_then(Value::as_array) else {
        return default;
    };
    let mut out = default;
    for (o, x) in out.iter_mut().zip(a) {
        if let Some(x) = x.as_f64() {
            *o = x as f32;
        }
    }
    out
}

fn index(v: &Value, key: &str) -> Option<usize> {
    v.get(key).and_then(Value::as_u64).map(|i| i as usize)
}

fn implicit_shape(v: &Value) -> ImplicitShape {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
    let p = v.get(ty).unwrap_or(&Value::Null);
    match ty {
        "sphere" => ImplicitShape::Sphere {
            radius: number(p, "radius", 0.5),
        },
        "box" => ImplicitShape::Box {
            size: vector(p, "size", [1.0; 3]),
        },
        "capsule" => ImplicitShape::Capsule {
            height: number(p, "height", 0.5),
            radius_top: number(p, "radiusTop", 0.25),
            radius_bottom: number(p, "radiusBottom", 0.25),
        },
        "cylinder" => ImplicitShape::Cylinder {
            height: number(p, "height", 0.5),
            radius_top: number(p, "radiusTop", 0.25),
            radius_bottom: number(p, "radiusBottom", 0.25),
        },
        "plane" => ImplicitShape::Plane {
            size_x: p.get("sizeX").and_then(Value::as_f64).map(|x| x as f32),
            size_z: p.get("sizeZ").and_then(Value::as_f64).map(|x| x as f32),
            double_sided: p
                .get("doubleSided")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        _ => ImplicitShape::Unknown,
    }
}

fn material(v: &Value) -> PhysicsMaterialData {
    PhysicsMaterialData {
        static_friction: number(v, "staticFriction", 0.6),
        dynamic_friction: number(v, "dynamicFriction", 0.6),
        restitution: number(v, "restitution", 0.0),
    }
}

/// A collider's or trigger's geometry: `geometry: { shape | node, convexHull }`, or (earlier
/// drafts) `shape` on the collider itself.
fn geometry(v: &Value) -> Option<Geometry> {
    let g = v.get("geometry").unwrap_or(v);
    if let Some(shape) = index(g, "shape") {
        return Some(Geometry::Shape(shape));
    }
    index(g, "node").map(|node| Geometry::Node {
        node,
        convex_hull: g
            .get("convexHull")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn collider(v: &Value) -> ColliderData {
    ColliderData {
        geometry: geometry(v),
        material: index(v, "physicsMaterial"),
        nodes: v
            .get("nodes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|n| n.as_u64().map(|n| n as usize))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn motion(v: &Value) -> MotionData {
    // A mass of 0 stands for infinity: nothing moves it.
    let mass = v.get("mass").and_then(Value::as_f64).map(|m| m as f32);
    MotionData {
        kinematic: v
            .get("isKinematic")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || mass == Some(0.0),
        mass: mass.filter(|&m| m > 0.0),
        linear_velocity: vector(v, "linearVelocity", [0.0; 3]),
        angular_velocity: vector(v, "angularVelocity", [0.0; 3]),
        gravity_factor: number(v, "gravityFactor", 1.0),
    }
}

/// The file's shapes and physics materials.
pub(crate) fn scene_physics(document: &gltf::Document) -> PhysicsData {
    let shapes = document
        .extension_value("KHR_implicit_shapes")
        .and_then(|v| v.get("shapes"))
        .and_then(Value::as_array)
        .map(|a| a.iter().map(implicit_shape).collect())
        .unwrap_or_default();
    let materials = document
        .extension_value("KHR_physics_rigid_bodies")
        .and_then(|v| v.get("physicsMaterials"))
        .and_then(Value::as_array)
        .map(|a| a.iter().map(material).collect())
        .unwrap_or_default();
    PhysicsData { shapes, materials }
}

/// A node's rigid body, collider and trigger, if it has any.
pub(crate) fn node_physics(node: &gltf::Node<'_>) -> Option<NodePhysics> {
    let v = node.extension_value("KHR_physics_rigid_bodies")?;
    Some(NodePhysics {
        motion: v.get("motion").map(motion),
        collider: v.get("collider").map(collider),
        trigger: v.get("trigger").map(collider),
    })
}

/// What an authored collision mesh's name says (Unreal's convention): its shape, and the
/// name of the mesh or node it belongs to. `UCX_Chair_01` is a convex hull of Chair's;
/// `UBX_`, `USP_` and `UCP_` are a box, a sphere and a capsule fitted to it. A numbered
/// suffix (`_01`, Blender's `.001`) is dropped.
pub fn collision_name(name: &str) -> Option<(AuthoredKind, &str)> {
    let (kind, rest) = [
        ("UCX_", AuthoredKind::Hull),
        ("UBX_", AuthoredKind::Box),
        ("USP_", AuthoredKind::Sphere),
        ("UCP_", AuthoredKind::Capsule),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| name.strip_prefix(prefix).map(|rest| (kind, rest)))?;
    let trimmed = rest.trim_end_matches(|c: char| c.is_ascii_digit());
    let base = if trimmed.len() < rest.len() {
        trimmed
            .strip_suffix('_')
            .or_else(|| trimmed.strip_suffix('.'))
            .unwrap_or(rest)
    } else {
        rest
    };
    (!base.is_empty()).then_some((kind, base))
}

/// The shape an authored collision mesh stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthoredKind {
    /// `UCX_`: its convex hull.
    Hull,
    /// `UBX_`: a box fitted to it.
    Box,
    /// `USP_`: a sphere fitted to it.
    Sphere,
    /// `UCP_`: a capsule fitted to it.
    Capsule,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_names() {
        assert_eq!(
            collision_name("UCX_Chair"),
            Some((AuthoredKind::Hull, "Chair"))
        );
        assert_eq!(
            collision_name("UCX_Chair_01"),
            Some((AuthoredKind::Hull, "Chair"))
        );
        assert_eq!(
            collision_name("UBX_Big_Table.003"),
            Some((AuthoredKind::Box, "Big_Table"))
        );
        assert_eq!(
            collision_name("USP_Ball2"),
            Some((AuthoredKind::Sphere, "Ball2"))
        );
        assert_eq!(
            collision_name("UCP_Pillar_7"),
            Some((AuthoredKind::Capsule, "Pillar"))
        );
        assert_eq!(collision_name("UCX_"), None);
        assert_eq!(collision_name("Chair"), None);
        assert_eq!(collision_name("ucx_Chair"), None);
    }

    #[test]
    fn shapes_and_bodies_from_json() {
        let shape: Value = gltf::json::deserialize::from_str(
            r#"{ "type": "capsule", "capsule": { "height": 2.0, "radiusTop": 0.5 } }"#,
        )
        .unwrap();
        assert_eq!(
            implicit_shape(&shape),
            ImplicitShape::Capsule {
                height: 2.0,
                radius_top: 0.5,
                radius_bottom: 0.25
            }
        );
        let body: Value = gltf::json::deserialize::from_str(
            r#"{ "motion": { "mass": 0 }, "collider": { "geometry": { "node": 3, "convexHull": true }, "physicsMaterial": 1 } }"#,
        )
        .unwrap();
        let m = motion(&body["motion"]);
        assert!(m.kinematic && m.mass.is_none());
        let c = collider(&body["collider"]);
        assert_eq!(
            c.geometry,
            Some(Geometry::Node {
                node: 3,
                convex_hull: true
            })
        );
        assert_eq!(c.material, Some(1));
        // An earlier draft's form: the shape on the collider.
        let old: Value = gltf::json::deserialize::from_str(r#"{ "shape": 2 }"#).unwrap();
        assert_eq!(collider(&old).geometry, Some(Geometry::Shape(2)));
    }
}
