//! Convex hulls: QuickHull (Barber, Dobkin and Huhdanpaa 1996) after Dirk Gregorius's
//! "Implementing Quickhull" (GDC 2014). Written for this engine; no code was copied.
//!
//! - Arithmetic is in f64, on points moved to their bounds' centre. A point closer to a
//!   face's plane than the tolerance counts as on it. The default tolerance comes from the
//!   coordinates' magnitude, the way qhull sets its own, with f32's epsilon: the input
//!   points are f32, so their rounding decides which lie on a plane.
//! - The next point added is the farthest outside point of all faces (a heap), not just of
//!   one face. A vertex limit then stops at the hull of the points that mattered most, and
//!   [`Hull::error`] says how far the points it left out lie outside it.
//! - A point whose visible faces do not border on one simple loop (rounding) is dropped;
//!   it lies within rounding of the hull.

use core::cmp::Ordering;
use engine_core::math::{DVec3, Vec3};
use std::collections::BinaryHeap;

const NONE: u32 = u32::MAX;

/// Why a hull could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HullError {
    /// Fewer than four points.
    TooFewPoints,
    /// All points lie on one line (within the tolerance).
    Collinear,
    /// All points lie in one plane (within the tolerance).
    Coplanar,
}

impl core::fmt::Display for HullError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::TooFewPoints => "a hull needs at least four points",
            Self::Collinear => "the points lie on one line",
            Self::Coplanar => "the points lie in one plane",
        })
    }
}

impl std::error::Error for HullError {}

/// How to build a hull.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HullSettings {
    /// At most this many vertices (4 or more). The hull is then the hull of the points
    /// added first, each the farthest out at its turn. `usize::MAX`: the exact hull.
    pub max_vertices: usize,
    /// Points closer than this to a face's plane count as on it (scene units). 0: from
    /// the coordinates' magnitude (f32 rounding).
    pub tolerance: f32,
}

impl Default for HullSettings {
    fn default() -> Self {
        Self {
            max_vertices: usize::MAX,
            tolerance: 0.0,
        }
    }
}

/// A convex hull.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hull {
    /// Its vertices, a subset of the points it was built from.
    pub points: Vec<Vec3>,
    /// Triangles, counter-clockwise seen from outside, indexing [`Self::points`].
    pub triangles: Vec<[u32; 3]>,
    /// How far the farthest input point lies outside the hull: 0 unless a vertex limit
    /// stopped it.
    pub error: f32,
}

impl Hull {
    /// The exact hull of `points`.
    pub fn new(points: &[Vec3]) -> Result<Self, HullError> {
        Self::with_settings(points, &HullSettings::default())
    }

    /// The hull of `points` within `settings`.
    pub fn with_settings(points: &[Vec3], settings: &HullSettings) -> Result<Self, HullError> {
        engine_profiler::zone!("collision::hull");
        Builder::new(points, settings)?.run(settings.max_vertices.max(4))
    }

    /// Volume.
    pub fn volume(&self) -> f32 {
        let Some(&origin) = self.points.first() else {
            return 0.0;
        };
        let o = origin.as_dvec3();
        let mut six = 0.0;
        for t in &self.triangles {
            let [a, b, c] = t.map(|i| self.points[i as usize].as_dvec3() - o);
            six += a.dot(b.cross(c));
        }
        (six / 6.0) as f32
    }

    /// The planes of its faces: unit normal `n` (outward) and offset `d`, a point `p` on
    /// the plane where `n · p = d`. Faces too small to have a normal are skipped.
    pub fn planes(&self) -> impl Iterator<Item = (Vec3, f32)> + '_ {
        self.triangles.iter().filter_map(|t| {
            let [a, b, c] = t.map(|i| self.points[i as usize].as_dvec3());
            let n = (b - a).cross(c - a);
            let length = n.length();
            (length > 0.0).then(|| {
                let n = n / length;
                (n.as_vec3(), n.dot(a) as f32)
            })
        })
    }

    /// How far `point` lies outside the hull: the largest of its distances above the
    /// faces' planes (negative inside, a lower bound of the true distance outside).
    pub fn distance_outside(&self, point: Vec3) -> f32 {
        self.planes()
            .map(|(n, d)| n.dot(point) - d)
            .fold(f32::MIN, f32::max)
    }
}

