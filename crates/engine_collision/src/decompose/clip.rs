//! A closed mesh cut by a plane into two closed meshes (ADR 0101, the decomposition's cut):
//! the triangles on each side, the ones the plane crosses split along it, and the cut
//! itself capped on both. Written for this engine after CoACD's `Clip` (MIT); no code was
//! copied.
//!
//! - Vertices within the tolerance of the plane are taken as on it: the two sides then agree
//!   on every point of the cut.
//! - A triangle in the plane belongs to the side behind it (its normal points out of that
//!   side's solid).
//! - Each side's cap closes the outline its own triangles leave in the plane (directed
//!   edges in the plane without their reverse), so a face lying in the plane stays on one
//!   side only. The outline's loops are nested by their signed area (outer boundaries and
//!   holes) and triangulated (`super::triangulate`).

use super::triangulate::{contains, signed_area, triangulate};
use engine_core::math::{DVec2, DVec3};
use std::collections::HashMap;

/// A closed triangle mesh, counter-clockwise from outside.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Solid {
    /// Vertices.
    pub(crate) points: Vec<DVec3>,
    /// Triangles.
    pub(crate) triangles: Vec<[u32; 3]>,
}

/// A plane: points `p` with `normal · p = offset` (`normal` of unit length).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Plane {
    pub(crate) normal: DVec3,
    pub(crate) offset: f64,
}

impl Plane {
    pub(crate) fn distance(&self, p: DVec3) -> f64 {
        self.normal.dot(p) - self.offset
    }
}

impl Solid {
    /// Volume (positive for a closed, outward-wound mesh).
    pub(crate) fn volume(&self) -> f64 {
        let Some(&o) = self.points.first() else {
            return 0.0;
        };
        let mut six = 0.0;
        for t in &self.triangles {
            let [a, b, c] = t.map(|i| self.points[i as usize] - o);
            six += a.dot(b.cross(c));
        }
        six / 6.0
    }

    /// Surface area.
    pub(crate) fn area(&self) -> f64 {
        self.triangles
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| self.points[i as usize]);
                0.5 * (b - a).cross(c - a).length()
            })
            .sum()
    }

    /// Bounds.
    pub(crate) fn bounds(&self) -> (DVec3, DVec3) {
        self.points.iter().fold(
            (DVec3::splat(f64::MAX), DVec3::splat(f64::MIN)),
            |(lo, hi), &p| (lo.min(p), hi.max(p)),
        )
    }
}

