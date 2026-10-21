//! The shapes brushes start as (ADR 0110), in a brush's own grid: its base's middle at the
//! origin, its height along +y, its size `[x, y, z]` in grid units.

use crate::exact::Plane;
use crate::polygon::{PlaneId, Planes};
use core::f64::consts::{PI, TAU};
use engine_core::math::DVec3;

/// `parts` with every plane moved `by` grid units out along its normal: each convex part
/// grown by `by` on every side, as a room's walls stand around its inside (ADR 0110).
/// `None` when a plane leaves the grid's bounds.
pub fn grown(parts: &[Vec<Plane>], by: i64) -> Option<Vec<Vec<Plane>>> {
    parts
        .iter()
        .map(|part| part.iter().map(|p| p.offset(by as f64)).collect())
        .collect()
}

/// The planes of the convex hull of grid points `points`, each face's once, facing out
/// (ADR 0110: a brush's part made again from its corners when a corner or an edge of it
/// moves). Points that end up inside go, and corners that meet merge: a box's edge moved
/// onto another makes a wedge.
///
/// Exact when a face's normal, its common factors gone, is within the planes' bounds (a
/// brush's own faces, faces through grid points less than a few metres apart); a larger one
/// is rounded, the plane through the face's first corner. `None` when the points span no
/// volume. Brute force over triples of points, for the few corners a part has.
pub fn hull(points: &[[i64; 3]]) -> Option<Vec<Plane>> {
    let mut points = points.to_vec();
    points.sort_unstable();
    points.dedup();
    let n = points.len();
    let sub = |a: [i64; 3], b: [i64; 3]| [0, 1, 2].map(|i| i128::from(a[i] - b[i]));
    let dot = |n: [i128; 3], p: [i64; 3]| (0..3).map(|i| n[i] * i128::from(p[i])).sum::<i128>();
    let mut out: Vec<Plane> = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            for k in j + 1..n {
                let (a, b, c) = (points[i], points[j], points[k]);
                let (u, v) = (sub(b, a), sub(c, a));
                let normal = [
                    u[1] * v[2] - u[2] * v[1],
                    u[2] * v[0] - u[0] * v[2],
                    u[0] * v[1] - u[1] * v[0],
                ];
                if normal == [0; 3] {
                    continue;
                }
                let d = dot(normal, a);
                let (mut front, mut back) = (false, false);
                for &p in &points {
                    match dot(normal, p).cmp(&d) {
                        core::cmp::Ordering::Greater => front = true,
                        core::cmp::Ordering::Less => back = true,
                        core::cmp::Ordering::Equal => {}
                    }
                    if front && back {
                        break;
                    }
                }
                if front == back {
                    // Points on both sides: not a face. On neither: all in one plane.
                    continue;
                }
                // Facing out: the other points behind it.
                let (normal, d) = if front {
                    (normal.map(|c| -c), -d)
                } else {
                    (normal, d)
                };
                let plane = hull_plane(normal, d, a)?;
                if !out.contains(&plane) {
                    out.push(plane);
                }
            }
        }
    }
    (out.len() >= 4).then_some(out)
}

