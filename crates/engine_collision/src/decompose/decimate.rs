//! A closed mesh simplified by edge collapses (Garland and Heckbert, "Surface Simplification
//! Using Quadric Error Metrics", 1997) that keep it a closed manifold: an edge collapses only
//! if the two vertices' neighbourhoods meet in just the edge's two opposite vertices (the link
//! condition), and no triangle around it flips or folds.
//!
//! A collapse costs the mean squared distance of the new vertex to the planes around it (the
//! quadric's error over the area it gathers), plus a little for the edge's length: where the
//! errors tie, as everywhere on a flat face, shorter edges go first, so a flat face coarsens
//! evenly instead of into a fan around one vertex (whose many neighbours would make every
//! later collapse there slow). Written for this engine; no code was copied.

use super::clip::Solid;
use core::cmp::Ordering;
use engine_core::math::{DMat3, DVec3};
use std::collections::BinaryHeap;

/// The edge length's share of a collapse's cost (per squared length, against squared
/// distances): small enough not to reorder collapses that move the surface.
const LENGTH_WEIGHT: f64 = 1e-4;

/// No collapse leaves a vertex with more neighbours than this.
const MAX_VALENCE: usize = 32;

/// A symmetric 4 × 4 quadric `[a², ab, ac, ad, b², bc, bd, c², cd, d²]` and the area its
/// planes were weighted by.
#[derive(Clone, Copy, Default)]
struct Quadric {
    q: [f64; 10],
    area: f64,
}

impl Quadric {
    fn plane(n: DVec3, d: f64, area: f64) -> Self {
        let (a, b, c) = (n.x, n.y, n.z);
        Self {
            q: [
                a * a,
                a * b,
                a * c,
                a * d,
                b * b,
                b * c,
                b * d,
                c * c,
                c * d,
                d * d,
            ]
            .map(|x| x * area),
            area,
        }
    }

    fn add(self, o: Self) -> Self {
        let mut q = self.q;
        for (x, y) in q.iter_mut().zip(o.q) {
            *x += y;
        }
        Self {
            q,
            area: self.area + o.area,
        }
    }

    /// The area-weighted sum of squared distances of `p` to the planes.
    fn error(&self, p: DVec3) -> f64 {
        let q = &self.q;
        let (x, y, z) = (p.x, p.y, p.z);
        q[0] * x * x
            + 2.0 * q[1] * x * y
            + 2.0 * q[2] * x * z
            + 2.0 * q[3] * x
            + q[4] * y * y
            + 2.0 * q[5] * y * z
            + 2.0 * q[6] * y
            + q[7] * z * z
            + 2.0 * q[8] * z
            + q[9]
    }

    /// The mean squared distance of `p` to the planes, by area.
    fn mean(&self, p: DVec3) -> f64 {
        (self.error(p) / self.area.max(1e-300)).max(0.0)
    }

    /// The point of least error, if the quadric's 3 × 3 part is well conditioned.
    fn optimum(&self) -> Option<DVec3> {
        let q = &self.q;
        let m = DMat3::from_cols(
            DVec3::new(q[0], q[1], q[2]),
            DVec3::new(q[1], q[4], q[5]),
            DVec3::new(q[2], q[5], q[7]),
        );
        let det = m.determinant();
        let scale = (q[0] + q[4] + q[7]).max(1e-300);
        (det.abs() > 1e-9 * scale * scale * scale)
            .then(|| m.inverse() * -DVec3::new(q[3], q[6], q[8]))
    }

    /// Where the collapse of an edge from `pa` to `pb` puts its vertex, and the mean squared
    /// distance there.
    fn place(&self, pa: DVec3, pb: DVec3) -> (DVec3, f64) {
        let mut best = self.optimum().map(|p| (p, self.mean(p)));
        for p in [pa, pb, (pa + pb) * 0.5] {
            let e = self.mean(p);
            if best.is_none_or(|(_, b)| e < b) {
                best = Some((p, e));
            }
        }
        best.unwrap_or(((pa + pb) * 0.5, 0.0))
    }
}

struct Candidate {
    cost: f64,
    error: f64,
    a: u32,
    b: u32,
    stamp: (u32, u32),
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // The heap is a max-heap: the cheapest first.
        other
            .cost
            .total_cmp(&self.cost)
            .then(other.a.cmp(&self.a))
            .then(other.b.cmp(&self.b))
    }
}

/// The mesh being simplified.
struct Mesh {
    points: Vec<DVec3>,
    faces: Vec<[u32; 3]>,
    alive: Vec<bool>,
    /// The faces around each vertex (dead ones dropped when the vertex is next visited).
    around: Vec<Vec<u32>>,
    quadrics: Vec<Quadric>,
    version: Vec<u32>,
    removed: Vec<bool>,
}