/// The two sides of `solid` cut by `plane`: (positive side, negative side), each closed.
/// `tolerance`: how close to the plane a vertex counts as on it. Either side is empty when
/// the plane does not cut the solid.
pub(crate) fn clip(solid: &Solid, plane: &Plane, tolerance: f64) -> (Solid, Solid) {
    let n = solid.points.len();
    // Side of each vertex: +1, -1 or 0 (on the plane, snapped onto it).
    let mut side = Vec::with_capacity(n);
    let mut points: Vec<DVec3> = Vec::with_capacity(n + 64);
    for &p in &solid.points {
        let d = plane.distance(p);
        if d > tolerance {
            side.push(1i8);
            points.push(p);
        } else if d < -tolerance {
            side.push(-1);
            points.push(p);
        } else {
            side.push(0);
            points.push(p - plane.normal * d);
        }
    }
    if side.iter().all(|&s| s >= 0) || side.iter().all(|&s| s <= 0) {
        // Nothing to cut: the solid is on one side (its faces in the plane included).
        return if side.iter().any(|&s| s > 0) {
            (solid.clone(), Solid::default())
        } else {
            (Solid::default(), solid.clone())
        };
    }
    // Points where edges cross the plane, shared by the edge's two triangles.
    let mut crossing: HashMap<(u32, u32), u32> = HashMap::new();
    let mut on_plane: Vec<bool> = side.iter().map(|&s| s == 0).collect();
    let mut cross_at = |a: u32, b: u32, points: &mut Vec<DVec3>, on_plane: &mut Vec<bool>| -> u32 {
        let key = (a.min(b), a.max(b));
        *crossing.entry(key).or_insert_with(|| {
            let (pa, pb) = (points[key.0 as usize], points[key.1 as usize]);
            let (da, db) = (plane.distance(pa), plane.distance(pb));
            let t = da / (da - db);
            let p = pa + (pb - pa) * t;
            points.push(p - plane.normal * plane.distance(p));
            on_plane.push(true);
            (points.len() - 1) as u32
        })
    };
    let mut pos: Vec<[u32; 3]> = Vec::new();
    let mut neg: Vec<[u32; 3]> = Vec::new();
    for t in &solid.triangles {
        let s = t.map(|i| side[i as usize]);
        if s.iter().all(|&x| x >= 0) && s.iter().any(|&x| x > 0) {
            pos.push(*t);
        } else if s.iter().all(|&x| x <= 0) && s.iter().any(|&x| x < 0) {
            neg.push(*t);
        } else if s.iter().all(|&x| x == 0) {
            // In the plane: the side behind it.
            let [a, b, c] = t.map(|i| points[i as usize]);
            if (b - a).cross(c - a).dot(plane.normal) > 0.0 {
                neg.push(*t);
            } else {
                pos.push(*t);
            }
        } else {
            // The plane crosses it: one vertex alone on its side (and maybe one on the plane).
            // Rotate so that `t[0]` is the lone one.
            let lone = (0..3)
                .find(|&k| s[k] != 0 && s[(k + 1) % 3] != s[k] && s[(k + 2) % 3] != s[k])
                .unwrap_or(0);
            let (a, b, c) = (t[lone], t[(lone + 1) % 3], t[(lone + 2) % 3]);
            let (sa, sb, sc) = (s[lone], s[(lone + 1) % 3], s[(lone + 2) % 3]);
            let (lone_side, other) = if sa > 0 {
                (&mut pos, &mut neg)
            } else {
                (&mut neg, &mut pos)
            };
            if sb == 0 {
                // b on the plane: a | c split by the crossing on a-c.
                let ac = cross_at(a, c, &mut points, &mut on_plane);
                lone_side.push([a, b, ac]);
                other.push([b, c, ac]);
            } else if sc == 0 {
                let ab = cross_at(a, b, &mut points, &mut on_plane);
                lone_side.push([a, ab, c]);
                other.push([ab, b, c]);
            } else {
                let ab = cross_at(a, b, &mut points, &mut on_plane);
                let ac = cross_at(a, c, &mut points, &mut on_plane);
                lone_side.push([a, ab, ac]);
                other.push([ab, b, c]);
                other.push([ab, c, ac]);
            }
        }
    }
    let pos_cap = cap(&pos, &on_plane, &points, plane.normal);
    let neg_cap = cap(&neg, &on_plane, &points, -plane.normal);
    pos.extend(pos_cap);
    neg.extend(neg_cap);
    (compact(&points, &pos), compact(&points, &neg))
}