/// A face of the hull being built.
#[derive(Clone)]
struct Face {
    /// Point indices, counter-clockwise from outside.
    v: [u32; 3],
    /// The neighbour across edge `i` (from `v[i]` to `v[(i + 1) % 3]`).
    n: [u32; 3],
    normal: DVec3,
    offset: f64,
    /// First point of its outside set (linked through `Builder::next`).
    outside: u32,
    /// The farthest of them.
    far: u32,
    far_distance: f64,
    alive: bool,
    /// Bumped when the face is reused or its outside set changes: stale heap entries are
    /// skipped.
    generation: u32,
    /// The visit of the current point, and whether it sees the face.
    stamp: u32,
    visible: bool,
}

/// A face with outside points, by its farthest one.
struct Candidate {
    distance: f64,
    face: u32,
    generation: u32,
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
        // Ties by face: the same order on every run.
        self.distance
            .total_cmp(&other.distance)
            .then(other.face.cmp(&self.face))
    }
}

struct Builder<'a> {
    input: &'a [Vec3],
    p: Vec<DVec3>,
    next: Vec<u32>,
    faces: Vec<Face>,
    free: Vec<u32>,
    heap: BinaryHeap<Candidate>,
    eps: f64,
    stamp: u32,
    /// Per point: the visit that last used it as a horizon edge's start.
    mark: Vec<u32>,
    vertices: usize,
    visible: Vec<u32>,
    horizon: Vec<(u32, u32, u32)>,
    stack: Vec<u32>,
    orphans: Vec<u32>,
    new_faces: Vec<u32>,
}

