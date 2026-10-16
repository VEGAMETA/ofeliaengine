//! A bounding volume hierarchy over a mesh's triangles for the distance from a point to the
//! mesh's surface (the decomposition's Hausdorff term). Median splits along the longest axis
//! of the centroids' bounds, leaves of up to four triangles; queries visit the nearer child
//! first and skip boxes farther than the best so far. The closest point on a triangle is
//! found by its Voronoi regions (Ericson, "Real-Time Collision Detection", 5.1.5). Written
//! for this engine; no code was copied.

use engine_core::math::DVec3;

#[derive(Clone, Copy)]
struct Node {
    lo: DVec3,
    hi: DVec3,
    /// Leaf: first triangle (in `order`) and count; inner: first child, count 0.
    first: u32,
    count: u32,
}

/// The tree.
pub(crate) struct Bvh {
    nodes: Vec<Node>,
    /// Triangle indices in leaf order.
    order: Vec<u32>,
    triangles: Vec<[DVec3; 3]>,
}

const LEAF: usize = 4;

impl Bvh {
    /// A tree over the triangles `triangles` of `points`.
    pub(crate) fn new(points: &[DVec3], triangles: &[[u32; 3]]) -> Self {
        let tris: Vec<[DVec3; 3]> = triangles
            .iter()
            .map(|t| t.map(|i| points[i as usize]))
            .collect();
        let centroids: Vec<DVec3> = tris.iter().map(|t| (t[0] + t[1] + t[2]) / 3.0).collect();
        let mut order: Vec<u32> = (0..tris.len() as u32).collect();
        let mut nodes = Vec::with_capacity(2 * tris.len() / LEAF + 1);
        nodes.push(Node {
            lo: DVec3::ZERO,
            hi: DVec3::ZERO,
            first: 0,
            count: 0,
        });
        // (node, start, end) still to build.
        let mut stack = vec![(0usize, 0usize, order.len())];
        while let Some((node, start, end)) = stack.pop() {
            let (mut lo, mut hi) = (DVec3::splat(f64::MAX), DVec3::splat(f64::MIN));
            let (mut clo, mut chi) = (DVec3::splat(f64::MAX), DVec3::splat(f64::MIN));
            for &t in &order[start..end] {
                for p in tris[t as usize] {
                    lo = lo.min(p);
                    hi = hi.max(p);
                }
                clo = clo.min(centroids[t as usize]);
                chi = chi.max(centroids[t as usize]);
            }
            nodes[node].lo = lo;
            nodes[node].hi = hi;
            if end - start <= LEAF {
                nodes[node].first = start as u32;
                nodes[node].count = (end - start) as u32;
                continue;
            }
            let extent = chi - clo;
            let axis = if extent.x >= extent.y && extent.x >= extent.z {
                0
            } else if extent.y >= extent.z {
                1
            } else {
                2
            };
            let mid = (start + end) / 2;
            order[start..end].select_nth_unstable_by(mid - start, |&a, &b| {
                centroids[a as usize][axis].total_cmp(&centroids[b as usize][axis])
            });
            let left = nodes.len();
            nodes.push(Node {
                lo: DVec3::ZERO,
                hi: DVec3::ZERO,
                first: 0,
                count: 0,
            });
            nodes.push(Node {
                lo: DVec3::ZERO,
                hi: DVec3::ZERO,
                first: 0,
                count: 0,
            });
            nodes[node].first = left as u32;
            nodes[node].count = 0;
            stack.push((left, start, mid));
            stack.push((left + 1, mid, end));
        }
        Self {
            nodes,
            order,
            triangles: tris,
        }
    }

    /// The distance from `p` to the nearest triangle (`f64::MAX` without triangles).
    #[cfg(test)]
    pub(crate) fn distance(&self, p: DVec3) -> f64 {
        self.distance_squared(p, f64::MAX).sqrt()
    }