/// The cap closing one side: `triangles` are that side's (its solid toward `up` from the
/// plane), `on_plane` marks the points in the plane. The side's open outline is its edges in
/// the plane without their reverse; it runs counter-clockwise seen from `up`. The cap faces
/// `-up` and runs the other way: reversed, the outline's loops are counter-clockwise in a
/// frame with u × v = -up (holes clockwise), which is how they are triangulated.
fn cap(triangles: &[[u32; 3]], on_plane: &[bool], points: &[DVec3], up: DVec3) -> Vec<[u32; 3]> {
    let mut in_plane: HashMap<(u32, u32), u32> = HashMap::new();
    for t in triangles {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            if on_plane[a as usize] && on_plane[b as usize] {
                *in_plane.entry((a, b)).or_insert(0) += 1;
            }
        }
    }
    // The reversed outline: an edge b -> a for every open a -> b.
    let mut outline: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut open: Vec<(u32, u32)> = in_plane
        .keys()
        .filter(|&&(a, b)| !in_plane.contains_key(&(b, a)))
        .copied()
        .collect();
    open.sort_unstable();
    for (a, b) in open {
        outline.entry(b).or_default().push(a);
    }
    let mut loops: Vec<Vec<u32>> = Vec::new();
    let mut starts: Vec<u32> = outline.keys().copied().collect();
    starts.sort_unstable();
    for start in starts {
        while let Some(first) = outline.get_mut(&start).and_then(Vec::pop) {
            let mut ring = vec![start];
            let mut at = first;
            let mut guard = 0;
            while at != start && guard <= points.len() {
                ring.push(at);
                let Some(next) = outline.get_mut(&at).and_then(Vec::pop) else {
                    break;
                };
                at = next;
                guard += 1;
            }
            if at == start && ring.len() >= 3 {
                loops.push(ring);
            }
        }
    }
    let u = up.any_orthonormal_vector();
    let v = -up.cross(u);
    let flat: Vec<DVec2> = points
        .iter()
        .map(|&p| DVec2::new(p.dot(u), p.dot(v)))
        .collect();
    let mut outers: Vec<Vec<u32>> = Vec::new();
    let mut holes: Vec<Vec<u32>> = Vec::new();
    for ring in loops {
        let area = signed_area(&flat, &ring);
        if area > 0.0 {
            outers.push(ring);
        } else if area < 0.0 {
            holes.push(ring);
        }
    }
    // Each hole in the smallest outer boundary around it.
    let mut by_outer: Vec<Vec<Vec<u32>>> = vec![Vec::new(); outers.len()];
    for hole in holes {
        let probe = flat[hole[0] as usize];
        let around = (0..outers.len())
            .filter(|&o| contains(&flat, &outers[o], probe))
            .min_by(|&a, &b| {
                signed_area(&flat, &outers[a]).total_cmp(&signed_area(&flat, &outers[b]))
            });
        if let Some(o) = around {
            by_outer[o].push(hole);
        }
    }
    let mut out = Vec::new();
    for (outer, holes) in outers.iter().zip(by_outer.iter_mut()) {
        triangulate(&flat, outer, holes, &mut out);
    }
    out
}