/// The plane `normal · x = d` through grid point `a`, reduced, or rounded when it is beyond
/// the planes' bounds.
fn hull_plane(normal: [i128; 3], d: i128, a: [i64; 3]) -> Option<Plane> {
    let gcd = |mut x: u128, mut y: u128| {
        while y != 0 {
            (x, y) = (y, x % y);
        }
        x
    };
    let g = normal
        .iter()
        .chain(core::iter::once(&d))
        .fold(0u128, |g, &c| gcd(g, c.unsigned_abs()))
        .max(1) as i128;
    let (n, d) = (normal.map(|c| c / g), d / g);
    let fits = n
        .iter()
        .all(|c| c.abs() <= i128::from(crate::exact::MAX_NORMAL))
        && d.abs() <= i128::from(crate::exact::MAX_OFFSET);
    if fits {
        Plane::new(n.map(|c| c as i64), d as i64)
    } else {
        let a = DVec3::new(a[0] as f64, a[1] as f64, a[2] as f64);
        Plane::from_f64(DVec3::new(n[0] as f64, n[1] as f64, n[2] as f64), a)
    }
}

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
    /// A cone of this many sides around y, its top this many thousandths of its base (0:
    /// pointed; between: truncated).
    Cone(u32, u32),
    /// A ring around y of this many convex parts, its tube of this many sides, lying on its
    /// base: the size's x and z its outer diameters, its y the tube's.
    Torus(u32, u32),
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
        Shape::Cone(sides, top) => {
            let n = sides.max(3);
            let t = f64::from(top.min(1000)) / 1000.0;
            let (a, b) = (sx as f64 / 2.0, sz as f64 / 2.0);
            let corner = |k: u32, scale: f64, y: f64| {
                let u = TAU * f64::from(k) / f64::from(n) + PI / f64::from(n);
                DVec3::new(a * scale * u.cos(), y, b * scale * u.sin())
            };
            let h = sy as f64;
            let inside = DVec3::new(0.0, h / 4.0, 0.0);
            let mut planes = vec![Plane::new([0, -1, 0], 0)?];
            if t > 0.0 {
                planes.push(Plane::new([0, 1, 0], sy)?);
            }
            for k in 0..n {
                let (p, q) = (corner(k, 1.0, 0.0), corner(k + 1, 1.0, 0.0));
                // The side through the base's edge and the top's (or the apex).
                let up = corner(k, t, h);
                let mut normal = (q - p).cross(up - p);
                if normal.dot(p - inside) < 0.0 {
                    normal = -normal;
                }
                planes.push(Plane::from_f64(normal, p)?);
            }
            Some(vec![planes])
        }
        Shape::Torus(around, tube) => {
            let (n, m) = (around.max(3), tube.max(3));
            let r = sy as f64 / 2.0;
            let (a, b) = (sx as f64 / 2.0 - r, sz as f64 / 2.0 - r);
            if a <= 0.0 || b <= 0.0 {
                return None;
            }
            // The tube's centre line: a polygon on the ellipse, at its middle height.
            let point = |k: u32| {
                let u = TAU * f64::from(k % n) / f64::from(n);
                DVec3::new(a * u.cos(), r, b * u.sin())
            };
            let along = |k: u32| (point(k + 1) - point(k)).normalize();
            let mut parts = Vec::with_capacity(n as usize);
            for k in 0..n {
                let (p0, p1) = (point(k), point(k + 1));
                let d = along(k);
                // Across the segment, outwards (away from the ring's axis).
                let mid = (p0 + p1) / 2.0;
                let mut out = DVec3::new(d.z, 0.0, -d.x);
                if out.dot(DVec3::new(mid.x, 0.0, mid.z)) < 0.0 {
                    out = -out;
                }
                let mut planes = Vec::with_capacity(m as usize + 2);
                for j in 0..m {
                    // The tube's sides around the segment, the first flat on the ground.
                    let beta = TAU * f64::from(j) / f64::from(m) - PI / 2.0;
                    let normal = out * beta.cos() + DVec3::Y * beta.sin();
                    planes.push(Plane::from_f64(normal, mid + normal * r)?);
                }
                // The ends: the planes halfway between this segment and its neighbours',
                // the same plane both share (turned around), so their faces there go.
                let before = along(k + n - 1) + d;
                let after = d + along(k + 1);
                planes.push(Plane::from_f64(-before, p0)?);
                planes.push(Plane::from_f64(after, p1)?);
                parts.push(planes);
            }
            Some(parts)
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
            inside: Vec::new(),
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
    fn a_hull_of_a_boxs_corners_is_the_box_and_an_edge_moved_onto_another_makes_a_wedge() {
        let corner = |i: usize, top_back: Option<[i64; 3]>| {
            let p = [
                if i & 1 != 0 { 2000 } else { 0 },
                if i & 2 != 0 { 1000 } else { 0 },
                if i & 4 != 0 { 3000 } else { 0 },
            ];
            // The top's back edge (y = 1000, z = 3000) moved.
            match top_back {
                Some(to) if i & 6 == 6 => [p[0] + to[0], p[1] + to[1], p[2] + to[2]],
                _ => p,
            }
        };
        let id = (DMat3::IDENTITY, DVec3::ZERO);
        let mut points: Vec<[i64; 3]> = (0..8).map(|i| corner(i, None)).collect();
        // A point inside and a corner twice change nothing.
        points.push([1000, 500, 1500]);
        points.push(points[3]);
        let planes = hull(&points).unwrap_or_else(|| panic!());
        let mut want = cuboid([0, 0, 0], [2000, 1000, 3000]).unwrap_or_else(|| panic!());
        let mut got = planes.clone();
        let key = |p: &Plane| (p.n, p.d);
        want.sort_by_key(key);
        got.sort_by_key(key);
        assert_eq!(got, want);
        // The top's back edge down onto the bottom's: a wedge, five faces, half the volume.
        let points: Vec<[i64; 3]> = (0..8).map(|i| corner(i, Some([0, -1000, 0]))).collect();
        let wedge = hull(&points).unwrap_or_else(|| panic!());
        assert_eq!(wedge.len(), 5);
        let v = volume(&[wedge], id.0, id.1);
        assert_eq!(v, 2000.0 * 1000.0 * 3000.0 / 2.0);
        // Moved out and up a little: a sloped back, still closed, the volume between.
        let points: Vec<[i64; 3]> = (0..8).map(|i| corner(i, Some([0, 250, 700]))).collect();
        let sloped = hull(&points).unwrap_or_else(|| panic!());
        let v = volume(&[sloped], id.0, id.1);
        assert!(v > 2000.0 * 1000.0 * 3000.0, "{v}");
        // All in one plane: no hull.
        assert!(hull(&[[0, 0, 0], [1000, 0, 0], [0, 0, 1000], [1000, 0, 1000]]).is_none());
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
    fn cones_pointed_and_truncated_have_their_volumes() {
        let id = (DMat3::IDENTITY, DVec3::ZERO);
        let size = [2000, 1500, 2000];
        // A 16-gon inscribed in a circle of radius 1000.
        let n = 16.0;
        let base = n / 2.0 * 1000.0 * 1000.0 * (TAU / n).sin();
        let pointed = volume(
            &parts(Shape::Cone(16, 0), size).unwrap_or_else(|| panic!()),
            id.0,
            id.1,
        );
        let want = base * 1500.0 / 3.0;
        assert!(
            (pointed / want - 1.0).abs() < 1e-5,
            "{pointed} against {want}"
        );
        // Half the base's size on top: a frustum.
        let half = volume(
            &parts(Shape::Cone(16, 500), size).unwrap_or_else(|| panic!()),
            id.0,
            id.1,
        );
        let top = base / 4.0;
        let want = 1500.0 / 3.0 * (base + top + (base * top).sqrt());
        assert!((half / want - 1.0).abs() < 1e-5, "{half} against {want}");
        // The whole base on top: the cylinder.
        let full = volume(
            &parts(Shape::Cone(16, 1000), size).unwrap_or_else(|| panic!()),
            id.0,
            id.1,
        );
        let cylinder = volume(
            &parts(Shape::Cylinder(16), size).unwrap_or_else(|| panic!()),
            id.0,
            id.1,
        );
        assert!(
            (full / cylinder - 1.0).abs() < 1e-5,
            "{full} against {cylinder}"
        );
    }

    #[test]
    fn a_torus_is_its_convex_segments_closed_into_one_ring() {
        let id = (DMat3::IDENTITY, DVec3::ZERO);
        // Outer diameter 4 m, tube 1 m: the ring's radius 1.5 m.
        let ring = parts(Shape::Torus(24, 12), [4000, 1000, 4000]).unwrap_or_else(|| panic!());
        assert_eq!(ring.len(), 24);
        let v = volume(&ring, id.0, id.1);
        let want = 2.0 * PI * PI * 1500.0 * 500.0 * 500.0;
        assert!((v / want - 1.0).abs() < 0.03, "{v} against {want}");
        // Only the tube's sides show: the segments' ends, shared, both go (all but strips
        // nanometres wide where the rounded sides meet them, which make no triangles).
        let mut table = Planes::new();
        let brush = Brush {
            op: Op::Add,
            parts: ring
                .iter()
                .map(|p| p.iter().map(|q| table.add(*q)).collect())
                .collect(),
            inside: Vec::new(),
        };
        let e = evaluate(&[brush], &table);
        let shown = e
            .surface
            .iter()
            .filter(|(_, p)| {
                let c: Vec<DVec3> = p.corners.iter().map(|c| c.to_f64()).collect();
                !crate::mesh::is_sliver(&c)
            })
            .count();
        assert_eq!(shown, 24 * 12);
        // No hole left: refused.
        assert!(parts(Shape::Torus(24, 12), [1000, 1000, 1000]).is_none());
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
