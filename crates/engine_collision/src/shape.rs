//! Colliders in the engine's own form (ADR 0101): what the cook makes, what the pack keeps
//! for the runtime's rebuilds, and what the editor will edit (a hull's points, faces and
//! edges; a primitive's parameters). Nothing here depends on a physics backend.
//!
//! **Scale.** [`Shape::transformed`] gives what a shape becomes under any affine map:
//! uniform or not, mirrored, sheared. Exact where the result is still that kind of shape or
//! a hull of exact points; round shapes that stop being round become hulls within a
//! tolerance. A decomposed prop never needs decomposing again: a linear map keeps convex
//! parts convex and their union the mapped prop.

use crate::fit::Primitive;
use core::f32::consts::{PI, TAU};
use engine_core::math::{Affine3A, Mat3, Quat, Vec3};

/// A shape of a collider, in the collider's frame (its mesh's object space).
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// An oriented box.
    Box {
        /// Centre.
        center: Vec3,
        /// From the box's axes to the collider's frame.
        rotation: Quat,
        /// Half the size along each of its axes.
        half_extents: Vec3,
    },
    /// A sphere.
    Sphere {
        /// Centre.
        center: Vec3,
        /// Radius.
        radius: f32,
    },
    /// A capsule: a segment along its local Y swept by a sphere.
    Capsule {
        /// Centre.
        center: Vec3,
        /// From its local frame to the collider's.
        rotation: Quat,
        /// Half the segment's length.
        half_height: f32,
        /// Radius.
        radius: f32,
    },
    /// A cylinder along its local Y.
    Cylinder {
        /// Centre.
        center: Vec3,
        /// From its local frame to the collider's.
        rotation: Quat,
        /// Half its height.
        half_height: f32,
        /// Radius.
        radius: f32,
    },
    /// The convex hull of these points, in the collider's frame. A hull has no rotation of
    /// its own, so a physics backend can scale it any way (ADR 0101).
    Hull {
        /// Its points (its faces follow from them).
        points: Vec<Vec3>,
    },
    /// A triangle mesh.
    Mesh {
        /// Vertices.
        vertices: Vec<Vec3>,
        /// Triangles, counter-clockwise seen from outside.
        triangles: Vec<[u32; 3]>,
        /// Each triangle's surface (what it is made of: footsteps, impacts), or empty: all
        /// the shape's own.
        surfaces: Vec<u16>,
    },
}

/// One of a collider's shapes.
#[derive(Debug, Clone, PartialEq)]
pub struct ColliderShape {
    /// Stable within its collider: what the editor selects and its commands name, kept
    /// across edits.
    pub id: u32,
    /// The shape.
    pub shape: Shape,
    /// What it is made of (for a mesh: its triangles without a surface of their own).
    pub surface: u16,
}

/// Where a collider came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Origin {
    /// Made by the cook from its mesh and rules: made again when they change.
    #[default]
    Generated,
    /// Taken from the source file (glTF physics extensions, `UCX_` names).
    Authored,
    /// Changed by hand: source data, kept as it is.
    Edited,
}

/// A collider: the shapes of one mesh (ADR 0101).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Collider {
    /// Its shapes.
    pub shapes: Vec<ColliderShape>,
    /// Where it came from.
    pub origin: Origin,
}

impl From<Primitive> for Shape {
    fn from(p: Primitive) -> Self {
        match p {
            Primitive::Box(b) => Self::Box {
                center: b.center,
                rotation: b.rotation,
                half_extents: b.half_extents,
            },
            Primitive::Sphere(s) => Self::Sphere {
                center: s.center,
                radius: s.radius,
            },
            Primitive::Capsule(c) => Self::Capsule {
                center: c.center,
                rotation: c.rotation,
                half_height: c.half_height,
                radius: c.radius,
            },
            Primitive::Cylinder(c) => Self::Cylinder {
                center: c.center,
                rotation: c.rotation,
                half_height: c.half_height,
                radius: c.radius,
            },
        }
    }
}