impl Mesh {
    /// `v`'s neighbours into `ring`, each once: `mark[w] == stamp` for each of them after.
    fn ring(&mut self, v: u32, ring: &mut Vec<u32>, mark: &mut [u32], stamp: u32) {
        ring.clear();
        let alive = &self.alive;
        let around = &mut self.around[v as usize];
        around.retain(|&f| alive[f as usize]);
        for &f in around.iter() {
            for &w in &self.faces[f as usize] {
                if w != v && mark[w as usize] != stamp {
                    mark[w as usize] = stamp;
                    ring.push(w);
                }
            }
        }
    }

    fn push(&self, a: u32, b: u32, heap: &mut BinaryHeap<Candidate>) {
        let (pa, pb) = (self.points[a as usize], self.points[b as usize]);
        let q = self.quadrics[a as usize].add(self.quadrics[b as usize]);
        let (_, error) = q.place(pa, pb);
        heap.push(Candidate {
            cost: error + LENGTH_WEIGHT * pa.distance_squared(pb),
            error,
            a,
            b,
            stamp: (self.version[a as usize], self.version[b as usize]),
        });
    }

    /// Whether moving `a` and `b` to `p` keeps every triangle around them (but `shared`, the
    /// two the collapse removes) from flipping or folding (its normal turned more than ~78°).
    fn keeps_orientation(&self, a: u32, b: u32, p: DVec3, shared: [u32; 2]) -> bool {
        for v in [a, b] {
            for &f in &self.around[v as usize] {
                if !self.alive[f as usize] || shared.contains(&f) {
                    continue;
                }
                let t = self.faces[f as usize];
                let old = t.map(|i| self.points[i as usize]);
                let new = t.map(|i| {
                    if i == a || i == b {
                        p
                    } else {
                        self.points[i as usize]
                    }
                });
                let n0 = (old[1] - old[0]).cross(old[2] - old[0]);
                let n1 = (new[1] - new[0]).cross(new[2] - new[0]);
                let (l0, l1) = (n0.length(), n1.length());
                if l1 <= 1e-12 * l0.max(1e-300) || n0.dot(n1) <= 0.2 * l0 * l1 {
                    return false;
                }
            }
        }
        true
    }
}

