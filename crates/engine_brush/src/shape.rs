//! The shapes brushes start as (ADR 0110), in a brush's own grid: its base's middle at the
//! origin, its height along +y, its size `[x, y, z]` in grid units.

use crate::exact::Plane;
use crate::polygon::{PlaneId, Planes};
use core::f64::consts::{PI, TAU};
use engine_core::math::DVec3;

/// The six planes of the box from `lo` to `hi` (+x, -x, +y, -y, +z, -z); `None` when a
/// side is out of bounds.
pub fn box_planes(lo: [i64; 3], hi: [i64; 3], table: &mut Planes) -> Option<Vec<PlaneId>> {
    Some(cuboid(lo, hi)?.into_iter().map(|p| table.add(p)).collect())
}

/// The six planes of the box from `lo` to `hi`, as values.
pub fn cuboid(lo: [i64; 3], hi: [i64; 3]) -> Option<Vec<Plane>> {
    let mut out = Vec::with_capacity(6);
    for axis in 0..3 {
        let mut n = [0; 3];
        n[axis] = 1;
        out.push(Plane::new(n, hi[axis])?);
        n[axis] = -1;
        out.push(Plane::new(n, -lo[axis])?);
    }
    Some(out)
}

/// What a brush starts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A box.
    Box,
    /// A ramp rising from its -z side to its +z side.
    Wedge,
    /// Stairs of this many steps rising towards +z, one part per step.
    Stairs(u32),
    /// A prism of this many sides around y, its section an ellipse's polygon.
    Cylinder(u32),
    /// A polyhedron of this many sides around y, of half as many rings.
    Sphere(u32),
}