impl<'a> Builder<'a> {
    fn new(input: &'a [Vec3], settings: &HullSettings) -> Result<Self, HullError> {
        if input.len() < 4 {
            return Err(HullError::TooFewPoints);
        }
        let (lo, hi) = input.iter().fold(
            (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
            |(lo, hi), &q| (lo.min(q), hi.max(q)),
        );
        let centre = ((lo + hi) * 0.5).as_dvec3();
        let magnitude = lo.abs().max(hi.abs()).as_dvec3();
        let eps = if settings.tolerance > 0.0 {
            f64::from(settings.tolerance)
        } else {
            3.0 * (magnitude.x + magnitude.y + magnitude.z) * f64::from(f32::EPSILON)
        };
        Ok(Self {
            input,
            p: input.iter().map(|q| q.as_dvec3() - centre).collect(),
            next: vec![NONE; input.len()],
            faces: Vec::new(),
            free: Vec::new(),
            heap: BinaryHeap::new(),
            eps: eps.max(f64::MIN_POSITIVE),
            stamp: 0,
            mark: vec![0; input.len()],
            vertices: 0,
            visible: Vec::new(),
            horizon: Vec::new(),
            stack: Vec::new(),
            orphans: Vec::new(),
            new_faces: Vec::new(),
        })
    }

    fn distance(&self, face: u32, point: u32) -> f64 {
        let f = &self.faces[face as usize];
        f.normal.dot(self.p[point as usize]) - f.offset
    }

    fn alloc_face(&mut self, v: [u32; 3]) -> u32 {
        let [a, b, c] = v.map(|i| self.p[i as usize]);
        let normal = (b - a).cross(c - a);
        let length = normal.length();
        // A sliver too thin for a normal sees no points; its neighbours carry on.
        let normal = if length > 0.0 {
            normal / length
        } else {
            DVec3::ZERO
        };
        let face = Face {
            v,
            n: [NONE; 3],
            normal,
            offset: normal.dot(a),
            outside: NONE,
            far: NONE,
            far_distance: 0.0,
            alive: true,
            generation: 0,
            stamp: 0,
            visible: false,
        };
        if let Some(index) = self.free.pop() {
            let generation = self.faces[index as usize].generation.wrapping_add(1);
            self.faces[index as usize] = Face { generation, ..face };
            index
        } else {
            self.faces.push(face);
            (self.faces.len() - 1) as u32
        }
    }

    fn add_outside(&mut self, face: u32, point: u32, distance: f64) {
        let f = &mut self.faces[face as usize];
        self.next[point as usize] = f.outside;
        f.outside = point;
        if f.far == NONE || distance > f.far_distance {
            f.far = point;
            f.far_distance = distance;
        }
    }

    fn push_candidate(&mut self, face: u32) {
        let f = &self.faces[face as usize];
        if f.outside != NONE {
            self.heap.push(Candidate {
                distance: f.far_distance,
                face,
                generation: f.generation,
            });
        }
    }

    /// The first tetrahedron: from the extreme points, then the farthest from their line
    /// and from their plane.
    fn simplex(&mut self) -> Result<[u32; 4], HullError> {
        let n = self.p.len() as u32;
        let mut extremes = [0u32; 6];
        for i in 0..n {
            let q = self.p[i as usize];
            for axis in 0..3 {
                if q[axis] < self.p[extremes[2 * axis] as usize][axis] {
                    extremes[2 * axis] = i;
                }
                if q[axis] > self.p[extremes[2 * axis + 1] as usize][axis] {
                    extremes[2 * axis + 1] = i;
                }
            }
        }
        let (mut i0, mut i1, mut best) = (0, 0, -1.0);
        for a in 0..6 {
            for b in a + 1..6 {
                let d = self.p[extremes[a] as usize].distance_squared(self.p[extremes[b] as usize]);
                if d > best {
                    (i0, i1, best) = (extremes[a], extremes[b], d);
                }
            }
        }
        if best.sqrt() <= self.eps {
            return Err(HullError::Collinear);
        }
        let (a, b) = (self.p[i0 as usize], self.p[i1 as usize]);
        let axis = (b - a).normalize();
        let (mut i2, mut best) = (NONE, self.eps);
        for i in 0..n {
            let r = self.p[i as usize] - a;
            let d = (r - axis * r.dot(axis)).length();
            if d > best {
                (i2, best) = (i, d);
            }
        }
        if i2 == NONE {
            return Err(HullError::Collinear);
        }
        let c = self.p[i2 as usize];
        let normal = (b - a).cross(c - a).normalize();
        let (mut i3, mut best) = (NONE, self.eps);
        for i in 0..n {
            let d = (self.p[i as usize] - a).dot(normal).abs();
            if d > best {
                (i3, best) = (i, d);
            }
        }
        if i3 == NONE {
            return Err(HullError::Coplanar);
        }
        Ok([i0, i1, i2, i3])
    }

    fn run(mut self, max_vertices: usize) -> Result<Hull, HullError> {
        let simplex = self.simplex()?;
        // The four faces, each turned away from the fourth point.
        for skip in 0..4 {
            let mut v = [0u32; 3];
            let mut k = 0;
            for (i, &s) in simplex.iter().enumerate() {
                if i != skip {
                    v[k] = s;
                    k += 1;
                }
            }
            let face = self.alloc_face(v);
            if self.distance(face, simplex[skip]) > 0.0 {
                let f = &mut self.faces[face as usize];
                f.v.swap(1, 2);
                f.normal = -f.normal;
                f.offset = -f.offset;
            }
        }
        for f in 0..4u32 {
            for i in 0..3 {
                let (a, b) = (
                    self.faces[f as usize].v[i],
                    self.faces[f as usize].v[(i + 1) % 3],
                );
                for g in 0..4u32 {
                    let w = self.faces[g as usize].v;
                    if g != f && (0..3).any(|j| w[j] == b && w[(j + 1) % 3] == a) {
                        self.faces[f as usize].n[i] = g;
                    }
                }
            }
        }
        self.vertices = 4;
        for i in 0..self.p.len() as u32 {
            if simplex.contains(&i) {
                continue;
            }
            let (mut best, mut face) = (self.eps, NONE);
            for f in 0..4u32 {
                let d = self.distance(f, i);
                if d > best {
                    (best, face) = (d, f);
                }
            }
            if face != NONE {
                self.add_outside(face, i, best);
            }
        }
        for f in 0..4 {
            self.push_candidate(f);
        }

        let mut stopped = false;
        while let Some(c) = self.heap.pop() {
            let f = &self.faces[c.face as usize];
            if !f.alive || f.generation != c.generation || f.outside == NONE {
                continue;
            }
            if self.vertices >= max_vertices {
                stopped = true;
                break;
            }
            let eye = f.far;
            if self.add_point(c.face, eye) {
                self.vertices += 1;
            } else {
                self.drop_outside(c.face, eye);
            }
        }
        Ok(self.finish(stopped))
    }

    /// Adds `eye`, the farthest point outside `face`: removes the faces it sees and joins
    /// it to their border. False (nothing changed) if that border is not one simple loop.
    fn add_point(&mut self, face: u32, eye: u32) -> bool {
        self.stamp = self.stamp.wrapping_add(1);
        let stamp = self.stamp;
        self.visible.clear();
        self.horizon.clear();
        self.stack.clear();
        {
            let f = &mut self.faces[face as usize];
            f.stamp = stamp;
            f.visible = true;
        }
        self.visible.push(face);
        self.stack.push(face);
        while let Some(g) = self.stack.pop() {
            for i in 0..3 {
                let h = self.faces[g as usize].n[i];
                if self.faces[h as usize].stamp != stamp {
                    let visible = self.distance(h, eye) > self.eps;
                    let f = &mut self.faces[h as usize];
                    f.stamp = stamp;
                    f.visible = visible;
                    if visible {
                        self.visible.push(h);
                        self.stack.push(h);
                    }
                }
                if !self.faces[h as usize].visible {
                    let v = self.faces[g as usize].v;
                    self.horizon.push((v[i], v[(i + 1) % 3], h));
                }
            }
        }
        // Order the border into a loop; each vertex starts one edge only.
        let count = self.horizon.len();
        for k in 0..count {
            let start = self.horizon[k].0;
            if self.mark[start as usize] == stamp {
                return false;
            }
            self.mark[start as usize] = stamp;
            if k + 1 == count {
                break;
            }
            let end = self.horizon[k].1;
            let Some(m) = (k + 1..count).find(|&m| self.horizon[m].0 == end) else {
                return false;
            };
            self.horizon.swap(k + 1, m);
        }
        if self.horizon[count - 1].1 != self.horizon[0].0 {
            return false;
        }

        // Points over the faces going away look for new faces.
        self.orphans.clear();
        for k in 0..self.visible.len() {
            let g = self.visible[k];
            let mut q = self.faces[g as usize].outside;
            while q != NONE {
                if q != eye {
                    self.orphans.push(q);
                }
                q = self.next[q as usize];
            }
            let f = &mut self.faces[g as usize];
            f.alive = false;
            f.outside = NONE;
            self.free.push(g);
        }
        // The cone from the eye over the border.
        self.new_faces.clear();
        for k in 0..count {
            let (a, b, twin) = self.horizon[k];
            let nf = self.alloc_face([a, b, eye]);
            let t = &mut self.faces[twin as usize];
            let Some(j) = (0..3).find(|&j| t.v[j] == b && t.v[(j + 1) % 3] == a) else {
                // Not reachable: the border's faces share these edges.
                return false;
            };
            t.n[j] = nf;
            self.faces[nf as usize].n[0] = twin;
            self.new_faces.push(nf);
        }
        for k in 0..count {
            let nf = self.new_faces[k];
            self.faces[nf as usize].n[1] = self.new_faces[(k + 1) % count];
            self.faces[nf as usize].n[2] = self.new_faces[(k + count - 1) % count];
        }
        for k in 0..self.orphans.len() {
            let q = self.orphans[k];
            let (mut best, mut face) = (self.eps, NONE);
            for &nf in &self.new_faces {
                let d = self.distance(nf, q);
                if d > best {
                    (best, face) = (d, nf);
                }
            }
            if face != NONE {
                self.add_outside(face, q, best);
            }
        }
        for k in 0..self.new_faces.len() {
            self.push_candidate(self.new_faces[k]);
        }
        true
    }

    /// Takes `point` out of `face`'s outside set (it could not be added).
    fn drop_outside(&mut self, face: u32, point: u32) {
        let mut rest = Vec::new();
        let mut q = self.faces[face as usize].outside;
        while q != NONE {
            if q != point {
                rest.push(q);
            }
            q = self.next[q as usize];
        }
        {
            let f = &mut self.faces[face as usize];
            f.outside = NONE;
            f.far = NONE;
            f.far_distance = 0.0;
            f.generation = f.generation.wrapping_add(1);
        }
        for q in rest {
            let d = self.distance(face, q);
            self.add_outside(face, q, d);
        }
        self.push_candidate(face);
    }

    /// How far the points left outside lie outside the hull: over every face's plane, not
    /// only their own face's.
    fn error(&self) -> f64 {
        let mut error = 0.0f64;
        for f in self.faces.iter().filter(|f| f.alive) {
            let mut q = f.outside;
            while q != NONE {
                let p = self.p[q as usize];
                let d = self
                    .faces
                    .iter()
                    .filter(|g| g.alive)
                    .map(|g| g.normal.dot(p) - g.offset)
                    .fold(f64::MIN, f64::max);
                error = error.max(d);
                q = self.next[q as usize];
            }
        }
        error
    }

    fn finish(self, stopped: bool) -> Hull {
        let error = if stopped { self.error() } else { 0.0 };
        let mut index = vec![NONE; self.input.len()];
        let mut points = Vec::with_capacity(self.vertices);
        let mut triangles = Vec::new();
        for f in self.faces.iter().filter(|f| f.alive) {
            triangles.push(f.v.map(|v| {
                if index[v as usize] == NONE {
                    index[v as usize] = points.len() as u32;
                    points.push(self.input[v as usize]);
                }
                index[v as usize]
            }));
        }
        Hull {
            points,
            triangles,
            error: error as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic generator (xorshift) for test points.
    pub(crate) struct Random(u64);

    impl Random {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }

        pub(crate) fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }

        pub(crate) fn in_ball(&mut self) -> Vec3 {
            loop {
                let p = Vec3::new(self.next(), self.next(), self.next()) * 2.0 - 1.0;
                if p.length_squared() <= 1.0 {
                    return p;
                }
            }
        }
    }

    /// Every input point inside, every vertex an input point, closed and convex.
    fn check(hull: &Hull, input: &[Vec3], tolerance: f32) {
        // Closed: every edge has its reverse.
        let mut edges = std::collections::HashMap::new();
        for t in &hull.triangles {
            for i in 0..3 {
                *edges.entry((t[i], t[(i + 1) % 3])).or_insert(0) += 1;
            }
        }
        for (&(a, b), &count) in &edges {
            assert_eq!(count, 1, "edge {a}-{b} used {count} times");
            assert_eq!(edges.get(&(b, a)), Some(&1), "edge {a}-{b} has no reverse");
        }
        // Euler: V - E + F = 2.
        let (v, e, f) = (hull.points.len(), edges.len() / 2, hull.triangles.len());
        assert_eq!(v as i64 - e as i64 + f as i64, 2);
        for p in input {
            assert!(
                hull.distance_outside(*p) <= tolerance,
                "{p} is {} outside",
                hull.distance_outside(*p)
            );
        }
        for p in &hull.points {
            assert!(input.contains(p));
        }
        assert!(hull.volume() > 0.0);
    }

    fn cube() -> Vec<Vec3> {
        let mut points = Vec::new();
        for i in 0..8 {
            points.push(Vec3::new(
                (i & 1) as f32,
                ((i >> 1) & 1) as f32,
                ((i >> 2) & 1) as f32,
            ));
        }
        points
    }

    #[test]
    fn cube_with_points_inside_and_on_faces() {
        let mut points = cube();
        let mut random = Random::new(7);
        for _ in 0..200 {
            points.push(Vec3::new(random.next(), random.next(), random.next()));
        }
        // On faces and edges: coplanar with the hull's faces.
        for k in 0..50 {
            let t = k as f32 / 50.0;
            points.push(Vec3::new(t, 0.0, 0.5));
            points.push(Vec3::new(1.0, t, t));
        }
        let hull = Hull::new(&points).unwrap();
        check(&hull, &points, 1e-6);
        assert_eq!(hull.points.len(), 8);
        assert!((hull.volume() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn random_ball_and_sphere() {
        let mut random = Random::new(3);
        let ball: Vec<Vec3> = (0..5000).map(|_| random.in_ball()).collect();
        let hull = Hull::new(&ball).unwrap();
        check(&hull, &ball, 1e-6);
        // Points on a sphere: every one is a vertex.
        let sphere: Vec<Vec3> = (0..2000).map(|_| random.in_ball().normalize()).collect();
        let hull = Hull::new(&sphere).unwrap();
        check(&hull, &sphere, 1e-6);
        assert!(hull.points.len() > 1990, "{}", hull.points.len());
        let v = hull.volume();
        assert!(v > 4.1 && v < 4.19, "{v}");
    }

    #[test]
    fn grid_far_from_the_origin() {
        // A grid: many coplanar points, rounding at 10 km.
        let mut points = Vec::new();
        for x in 0..10 {
            for y in 0..10 {
                for z in 0..10 {
                    points.push(
                        Vec3::new(10_000.0, -5_000.0, 2_000.0)
                            + Vec3::new(x as f32, y as f32, z as f32) * 0.1,
                    );
                }
            }
        }
        let hull = Hull::new(&points).unwrap();
        check(&hull, &points, 1e-2);
        assert_eq!(hull.points.len(), 8);
        assert!((hull.volume() - 0.729).abs() < 1e-2, "{}", hull.volume());
    }

    #[test]
    fn degenerate_inputs() {
        assert_eq!(
            Hull::new(&cube()[..3]).unwrap_err(),
            HullError::TooFewPoints
        );
        let line: Vec<Vec3> = (0..10).map(|i| Vec3::splat(i as f32)).collect();
        assert_eq!(Hull::new(&line).unwrap_err(), HullError::Collinear);
        let plane: Vec<Vec3> = (0..20)
            .map(|i| Vec3::new(i as f32, (i * i % 7) as f32, 0.0))
            .collect();
        assert_eq!(Hull::new(&plane).unwrap_err(), HullError::Coplanar);
        let same = vec![Vec3::ONE; 10];
        assert_eq!(Hull::new(&same).unwrap_err(), HullError::Collinear);
    }

    #[test]
    fn duplicates_change_nothing() {
        let mut points = cube();
        points.extend(cube());
        points.extend(cube());
        let hull = Hull::new(&points).unwrap();
        check(&hull, &points, 1e-6);
        assert_eq!(hull.points.len(), 8);
    }

    #[test]
    fn a_vertex_limit_keeps_the_farthest_points() {
        let mut random = Random::new(11);
        let sphere: Vec<Vec3> = (0..3000).map(|_| random.in_ball().normalize()).collect();
        let mut last = f32::MAX;
        for limit in [8, 16, 32, 64, 128] {
            let hull = Hull::with_settings(
                &sphere,
                &HullSettings {
                    max_vertices: limit,
                    ..HullSettings::default()
                },
            )
            .unwrap();
            assert_eq!(hull.points.len(), limit);
            let worst = sphere
                .iter()
                .map(|p| hull.distance_outside(*p))
                .fold(0.0, f32::max);
            // The error reported is the farthest point left out.
            assert!(
                (worst - hull.error).abs() < 1e-4,
                "{worst} against {}",
                hull.error
            );
            assert!(hull.error < last);
            last = hull.error;
        }
        // 64 vertices of a unit sphere: within about 5 % of its radius.
        assert!(last < 0.05, "{last}");
    }

    #[test]
    fn deterministic() {
        let mut random = Random::new(5);
        let points: Vec<Vec3> = (0..1000).map(|_| random.in_ball()).collect();
        assert_eq!(Hull::new(&points).unwrap(), Hull::new(&points).unwrap());
    }
}