/// `solid` (closed, outward-wound) simplified to about `target` triangles, no collapse putting
/// its vertex farther than `max_error` (scene units; the root mean square by area) from the
/// planes of the triangles it gathers. Stays closed and manifold.
pub(crate) fn decimate(solid: &Solid, target: usize, max_error: f64) -> Solid {
    engine_profiler::zone!("collision::decompose::decimate");
    let n = solid.points.len();
    let mut mesh = Mesh {
        points: solid.points.clone(),
        faces: solid.triangles.clone(),
        alive: vec![true; solid.triangles.len()],
        around: vec![Vec::new(); n],
        quadrics: vec![Quadric::default(); n],
        version: vec![0; n],
        removed: vec![false; n],
    };
    for (f, t) in mesh.faces.iter().enumerate() {
        for &v in t {
            mesh.around[v as usize].push(f as u32);
        }
        let [a, b, c] = t.map(|i| mesh.points[i as usize]);
        let normal = (b - a).cross(c - a);
        let area = normal.length() * 0.5;
        if area > 0.0 {
            let unit = normal.normalize();
            let q = Quadric::plane(unit, -unit.dot(a), area);
            for &v in t {
                mesh.quadrics[v as usize] = mesh.quadrics[v as usize].add(q);
            }
        }
    }
    let mut heap = BinaryHeap::with_capacity(mesh.faces.len() * 3 / 2);
    for t in &mesh.faces {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            if a < b {
                mesh.push(a, b, &mut heap);
            }
        }
    }
    let mut count = mesh.faces.len();
    let limit = max_error * max_error;
    let (mut ring_a, mut ring_b) = (Vec::new(), Vec::new());
    let (mut mark_a, mut mark_b) = (vec![0u32; n], vec![0u32; n]);
    let mut stamp = 0u32;
    let mut moved = Vec::new();
    while count > target {
        let Some(c) = heap.pop() else {
            break;
        };
        let (a, b) = (c.a, c.b);
        if mesh.removed[a as usize]
            || mesh.removed[b as usize]
            || c.stamp != (mesh.version[a as usize], mesh.version[b as usize])
            || c.error > limit
        {
            continue;
        }
        // A tetrahedron's last collapse would leave a flat solid.
        if count <= 4 {
            break;
        }
        stamp += 1;
        mesh.ring(a, &mut ring_a, &mut mark_a, stamp);
        mesh.ring(b, &mut ring_b, &mut mark_b, stamp);
        // Still an edge, between exactly two triangles?
        let mut shared = [u32::MAX; 2];
        let mut found = 0;
        for &f in &mesh.around[a as usize] {
            if mesh.faces[f as usize].contains(&b) {
                if found < 2 {
                    shared[found] = f;
                }
                found += 1;
            }
        }
        if found != 2 {
            continue;
        }
        // The link condition: the neighbours of a and b meet only at the two opposite vertices.
        let common = ring_b
            .iter()
            .filter(|&&w| mark_a[w as usize] == stamp)
            .count();
        if common != 2 {
            continue;
        }
        // a and b are in each other's rings; the new vertex has the rest of both.
        if ring_a.len() + ring_b.len() - common - 2 > MAX_VALENCE {
            continue;
        }
        let q = mesh.quadrics[a as usize].add(mesh.quadrics[b as usize]);
        let (p, _) = q.place(mesh.points[a as usize], mesh.points[b as usize]);
        if !mesh.keeps_orientation(a, b, p, shared) {
            continue;
        }
        // Collapse b into a.
        for f in shared {
            mesh.alive[f as usize] = false;
            count -= 1;
        }
        moved.clear();
        moved.extend(
            mesh.around[b as usize]
                .iter()
                .copied()
                .filter(|&f| mesh.alive[f as usize]),
        );
        for &f in &moved {
            for v in mesh.faces[f as usize].iter_mut() {
                if *v == b {
                    *v = a;
                }
            }
        }
        mesh.around[a as usize].extend_from_slice(&moved);
        mesh.around[b as usize].clear();
        mesh.removed[b as usize] = true;
        mesh.points[a as usize] = p;
        mesh.quadrics[a as usize] = q;
        mesh.version[a as usize] += 1;
        stamp += 1;
        mesh.ring(a, &mut ring_a, &mut mark_a, stamp);
        for &w in &ring_a {
            mesh.push(a.min(w), a.max(w), &mut heap);
        }
    }
    // Compact.
    let mut out = Solid::default();
    let mut index = vec![u32::MAX; n];
    for (f, t) in mesh.faces.iter().enumerate() {
        if !mesh.alive[f] {
            continue;
        }
        out.triangles.push(t.map(|i| {
            if index[i as usize] == u32::MAX {
                index[i as usize] = out.points.len() as u32;
                out.points.push(mesh.points[i as usize]);
            }
            index[i as usize]
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::clip::tests::cuboid;
    use crate::decompose::remesh::{closed_manifold, remesh};

    #[test]
    fn a_remeshed_box_simplifies_closed_and_keeps_its_volume() {
        let solid = cuboid(DVec3::ZERO, DVec3::new(1.0, 0.6, 2.0));
        let dense = remesh(&solid.points, &solid.triangles, 40, 0.55, None);
        assert!(closed_manifold(&dense));
        let lean = decimate(&dense, 300, 0.01);
        assert!(closed_manifold(&lean));
        assert!(lean.triangles.len() <= 302, "{}", lean.triangles.len());
        assert!(
            (lean.volume() - dense.volume()).abs() < 0.02 * dense.volume(),
            "{} against {}",
            lean.volume(),
            dense.volume()
        );
    }

    #[test]
    fn a_thin_shell_stays_closed() {
        // An open box: remeshed into a thin closed shell, then simplified hard.
        let solid = cuboid(DVec3::ZERO, DVec3::ONE);
        let open = Solid {
            points: solid.points.clone(),
            triangles: solid.triangles[..10].to_vec(),
        };
        let shell = remesh(&open.points, &open.triangles, 40, 0.55, None);
        let lean = decimate(&shell, 500, 0.02);
        assert!(closed_manifold(&lean), "{} triangles", lean.triangles.len());
    }

    #[test]
    fn flat_faces_coarsen_evenly() {
        // A box remeshed finely: its flat faces must not collapse into fans around single
        // vertices (no vertex keeps more than MAX_VALENCE neighbours).
        let solid = cuboid(DVec3::ZERO, DVec3::new(2.0, 1.0, 1.5));
        let dense = remesh(&solid.points, &solid.triangles, 80, 0.55, None);
        let lean = decimate(&dense, 400, 0.01);
        assert!(closed_manifold(&lean));
        let mut valence = vec![0usize; lean.points.len()];
        for t in &lean.triangles {
            for &v in t {
                valence[v as usize] += 1;
            }
        }
        // A closed manifold vertex has as many neighbours as triangles around it.
        assert!(
            valence.iter().all(|&v| v <= MAX_VALENCE),
            "{:?}",
            valence.iter().max()
        );
    }

    #[test]
    fn the_error_bound_holds() {
        // A sphere-like remesh simplified with a tight bound keeps more triangles than one
        // with a loose bound, and every point stays near the original surface.
        let solid = cuboid(DVec3::ZERO, DVec3::ONE);
        let dense = remesh(&solid.points, &solid.triangles, 40, 0.55, None);
        let tight = decimate(&dense, 0, 0.001);
        let loose = decimate(&dense, 0, 0.05);
        assert!(
            tight.triangles.len() > loose.triangles.len(),
            "{} against {}",
            tight.triangles.len(),
            loose.triangles.len()
        );
        let bvh = crate::decompose::bvh::Bvh::new(&dense.points, &dense.triangles);
        for p in &tight.points {
            assert!(bvh.distance_squared(*p, f64::MAX).sqrt() < 0.01, "{p}");
        }
    }
}