/// The rotation and the scales of a linear map whose columns are orthogonal (an axis
/// keeps its direction up to a rotation), or none if it shears them. A mirror flips the
/// last axis back, which a box, capsule or cylinder does not notice.
fn rotation_and_scale(axes: Mat3) -> Option<(Quat, Vec3)> {
    let c = [axes.x_axis, axes.y_axis, axes.z_axis];
    let s = Vec3::new(c[0].length(), c[1].length(), c[2].length());
    if s.min_element() <= 1e-12 * s.max_element().max(1e-30) {
        return None;
    }
    for (i, j) in [(0, 1), (0, 2), (1, 2)] {
        if c[i].dot(c[j]).abs() > 1e-5 * s[i] * s[j] {
            return None;
        }
    }
    let (x, y) = (c[0] / s.x, c[1] / s.y);
    // Right-handed again whether or not the map mirrors.
    let z = x.cross(y);
    let rotation = Quat::from_mat3(&Mat3::from_cols(x, y, z)).normalize();
    Some((rotation, s))
}

/// Points on the unit sphere (a Fibonacci spiral) for a hull within `relative` of it: the
/// faces between them dip by about π / 2n, so the points sit that much outside the sphere
/// over two, splitting the error.
fn sphere_points(relative: f32) -> Vec<Vec3> {
    let n = (PI / (2.0 * relative.max(1e-6))).clamp(32.0, 256.0) as usize;
    let lift = 1.0 + PI / (4.0 * n as f32);
    let golden = PI * (3.0 - 5f32.sqrt());
    (0..n)
        .map(|k| {
            let y = 1.0 - 2.0 * (k as f32 + 0.5) / n as f32;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let a = golden * k as f32;
            Vec3::new(r * a.cos(), y, r * a.sin()) * lift
        })
        .collect()
}

/// Points on the unit circle in the XZ plane for a polygon within `relative` of it.
fn circle_points(relative: f32) -> Vec<Vec3> {
    let n = (PI / (2.0 * relative.max(1e-6)).sqrt()).clamp(12.0, 120.0) as usize;
    // Halfway out between the polygon's vertices and its edges' middles.
    let lift = 1.0 + 0.5 * (1.0 - (PI / n as f32).cos());
    (0..n)
        .map(|k| {
            let a = TAU * k as f32 / n as f32;
            Vec3::new(a.cos(), 0.0, a.sin()) * lift
        })
        .collect()
}

impl Shape {
    /// Points whose convex hull is the shape: a box's corners, a hull's points, a mesh's
    /// vertices; round shapes' surfaces sampled so that the hull lies within `tolerance`
    /// of them (scene units).
    pub fn hull_points(&self, tolerance: f32) -> Vec<Vec3> {
        match self {
            Self::Box {
                center,
                rotation,
                half_extents,
            } => (0..8)
                .map(|i| {
                    let corner = Vec3::new(
                        if i & 1 == 0 { -1.0 } else { 1.0 },
                        if i & 2 == 0 { -1.0 } else { 1.0 },
                        if i & 4 == 0 { -1.0 } else { 1.0 },
                    );
                    *center + *rotation * (corner * *half_extents)
                })
                .collect(),
            Self::Sphere { center, radius } => sphere_points(tolerance / radius.max(1e-30))
                .into_iter()
                .map(|p| *center + p * *radius)
                .collect(),
            Self::Capsule {
                center,
                rotation,
                half_height,
                radius,
            } => {
                let relative = tolerance / (radius + half_height).max(1e-30);
                let mut local: Vec<Vec3> = sphere_points(relative)
                    .into_iter()
                    .map(|p| {
                        let y = if p.y >= 0.0 {
                            *half_height
                        } else {
                            -*half_height
                        };
                        p * *radius + Vec3::new(0.0, y, 0.0)
                    })
                    .collect();
                // The straight part's ends.
                for p in circle_points(relative) {
                    local.push(p * *radius + Vec3::new(0.0, *half_height, 0.0));
                    local.push(p * *radius - Vec3::new(0.0, *half_height, 0.0));
                }
                local.into_iter().map(|p| *center + *rotation * p).collect()
            }
            Self::Cylinder {
                center,
                rotation,
                half_height,
                radius,
            } => {
                let mut points = Vec::new();
                for p in circle_points(tolerance / radius.max(1e-30)) {
                    for y in [*half_height, -*half_height] {
                        points.push(*center + *rotation * (p * *radius + Vec3::new(0.0, y, 0.0)));
                    }
                }
                points
            }
            Self::Hull { points } => points.clone(),
            Self::Mesh { vertices, .. } => vertices.clone(),
        }
    }

