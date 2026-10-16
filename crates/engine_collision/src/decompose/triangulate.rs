//! Polygons with holes triangulated by ear clipping (after David Eberly, "Triangulation by
//! Ear Clipping", Geometric Tools, 2002-2015): holes are joined to their outer boundary by
//! bridges, then ears are cut off one by one. Written for this engine; no code was copied.
//!
//! The polygons are a closed mesh's cross-sections by a plane (`super::clip`): a few dozen
//! to a few hundred vertices, often nearly collinear runs. Every step keeps going when
//! rounding leaves no clean ear: it cuts the least bad one, so a polygon always becomes
//! `n - 2` triangles (plus two per hole).

use engine_core::math::DVec2;

/// Twice the signed area of a polygon (positive counter-clockwise).
pub(crate) fn signed_area(points: &[DVec2], polygon: &[u32]) -> f64 {
    let mut twice = 0.0;
    for i in 0..polygon.len() {
        let a = points[polygon[i] as usize];
        let b = points[polygon[(i + 1) % polygon.len()] as usize];
        twice += a.perp_dot(b);
    }
    twice
}

/// Whether `p` is inside the polygon (crossing number; points on the border either way).
pub(crate) fn contains(points: &[DVec2], polygon: &[u32], p: DVec2) -> bool {
    let mut inside = false;
    let n = polygon.len();
    for i in 0..n {
        let a = points[polygon[i] as usize];
        let b = points[polygon[(i + 1) % n] as usize];
        if (a.y > p.y) != (b.y > p.y) {
            let x = a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x);
            if x > p.x {
                inside = !inside;
            }
        }
    }
    inside
}

fn cross(o: DVec2, a: DVec2, b: DVec2) -> f64 {
    (a - o).perp_dot(b - o)
}

/// Whether `p` is inside or on the triangle `a b c` (counter-clockwise), within `eps`.
fn in_triangle(a: DVec2, b: DVec2, c: DVec2, p: DVec2, eps: f64) -> bool {
    cross(a, b, p) >= -eps && cross(b, c, p) >= -eps && cross(c, a, p) >= -eps
}

/// One polygon made of the outer boundary and its holes joined by bridges.
fn bridge_holes(points: &[DVec2], outer: &[u32], holes: &mut [Vec<u32>]) -> Vec<u32> {
    let mut polygon = outer.to_vec();
    // Holes from the rightmost: each bridge then sees only holes already joined.
    holes.sort_by(|a, b| {
        let right = |h: &Vec<u32>| {
            h.iter()
                .map(|&i| points[i as usize].x)
                .fold(f64::MIN, f64::max)
        };
        right(b).total_cmp(&right(a))
    });
    for hole in holes.iter() {
        if hole.len() < 3 {
            continue;
        }
        // The hole's rightmost vertex.
        let (m_at, m) = hole
            .iter()
            .enumerate()
            .map(|(k, &i)| (k, points[i as usize]))
            .fold((0, DVec2::splat(f64::MIN)), |best, (k, p)| {
                if p.x > best.1.x || (p.x == best.1.x && p.y < best.1.y) {
                    (k, p)
                } else {
                    best
                }
            });
        // The nearest edge of the polygon a ray to the right from it meets.
        let n = polygon.len();
        let mut best: Option<(f64, usize)> = None;
        for k in 0..n {
            let a = points[polygon[k] as usize];
            let b = points[polygon[(k + 1) % n] as usize];
            if (a.y > m.y) == (b.y > m.y) && a.y != m.y && b.y != m.y {
                continue;
            }
            if a.y == b.y {
                continue;
            }
            let t = (m.y - a.y) / (b.y - a.y);
            if !(0.0..=1.0).contains(&t) {
                continue;
            }
            let x = a.x + t * (b.x - a.x);
            if x >= m.x && best.is_none_or(|(bx, _)| x < bx) {
                best = Some((x, k));
            }
        }
        let Some((x, edge)) = best else {
            // No edge to the right (rounding): bridge to the nearest vertex instead.
            let nearest = (0..n)
                .min_by(|&a, &b| {
                    points[polygon[a] as usize]
                        .distance_squared(m)
                        .total_cmp(&points[polygon[b] as usize].distance_squared(m))
                })
                .unwrap_or(0);
            splice(&mut polygon, nearest, hole, m_at);
            continue;
        };
        let hit = DVec2::new(x, m.y);
        // The edge's end with the larger x is visible unless a reflex vertex lies in the
        // triangle (m, hit, end); then the one of those making the smallest angle with the
        // ray.
        let (a, b) = (edge, (edge + 1) % n);
        let mut candidate = if points[polygon[a] as usize].x > points[polygon[b] as usize].x {
            a
        } else {
            b
        };
        let end = points[polygon[candidate] as usize];
        let (t0, t1, t2) = if cross(m, hit, end) >= 0.0 {
            (m, hit, end)
        } else {
            (m, end, hit)
        };
        let mut best_angle = f64::MAX;
        for k in 0..n {
            let p = points[polygon[k] as usize];
            let prev = points[polygon[(k + n - 1) % n] as usize];
            let next = points[polygon[(k + 1) % n] as usize];
            let reflex = cross(prev, p, next) <= 0.0;
            if k == candidate || !reflex || !in_triangle(t0, t1, t2, p, 0.0) {
                continue;
            }
            let d = p - m;
            let angle = d.y.abs().atan2(d.x);
            if angle < best_angle
                || (angle == best_angle
                    && d.length_squared()
                        < (points[polygon[candidate] as usize] - m).length_squared())
            {
                best_angle = angle;
                candidate = k;
            }
        }
        splice(&mut polygon, candidate, hole, m_at);
    }
    polygon
}