    /// The squared distance from `p` to the nearest triangle, or `limit` if none is nearer.
    pub(crate) fn distance_squared(&self, p: DVec3, limit: f64) -> f64 {
        if self.triangles.is_empty() {
            return limit;
        }
        let mut best = limit;
        let mut stack: [u32; 64] = [0; 64];
        let mut top = 1;
        while top > 0 {
            top -= 1;
            let node = self.nodes[stack[top] as usize];
            if box_distance_squared(node.lo, node.hi, p) >= best {
                continue;
            }
            if node.count > 0 {
                for &t in &self.order[node.first as usize..(node.first + node.count) as usize] {
                    let [a, b, c] = self.triangles[t as usize];
                    let d = closest_on_triangle(p, a, b, c).distance_squared(p);
                    if d < best {
                        best = d;
                    }
                }
                continue;
            }
            let (l, r) = (node.first, node.first + 1);
            let dl = box_distance_squared(self.nodes[l as usize].lo, self.nodes[l as usize].hi, p);
            let dr = box_distance_squared(self.nodes[r as usize].lo, self.nodes[r as usize].hi, p);
            // The nearer child is visited first: pushed last.
            let (near, far) = if dl <= dr { (l, r) } else { (r, l) };
            if top + 2 <= stack.len() {
                stack[top] = far;
                stack[top + 1] = near;
                top += 2;
            }
        }
        best
    }
}

fn box_distance_squared(lo: DVec3, hi: DVec3, p: DVec3) -> f64 {
    let d = (lo - p).max(DVec3::ZERO).max(p - hi);
    d.length_squared()
}

/// The point of triangle `a b c` nearest to `p`.
pub(crate) fn closest_on_triangle(p: DVec3, a: DVec3, b: DVec3, c: DVec3) -> DVec3 {
    let (ab, ac, ap) = (b - a, c - a, p - a);
    let (d1, d2) = (ab.dot(ap), ac.dot(ap));
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = p - b;
    let (d3, d4) = (ab.dot(bp), ac.dot(bp));
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        return a + ab * (d1 / (d1 - d3));
    }
    let cp = p - c;
    let (d5, d6) = (ab.dot(cp), ac.dot(cp));
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        return a + ac * (d2 / (d2 - d6));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        return b + (c - b) * ((d4 - d3) / ((d4 - d3) + (d5 - d6)));
    }
    let denom = 1.0 / (va + vb + vc);
    a + ab * (vb * denom) + ac * (vc * denom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_match_brute_force() {
        // A bumpy grid of triangles.
        let mut points = Vec::new();
        let n = 20;
        for i in 0..=n {
            for j in 0..=n {
                let (x, z) = (i as f64 / n as f64, j as f64 / n as f64);
                points.push(DVec3::new(x, 0.1 * (7.0 * x).sin() * (5.0 * z).cos(), z));
            }
        }
        let mut triangles = Vec::new();
        for i in 0..n {
            for j in 0..n {
                let k = (i * (n + 1) + j) as u32;
                let w = (n + 1) as u32;
                triangles.push([k, k + 1, k + w]);
                triangles.push([k + 1, k + w + 1, k + w]);
            }
        }
        let bvh = Bvh::new(&points, &triangles);
        let mut seed = 7u64;
        for _ in 0..300 {
            let mut r = || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 11) as f64 / (1u64 << 53) as f64
            };
            let p = DVec3::new(r() * 1.4 - 0.2, r() * 0.8 - 0.4, r() * 1.4 - 0.2);
            let brute = triangles
                .iter()
                .map(|t| {
                    let [a, b, c] = t.map(|i| points[i as usize]);
                    closest_on_triangle(p, a, b, c).distance(p)
                })
                .fold(f64::MAX, f64::min);
            assert!(
                (bvh.distance(p) - brute).abs() < 1e-12,
                "{} against {brute}",
                bvh.distance(p)
            );
        }
    }

    #[test]
    fn closest_points_on_a_triangle() {
        let (a, b, c) = (DVec3::ZERO, DVec3::X, DVec3::Y);
        let near = |p: DVec3, q: DVec3| (p - q).length() < 1e-12;
        assert!(near(
            closest_on_triangle(DVec3::new(0.2, 0.2, 1.0), a, b, c),
            DVec3::new(0.2, 0.2, 0.0)
        ));
        assert!(near(
            closest_on_triangle(DVec3::new(-1.0, -1.0, 0.0), a, b, c),
            a
        ));
        assert!(near(
            closest_on_triangle(DVec3::new(0.5, -2.0, 0.0), a, b, c),
            DVec3::new(0.5, 0.0, 0.0)
        ));
        let q = closest_on_triangle(DVec3::new(1.0, 1.0, 0.0), a, b, c);
        assert!((q - DVec3::new(0.5, 0.5, 0.0)).length() < 1e-12);
    }
}