    /// Volume (a mesh's: what its triangles enclose, meaningful when it is closed).
    pub fn volume(&self) -> f32 {
        match self {
            Self::Box { half_extents, .. } => {
                8.0 * half_extents.x * half_extents.y * half_extents.z
            }
            Self::Sphere { radius, .. } => 4.0 / 3.0 * PI * radius.powi(3),
            Self::Capsule {
                half_height,
                radius,
                ..
            } => PI * radius * radius * 2.0 * half_height + 4.0 / 3.0 * PI * radius.powi(3),
            Self::Cylinder {
                half_height,
                radius,
                ..
            } => PI * radius * radius * 2.0 * half_height,
            Self::Hull { points } => crate::hull::Hull::new(points).map_or(0.0, |h| h.volume()),
            Self::Mesh {
                vertices,
                triangles,
                ..
            } => crate::mesh::volume(vertices, triangles),
        }
    }

    /// What this shape becomes under `map` (any affine map: scale, rotation, mirror, shear,
    /// translation). Round shapes that stop being round become hulls whose surface lies
    /// within `tolerance` (scene units, after the map) of the true one.
    pub fn transformed(&self, map: Affine3A, tolerance: f32) -> Self {
        let linear = Mat3::from(map.matrix3);
        let point = |p: Vec3| map.transform_point3(p);
        match self {
            Self::Box {
                center,
                rotation,
                half_extents,
            } => match rotation_and_scale(linear * Mat3::from_quat(*rotation)) {
                Some((rotation, s)) => Self::Box {
                    center: point(*center),
                    rotation,
                    half_extents: *half_extents * s,
                },
                // A parallelepiped: the hull of its corners, exactly.
                None => Self::Hull {
                    points: self.hull_points(0.0).into_iter().map(point).collect(),
                },
            },
            Self::Sphere { center, radius } => match rotation_and_scale(linear) {
                Some((_, s)) if s.max_element() - s.min_element() <= 1e-6 * s.max_element() => {
                    Self::Sphere {
                        center: point(*center),
                        radius: radius * s.x,
                    }
                }
                // The tolerance in the shape's own units.
                _ => Self::Hull {
                    points: self
                        .hull_points(tolerance / column_max(linear).max(1e-30))
                        .into_iter()
                        .map(point)
                        .collect(),
                },
            },
            Self::Capsule {
                center,
                rotation,
                half_height,
                radius,
            } => match rotation_and_scale(linear * Mat3::from_quat(*rotation)) {
                Some((r, s)) if (s.x - s.z).abs() <= 1e-6 * s.x => Self::Capsule {
                    center: point(*center),
                    rotation: r,
                    half_height: half_height * s.y,
                    radius: radius * s.x,
                },
                // The tolerance in the shape's own units.
                _ => Self::Hull {
                    points: self
                        .hull_points(tolerance / column_max(linear).max(1e-30))
                        .into_iter()
                        .map(point)
                        .collect(),
                },
            },
            Self::Cylinder {
                center,
                rotation,
                half_height,
                radius,
            } => match rotation_and_scale(linear * Mat3::from_quat(*rotation)) {
                Some((r, s)) if (s.x - s.z).abs() <= 1e-6 * s.x => Self::Cylinder {
                    center: point(*center),
                    rotation: r,
                    half_height: half_height * s.y,
                    radius: radius * s.x,
                },
                // The tolerance in the shape's own units.
                _ => Self::Hull {
                    points: self
                        .hull_points(tolerance / column_max(linear).max(1e-30))
                        .into_iter()
                        .map(point)
                        .collect(),
                },
            },
            Self::Hull { points } => Self::Hull {
                points: points.iter().map(|&p| point(p)).collect(),
            },
            Self::Mesh {
                vertices,
                triangles,
                surfaces,
            } => {
                // A mirror turns the triangles inside out: wind them back.
                let flip = linear.determinant() < 0.0;
                Self::Mesh {
                    vertices: vertices.iter().map(|&p| point(p)).collect(),
                    triangles: triangles
                        .iter()
                        .map(|&[a, b, c]| if flip { [a, c, b] } else { [a, b, c] })
                        .collect(),
                    surfaces: surfaces.clone(),
                }
            }
        }
    }
}