/// Inserts `hole` (from its vertex `at`) into `polygon` after vertex `k`, through a bridge
/// that goes there and back.
fn splice(polygon: &mut Vec<u32>, k: usize, hole: &[u32], at: usize) {
    let mut insert = Vec::with_capacity(hole.len() + 2);
    for j in 0..=hole.len() {
        insert.push(hole[(at + j) % hole.len()]);
    }
    insert.push(polygon[k]);
    let tail = polygon.split_off(k + 1);
    polygon.extend(insert);
    polygon.extend(tail);
}

/// Triangles of a polygon (counter-clockwise; holes clockwise, inside it), as indices of
/// `points`, counter-clockwise. Appends to `out`.
pub(crate) fn triangulate(
    points: &[DVec2],
    outer: &[u32],
    holes: &mut [Vec<u32>],
    out: &mut Vec<[u32; 3]>,
) {
    let polygon = bridge_holes(points, outer, holes);
    let n = polygon.len();
    if n < 3 {
        return;
    }
    // Scale for the tolerances.
    let (lo, hi) = polygon.iter().fold(
        (DVec2::splat(f64::MAX), DVec2::splat(f64::MIN)),
        |(lo, hi), &i| (lo.min(points[i as usize]), hi.max(points[i as usize])),
    );
    let eps = (hi - lo).length_squared() * 1e-12;
    let mut next: Vec<usize> = (0..n).map(|k| (k + 1) % n).collect();
    let mut prev: Vec<usize> = (0..n).map(|k| (k + n - 1) % n).collect();
    let p = |k: usize| points[polygon[k] as usize];
    let mut remaining = n;
    let mut k = 0;
    let mut stalled = 0;
    while remaining > 3 {
        let (a, b, c) = (prev[k], k, next[k]);
        let convex = cross(p(a), p(b), p(c)) > eps;
        let mut ear = convex;
        if ear {
            // No other vertex inside (the bridge's duplicates count only when elsewhere).
            let mut j = next[c];
            while j != a {
                let q = p(j);
                if q != p(a) && q != p(b) && q != p(c) && in_triangle(p(a), p(b), p(c), q, -eps) {
                    ear = false;
                    break;
                }
                j = next[j];
            }
        }
        if ear || stalled > remaining {
            // A full turn without an ear (rounding): cut the flattest corner.
            let (a, b, c) = if ear {
                (a, b, c)
            } else {
                let mut best = (f64::MAX, k);
                let mut j = k;
                for _ in 0..remaining {
                    let turn = cross(p(prev[j]), p(j), p(next[j]));
                    let score = if turn > 0.0 { -turn } else { turn.abs() * 0.5 };
                    if turn >= 0.0 && score < best.0 {
                        best = (score, j);
                    }
                    j = next[j];
                }
                let j = best.1;
                (prev[j], j, next[j])
            };
            out.push([polygon[a], polygon[b], polygon[c]]);
            next[a] = c;
            prev[c] = a;
            remaining -= 1;
            stalled = 0;
            k = c;
        } else {
            k = next[k];
            stalled += 1;
        }
    }
    let (a, b, c) = (prev[k], k, next[k]);
    out.push([polygon[a], polygon[b], polygon[c]]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(points: &[DVec2], triangles: &[[u32; 3]]) -> f64 {
        triangles
            .iter()
            .map(|t| {
                0.5 * cross(
                    points[t[0] as usize],
                    points[t[1] as usize],
                    points[t[2] as usize],
                )
            })
            .sum()
    }

    #[test]
    fn a_square_with_a_square_hole() {
        let points = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(4.0, 0.0),
            DVec2::new(4.0, 4.0),
            DVec2::new(0.0, 4.0),
            DVec2::new(1.0, 1.0),
            DVec2::new(1.0, 3.0),
            DVec2::new(3.0, 3.0),
            DVec2::new(3.0, 1.0),
        ];
        let mut out = Vec::new();
        triangulate(&points, &[0, 1, 2, 3], &mut [vec![4, 5, 6, 7]], &mut out);
        assert_eq!(out.len(), 8);
        assert!((area(&points, &out) - 12.0).abs() < 1e-9);
        assert!(out.iter().all(|t| cross(
            points[t[0] as usize],
            points[t[1] as usize],
            points[t[2] as usize]
        ) > 0.0));
    }

    #[test]
    fn a_comb_and_collinear_runs() {
        // A comb: deep notches, and many collinear points along its back.
        let mut points = Vec::new();
        for k in 0..=20 {
            points.push(DVec2::new(k as f64, 0.0));
        }
        for k in (0..10).rev() {
            let x = 2.0 * k as f64;
            points.push(DVec2::new(x + 2.0, 5.0));
            points.push(DVec2::new(x + 1.0, 1.0));
        }
        points.push(DVec2::new(0.0, 5.0));
        let outer: Vec<u32> = (0..points.len() as u32).collect();
        assert!(signed_area(&points, &outer) > 0.0);
        let mut out = Vec::new();
        triangulate(&points, &outer, &mut [], &mut out);
        assert_eq!(out.len(), points.len() - 2);
        assert!((area(&points, &out) - 0.5 * signed_area(&points, &outer)).abs() < 1e-9);
    }

    #[test]
    fn two_holes() {
        let points = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(10.0, 0.0),
            DVec2::new(10.0, 4.0),
            DVec2::new(0.0, 4.0),
            // Holes, clockwise.
            DVec2::new(1.0, 1.0),
            DVec2::new(1.0, 3.0),
            DVec2::new(3.0, 3.0),
            DVec2::new(3.0, 1.0),
            DVec2::new(6.0, 1.0),
            DVec2::new(6.0, 3.0),
            DVec2::new(8.0, 3.0),
            DVec2::new(8.0, 1.0),
        ];
        let mut out = Vec::new();
        triangulate(
            &points,
            &[0, 1, 2, 3],
            &mut [vec![4, 5, 6, 7], vec![8, 9, 10, 11]],
            &mut out,
        );
        assert!((area(&points, &out) - 32.0).abs() < 1e-9);
        assert_eq!(out.len(), 4 - 2 + 2 * (4 + 2));
    }
}
