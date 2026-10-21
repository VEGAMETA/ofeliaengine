//! Triangles of a brush's polygons (ADR 0110), in its group's space.
//!
//! Each polygon is grown in its plane by a small distance before it is cut into a fan:
//! where a corner of one polygon lies on another's edge (a T-junction), the rounding of
//! the corners to f32 would otherwise open hairline cracks. Grown, neighbours overlap
//! along their shared edges instead.
//!
//! Polygons thinner than [`SLIVER`] are left out first: where rounded planes meet (a
//! torus's segments, turned brushes) the exact surface keeps strips a few nanometres wide,
//! which no pixel would show and growing would make visible.

use crate::polygon::{Planes, Polygon};
use engine_core::math::DVec3;

/// Polygons thinner than this many grid units (1 µm at 1 mm; twice the area over the
/// perimeter) make no triangles.
pub const SLIVER: f64 = 0.001;

/// Triangles for a mesh: corners in the group's grid units, their normals, texture
/// coordinates in metres on the axis plane each face is nearest (Quake's projection), and
/// three indices per triangle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Triangles {
    /// Corners, grid units.
    pub positions: Vec<DVec3>,
    /// Unit normals, one per corner.
    pub normals: Vec<DVec3>,
    /// Texture coordinates, metres.
    pub uvs: Vec<[f64; 2]>,
    /// Corners of the triangles, counter-clockwise from in front.
    pub indices: Vec<u32>,
}

impl Triangles {
    /// Empties it, keeping its memory.
    pub fn clear(&mut self) {
        self.positions.clear();
        self.normals.clear();
        self.uvs.clear();
        self.indices.clear();
    }
}

/// Adds `polygons`' triangles to `out`, each polygon grown by `grow` grid units in its
/// plane; `unit` is a grid unit in metres.
pub fn triangles<'a>(
    polygons: impl IntoIterator<Item = &'a Polygon>,
    planes: &Planes,
    grow: f64,
    unit: f64,
    out: &mut Triangles,
) {
    let mut corners: Vec<DVec3> = Vec::new();
    for polygon in polygons {
        let normal = planes.get(polygon.on).normal();
        corners.clear();
        corners.extend(polygon.corners.iter().map(|c| c.to_f64()));
        let m = corners.len();
        if m < 3 || is_sliver(&corners) {
            continue;
        }
        let first = out.positions.len() as u32;
        // Each edge's outward direction in the plane (counter-clockwise seen from in front).
        let outward = |i: usize| {
            (corners[(i + 1) % m] - corners[i])
                .cross(normal)
                .normalize_or_zero()
        };
        let axis = dominant(normal);
        for (i, &corner) in corners.iter().enumerate() {
            let (a, b) = (outward((i + m - 1) % m), outward(i));
            // The corner where both edges, moved out by `grow`, meet; sharp corners are
            // limited to ten times the distance.
            let miter = (a + b) / (1.0 + a.dot(b)).max(0.2);
            let p = corner + miter * grow;
            out.positions.push(p);
            out.normals.push(normal);
            let w = p * unit;
            out.uvs.push(match axis {
                0 => [-w.z * normal.x.signum(), w.y],
                1 => [w.x, -w.z * normal.y.signum()],
                _ => [w.x * normal.z.signum(), w.y],
            });
        }
        for i in 1..m as u32 - 1 {
            out.indices.extend([first, first + i, first + i + 1]);
        }
    }
}

/// A convex polygon's area (Newell's method).
pub fn area(corners: &[DVec3]) -> f64 {
    let mut n = DVec3::ZERO;
    for i in 1..corners.len().saturating_sub(1) {
        n += (corners[i] - corners[0]).cross(corners[i + 1] - corners[0]);
    }
    n.length() / 2.0
}

/// Whether a convex polygon is thinner than [`SLIVER`].
pub fn is_sliver(corners: &[DVec3]) -> bool {
    let perimeter: f64 = (0..corners.len())
        .map(|i| corners[i].distance(corners[(i + 1) % corners.len()]))
        .sum();
    perimeter == 0.0 || 2.0 * area(corners) / perimeter < SLIVER
}

/// The axis a normal is nearest.
fn dominant(n: DVec3) -> usize {
    let a = n.abs();
    if a.x >= a.y && a.x >= a.z {
        0
    } else if a.y >= a.z {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::{Brush, Op, evaluate};
    use crate::shape::box_planes;

    #[test]
    fn a_box_is_twelve_triangles_grown_out_and_facing_out() {
        let mut planes = Planes::new();
        let b = Brush {
            op: Op::Add,
            parts: vec![
                box_planes([0, 0, 0], [1000, 2000, 3000], &mut planes).unwrap_or_else(|| panic!()),
            ],
        };
        let e = evaluate(&[b], &planes);
        let mut t = Triangles::default();
        triangles(
            e.surface.iter().map(|(_, p)| p),
            &planes,
            0.05,
            0.001,
            &mut t,
        );
        assert_eq!((t.positions.len(), t.indices.len()), (24, 36));
        // Grown: every corner 0.05 out along both of its face's edges.
        for p in &t.positions {
            for (v, hi) in [(p.x, 1000.0), (p.y, 2000.0), (p.z, 3000.0)] {
                let near = (v + 0.05)
                    .abs()
                    .min((v - hi - 0.05).abs())
                    .min((v).abs())
                    .min((v - hi).abs());
                assert!(near < 1e-9, "{p}");
            }
        }
        // Counter-clockwise from in front: each triangle's normal is its corners'.
        for tri in t.indices.chunks(3) {
            let [a, b, c] = [0, 1, 2].map(|k| t.positions[tri[k] as usize]);
            let n = (b - a).cross(c - a).normalize();
            assert!(n.dot(t.normals[tri[0] as usize]) > 0.999);
        }
        // Metres on the axis plane: the top's corners span 1 × 3 m.
        let top: Vec<[f64; 2]> = (0..24)
            .filter(|&i| t.normals[i].y > 0.5)
            .map(|i| t.uvs[i])
            .collect();
        let span = |k: usize| {
            let (lo, hi) = top.iter().fold((f64::MAX, f64::MIN), |(l, h), uv| {
                (l.min(uv[k]), h.max(uv[k]))
            });
            hi - lo
        };
        assert!((span(0) - 1.0001).abs() < 1e-9 && (span(1) - 3.0001).abs() < 1e-9);
    }
}