/// The convex parts of `shape` of `size`, each its planes; `None` when the size is not
/// positive or out of bounds.
pub fn parts(shape: Shape, size: [i64; 3]) -> Option<Vec<Vec<Plane>>> {
    if size.iter().any(|&s| s <= 0) {
        return None;
    }
    let [sx, sy, sz] = size;
    let (x0, z0) = (-(sx / 2), -(sz / 2));
    let (x1, z1) = (x0 + sx, z0 + sz);
    match shape {
        Shape::Box => Some(vec![cuboid([x0, 0, z0], [x1, sy, z1])?]),
        Shape::Wedge => {
            // Through the bottom's -z edge and the top's +z edge, facing up and to -z.
            let slope = Plane::new([0, sz, -sy], -sy * z0).or_else(|| {
                Plane::from_f64(
                    DVec3::new(0.0, sz as f64, -sy as f64),
                    DVec3::new(0.0, 0.0, z0 as f64),
                )
            })?;
            Some(vec![vec![
                Plane::new([1, 0, 0], x1)?,
                Plane::new([-1, 0, 0], -x0)?,
                Plane::new([0, -1, 0], 0)?,
                Plane::new([0, 0, 1], z1)?,
                slope,
            ]])
        }
        Shape::Stairs(steps) => {
            let n = i64::from(steps.max(1));
            (0..n)
                .map(|i| {
                    let (a, b) = (z0 + i * sz / n, z0 + (i + 1) * sz / n);
                    cuboid([x0, 0, a], [x1, (i + 1) * sy / n, b])
                })
                .collect()
        }
        Shape::Cylinder(sides) => {
            let n = sides.max(3);
            let (a, b) = (sx as f64 / 2.0, sz as f64 / 2.0);
            let corner = |k: u32| {
                let t = TAU * f64::from(k) / f64::from(n) + PI / f64::from(n);
                DVec3::new(a * t.cos(), 0.0, b * t.sin())
            };
            let mut planes = vec![Plane::new([0, 1, 0], sy)?, Plane::new([0, -1, 0], 0)?];
            for k in 0..n {
                let (p, q) = (corner(k), corner(k + 1));
                let e = q - p;
                // Outwards: the side's direction turned a quarter about -y.
                let normal = DVec3::new(e.z, 0.0, -e.x);
                planes.push(Plane::from_f64(normal, p)?);
            }
            Some(vec![planes])
        }
        Shape::Sphere(sides) => {
            let segments = sides.max(4);
            let rings = (segments / 2).max(2);
            let radii = DVec3::new(sx as f64, sy as f64, sz as f64) / 2.0;
            let centre = DVec3::new(0.0, sy as f64 / 2.0, 0.0);
            let point = |i: u32, j: u32| {
                let phi = PI * f64::from(i) / f64::from(rings);
                let theta = TAU * f64::from(j) / f64::from(segments);
                centre
                    + radii
                        * DVec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin())
            };
            let mut planes = Vec::new();
            for i in 0..rings {
                for j in 0..segments {
                    // Three distinct corners of the face (the poles' faces are triangles).
                    let (a, b, c) = if i == 0 {
                        (point(0, j), point(1, j + 1), point(1, j))
                    } else {
                        (point(i, j), point(i, j + 1), point(i + 1, j))
                    };
                    let mut normal = (b - a).cross(c - a);
                    if normal.dot(a - centre) < 0.0 {
                        normal = -normal;
                    }
                    planes.push(Plane::from_f64(normal, a)?);
                }
            }
            Some(vec![planes])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::{Brush, Op, evaluate};
    use engine_core::math::DMat3;

    /// The volume a part's surface encloses (grid units cubed).
    fn volume(parts: &[Vec<Plane>], linear: DMat3, offset: DVec3) -> f64 {
        let mut table = Planes::new();
        let brush = Brush {
            op: Op::Add,
            parts: parts
                .iter()
                .map(|p| {
                    p.iter()
                        .map(|q| {
                            table.add(q.transformed(linear, offset).unwrap_or_else(|| panic!()))
                        })
                        .collect()
                })
                .collect(),
        };
        let e = evaluate(&[brush], &table);
        assert!(e.invalid.is_empty(), "{:?}", e.invalid);
        let mut v = 0.0;
        for (_, p) in &e.surface {
            let c: Vec<DVec3> = p.corners.iter().map(|c| c.to_f64()).collect();
            for i in 1..c.len() - 1 {
                v += c[0].dot(c[i].cross(c[i + 1])) / 6.0;
            }
        }
        // The corners are exact; only the sum rounds.
        (v * 1e3).round() / 1e3
    }

    #[test]
    fn shapes_have_their_volumes() {
        let id = (DMat3::IDENTITY, DVec3::ZERO);
        let size = [2000, 1000, 4000];
        let v = |s: Shape| volume(&parts(s, size).unwrap_or_else(|| panic!()), id.0, id.1);
        let full = 2000.0 * 1000.0 * 4000.0;
        assert_eq!(v(Shape::Box), full);
        assert_eq!(v(Shape::Wedge), full / 2.0);
        // Four steps: a quarter, a half, three quarters and all of the height.
        assert_eq!(v(Shape::Stairs(4)), full * (1.0 + 2.0 + 3.0 + 4.0) / 16.0);
        // A 32-sided ellipse's polygon: within 1 % of the ellipse.
        let cylinder = v(Shape::Cylinder(32));
        let ellipse = PI * 1000.0 * 2000.0 * 1000.0;
        assert!(
            (cylinder / ellipse - 1.0).abs() < 0.01,
            "{cylinder} against {ellipse}"
        );
        let sphere = v(Shape::Sphere(16));
        let ellipsoid = 4.0 / 3.0 * PI * 1000.0 * 500.0 * 2000.0;
        // Inscribed in the ellipsoid: a little smaller (16 sides, 8 rings: 6 %).
        assert!(
            sphere < ellipsoid && sphere > 0.9 * ellipsoid,
            "{sphere} against {ellipsoid}"
        );
        assert!(parts(Shape::Box, [0, 1, 1]).is_none());
    }

    #[test]
    fn moves_and_quarter_turns_are_exact_and_turns_round() {
        let wedge = parts(Shape::Wedge, [3000, 1000, 4000]).unwrap_or_else(|| panic!());
        for p in &wedge[0] {
            // Moved by whole units: the same normal, the offset by n·t.
            let t = DVec3::new(250.0, -1000.0, 7.0);
            let q = p
                .transformed(DMat3::IDENTITY, t)
                .unwrap_or_else(|| panic!());
            let n = p.n.map(i64::from);
            assert_eq!(q.n, p.n);
            assert_eq!(q.d, p.d + n[0] * 250 - n[1] * 1000 + n[2] * 7);
            // A quarter turn about y (with f32's rounding of it): the normal permuted.
            let turn =
                engine_core::math::Mat3::from_rotation_y(core::f32::consts::FRAC_PI_2).as_dmat3();
            let r = p.transformed(turn, DVec3::ZERO).unwrap_or_else(|| panic!());
            assert_eq!(r.n, [p.n[2], p.n[1], -p.n[0]], "{p:?}");
            assert_eq!(r.d, p.d);
        }
        // Turned by 30°: the volume stays within rounding.
        let turn = DMat3::from_rotation_y(PI / 6.0);
        let v = volume(&wedge, turn, DVec3::new(10_000.0, 0.0, 0.0));
        assert!((v / 6.0e9 - 1.0).abs() < 1e-6, "{v}");
    }
}