/// The longest a unit vector gets under `linear` (bounded by its largest column's length
/// times √3; the columns' longest is close enough for sizing a tolerance).
fn column_max(linear: Mat3) -> f32 {
    linear
        .x_axis
        .length()
        .max(linear.y_axis.length())
        .max(linear.z_axis.length())
}

impl Collider {
    /// Every shape under `map` (see [`Shape::transformed`]); ids and surfaces stay.
    pub fn transformed(&self, map: Affine3A, tolerance: f32) -> Self {
        Self {
            shapes: self
                .shapes
                .iter()
                .map(|s| ColliderShape {
                    id: s.id,
                    shape: s.shape.transformed(map, tolerance),
                    surface: s.surface,
                })
                .collect(),
            origin: self.origin,
        }
    }

    /// The next free shape id.
    pub fn next_id(&self) -> u32 {
        self.shapes.iter().map(|s| s.id + 1).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::EulerRot;

    fn turned() -> Quat {
        Quat::from_euler(EulerRot::YXZ, 0.6, 0.3, -0.9)
    }

    fn close(a: f32, b: f32, relative: f32) -> bool {
        (a - b).abs() <= relative * a.abs().max(b.abs())
    }

    #[test]
    fn a_box_scaled_along_its_axes_stays_a_box() {
        let r = turned();
        let shape = Shape::Box {
            center: Vec3::new(1.0, 2.0, 3.0),
            rotation: r,
            half_extents: Vec3::new(0.5, 1.0, 2.0),
        };
        // A scale along the box's own axes, seen from the collider's frame.
        let local = Mat3::from_quat(r)
            * Mat3::from_diagonal(Vec3::new(2.0, 3.0, 0.5))
            * Mat3::from_quat(r.inverse());
        let map = Affine3A::from_mat3_translation(local, Vec3::new(0.0, 5.0, 0.0));
        let Shape::Box {
            center,
            half_extents,
            ..
        } = shape.transformed(map, 1e-3)
        else {
            panic!("not a box");
        };
        assert!(center.distance(map.transform_point3(Vec3::new(1.0, 2.0, 3.0))) < 1e-5);
        assert!(
            half_extents.distance(Vec3::new(1.0, 3.0, 1.0)) < 1e-5,
            "{half_extents}"
        );
    }

    #[test]
    fn a_turned_box_under_another_scale_becomes_its_exact_hull() {
        let shape = Shape::Box {
            center: Vec3::ZERO,
            rotation: Quat::from_rotation_z(PI / 4.0),
            half_extents: Vec3::splat(0.5),
        };
        let map = Affine3A::from_scale(Vec3::new(3.0, 1.0, 1.0));
        let out = shape.transformed(map, 1e-3);
        let Shape::Hull { points } = &out else {
            panic!("{out:?}");
        };
        assert_eq!(points.len(), 8);
        // Volume times the map's determinant, exactly.
        assert!(close(out.volume(), 3.0, 1e-5), "{}", out.volume());
    }

    #[test]
    fn spheres() {
        let shape = Shape::Sphere {
            center: Vec3::new(1.0, 0.0, 0.0),
            radius: 0.5,
        };
        let uniform = shape.transformed(
            Affine3A::from_scale_rotation_translation(Vec3::splat(-2.0), turned(), Vec3::ZERO),
            1e-3,
        );
        assert!(
            matches!(uniform, Shape::Sphere { radius, .. } if close(radius, 1.0, 1e-6)),
            "{uniform:?}"
        );
        // An ellipsoid: a hull within the tolerance of it.
        let scale = Vec3::new(2.0, 1.0, 0.5);
        let tolerance = 0.01;
        let out = shape.transformed(Affine3A::from_scale(scale), tolerance);
        let Shape::Hull { points } = &out else {
            panic!("{out:?}");
        };
        assert!(points.len() <= 256);
        let a = scale * 0.5;
        for p in points {
            let q = (*p - Vec3::new(2.0, 0.0, 0.0)) / a;
            // Radial error in the ellipsoid's units, times its largest axis.
            assert!(
                (q.length() - 1.0).abs() * a.max_element() <= tolerance,
                "{p}"
            );
        }
        let exact = 4.0 / 3.0 * PI * a.x * a.y * a.z;
        assert!(
            close(out.volume(), exact, 0.03),
            "{} against {exact}",
            out.volume()
        );
    }

    #[test]
    fn capsules_and_cylinders() {
        let capsule = Shape::Capsule {
            center: Vec3::ZERO,
            rotation: turned(),
            half_height: 1.0,
            radius: 0.25,
        };
        let cylinder = Shape::Cylinder {
            center: Vec3::ZERO,
            rotation: turned(),
            half_height: 1.0,
            radius: 0.25,
        };
        // Along the shapes' own axes: radial scale 2, axial 3.
        let local = Mat3::from_quat(turned())
            * Mat3::from_diagonal(Vec3::new(2.0, 3.0, 2.0))
            * Mat3::from_quat(turned().inverse());
        let map = Affine3A::from_mat3(local);
        assert!(
            matches!(capsule.transformed(map, 1e-3), Shape::Capsule { half_height, radius, .. } if close(half_height, 3.0, 1e-5) && close(radius, 0.5, 1e-5))
        );
        assert!(
            matches!(cylinder.transformed(map, 1e-3), Shape::Cylinder { half_height, radius, .. } if close(half_height, 3.0, 1e-5) && close(radius, 0.5, 1e-5))
        );
        // Squashed across: hulls with about the scaled volume.
        let map = Affine3A::from_scale(Vec3::new(1.0, 1.0, 0.2));
        for shape in [&capsule, &cylinder] {
            let out = shape.transformed(map, 1e-3);
            assert!(matches!(out, Shape::Hull { .. }), "{out:?}");
            assert!(
                close(out.volume(), shape.volume() * 0.2, 0.02),
                "{} against {}",
                out.volume(),
                shape.volume() * 0.2
            );
        }
    }

    #[test]
    fn mirrors() {
        let shape = Shape::Box {
            center: Vec3::ZERO,
            rotation: turned(),
            half_extents: Vec3::new(0.1, 0.2, 0.3),
        };
        let mirror = Affine3A::from_scale(Vec3::new(-1.0, 1.0, 1.0));
        let Shape::Box {
            rotation,
            half_extents,
            ..
        } = shape.transformed(mirror, 1e-3)
        else {
            panic!("not a box");
        };
        assert!(half_extents.distance(Vec3::new(0.1, 0.2, 0.3)) < 1e-6);
        assert!((Mat3::from_quat(rotation).determinant() - 1.0).abs() < 1e-5);
        // A closed mesh keeps a positive volume: its triangles are wound back.
        let cube = crate::mesh::tests::cube();
        let mesh = Shape::Mesh {
            vertices: cube.0,
            triangles: cube.1,
            surfaces: Vec::new(),
        };
        let out = mesh.transformed(mirror, 1e-3);
        assert!(close(out.volume(), 1.0, 1e-5), "{}", out.volume());
    }

    #[test]
    fn convex_parts_scale_with_their_prop() {
        // Two parts of an L: the union's volume follows the determinant under any map.
        let parts = [
            Shape::Hull {
                points: crate::mesh::tests::cube().0,
            },
            Shape::Hull {
                points: crate::mesh::tests::cube()
                    .0
                    .into_iter()
                    .map(|p| p * Vec3::new(3.0, 1.0, 1.0) + Vec3::new(1.0, 0.0, 0.0))
                    .collect(),
            },
        ];
        let map = Affine3A::from_mat3(Mat3::from_cols(
            Vec3::new(2.0, 0.3, 0.0),
            Vec3::new(0.0, 0.5, 0.1),
            Vec3::new(0.2, 0.0, -1.5),
        ));
        let det = Mat3::from(map.matrix3).determinant().abs();
        for part in &parts {
            let out = part.transformed(map, 1e-3);
            assert!(close(out.volume(), part.volume() * det, 1e-4));
        }
    }
}