/// A solid of `triangles` with only the points they use.
fn compact(points: &[DVec3], triangles: &[[u32; 3]]) -> Solid {
    let mut index = vec![u32::MAX; points.len()];
    let mut out = Solid::default();
    for t in triangles {
        out.triangles.push(t.map(|i| {
            if index[i as usize] == u32::MAX {
                index[i as usize] = out.points.len() as u32;
                out.points.push(points[i as usize]);
            }
            index[i as usize]
        }));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A box, closed and outward-wound.
    pub(crate) fn cuboid(lo: DVec3, hi: DVec3) -> Solid {
        let points = (0..8)
            .map(|i| {
                DVec3::new(
                    if i & 1 == 0 { lo.x } else { hi.x },
                    if i & 2 == 0 { lo.y } else { hi.y },
                    if i & 4 == 0 { lo.z } else { hi.z },
                )
            })
            .collect();
        let triangles = vec![
            [0, 2, 1],
            [1, 2, 3],
            [4, 5, 6],
            [5, 7, 6],
            [0, 1, 4],
            [1, 5, 4],
            [2, 6, 3],
            [3, 6, 7],
            [0, 4, 2],
            [2, 4, 6],
            [1, 3, 5],
            [3, 7, 5],
        ];
        Solid { points, triangles }
    }

    /// Every edge once each way.
    pub(crate) fn closed(s: &Solid) -> bool {
        let mut edges = HashMap::new();
        for t in &s.triangles {
            for k in 0..3 {
                *edges.entry((t[k], t[(k + 1) % 3])).or_insert(0) += 1;
            }
        }
        edges
            .iter()
            .all(|(&(a, b), &n)| n == 1 && edges.get(&(b, a)) == Some(&1))
    }

    #[test]
    fn a_box_cut_in_two() {
        let solid = cuboid(DVec3::ZERO, DVec3::new(2.0, 1.0, 1.0));
        let plane = Plane {
            normal: DVec3::X,
            offset: 0.5,
        };
        let (pos, neg) = clip(&solid, &plane, 1e-9);
        assert!(closed(&pos) && closed(&neg));
        assert!((pos.volume() - 1.5).abs() < 1e-9, "{}", pos.volume());
        assert!((neg.volume() - 0.5).abs() < 1e-9, "{}", neg.volume());
        // A slanted cut.
        let plane = Plane {
            normal: DVec3::new(1.0, 1.0, 0.3).normalize(),
            offset: 1.0,
        };
        let (pos, neg) = clip(&solid, &plane, 1e-9);
        assert!(closed(&pos) && closed(&neg));
        assert!((pos.volume() + neg.volume() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_ring_cut_across_leaves_two_loops_and_through_leaves_a_hole() {
        // A square ring: a box with a square hole through it (16 vertices).
        let mut s = Solid::default();
        let (outer, inner) = (2.0, 1.0);
        let ring = |r: f64| {
            [
                DVec3::new(-r, -r, 0.0),
                DVec3::new(r, -r, 0.0),
                DVec3::new(r, r, 0.0),
                DVec3::new(-r, r, 0.0),
            ]
        };
        for z in [0.0, 1.0] {
            for p in ring(outer) {
                s.points.push(p + DVec3::Z * z);
            }
            for p in ring(inner) {
                s.points.push(p + DVec3::Z * z);
            }
        }
        // Indices: bottom outer 0-3, bottom inner 4-7, top outer 8-11, top inner 12-15.
        for k in 0..4u32 {
            let k1 = (k + 1) % 4;
            // Bottom (facing -z) and top (facing +z) quads between the rings.
            s.triangles.push([k, 4 + k1, k1]);
            s.triangles.push([k, 4 + k, 4 + k1]);
            s.triangles.push([8 + k, 8 + k1, 12 + k1]);
            s.triangles.push([8 + k, 12 + k1, 12 + k]);
            // Outer wall (facing out) and inner wall (facing the hole).
            s.triangles.push([k, k1, 8 + k1]);
            s.triangles.push([k, 8 + k1, 8 + k]);
            s.triangles.push([4 + k, 12 + k1, 4 + k1]);
            s.triangles.push([4 + k, 12 + k, 12 + k1]);
        }
        assert!(closed(&s));
        assert!((s.volume() - (16.0 - 4.0)).abs() < 1e-9, "{}", s.volume());
        // Across z: the cut is a square with a square hole.
        let (pos, neg) = clip(
            &s,
            &Plane {
                normal: DVec3::Z,
                offset: 0.25,
            },
            1e-9,
        );
        assert!(closed(&pos) && closed(&neg));
        assert!((pos.volume() - 9.0).abs() < 1e-9 && (neg.volume() - 3.0).abs() < 1e-9);
        // Across x through the hole: two separate rectangles in the cut.
        let (pos, neg) = clip(
            &s,
            &Plane {
                normal: DVec3::X,
                offset: 0.3,
            },
            1e-9,
        );
        assert!(closed(&pos) && closed(&neg));
        assert!((pos.volume() + neg.volume() - 12.0).abs() < 1e-9);
    }

    #[test]
    fn a_plane_through_vertices_and_faces() {
        let solid = cuboid(DVec3::ZERO, DVec3::ONE);
        // Through four vertices exactly (the cube's diagonal plane).
        let plane = Plane {
            normal: DVec3::new(1.0, -1.0, 0.0).normalize(),
            offset: 0.0,
        };
        let (pos, neg) = clip(&solid, &plane, 1e-9);
        assert!(closed(&pos) && closed(&neg));
        assert!((pos.volume() - 0.5).abs() < 1e-9 && (neg.volume() - 0.5).abs() < 1e-9);
        // In a face's plane: nothing to cut.
        let plane = Plane {
            normal: DVec3::Z,
            offset: 1.0,
        };
        let (pos, neg) = clip(&solid, &plane, 1e-9);
        assert!(pos.triangles.is_empty() && (neg.volume() - 1.0).abs() < 1e-9);
    }
}
