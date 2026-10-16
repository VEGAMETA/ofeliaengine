//! Primitives fitted around a hull (ADR 0101). Each one contains every point of the hull,
//! so it never cuts into the mesh, and the smallest is chosen:
//!
//! - **box:** the smallest of the boxes along the world axes, along the principal axes of
//!   the hull's surface, and along each face's normal turned to the smallest rectangle
//!   around the hull in that face's plane (close to the smallest box; O'Rourke's exact
//!   search is O(n³));
//! - **sphere:** the smallest enclosing sphere (Welzl 1991);
//! - **capsule** and **cylinder:** along each of the box's axes, around the smallest circle
//!   enclosing the points seen down the axis; the capsule's radius is searched for the
//!   smallest volume.
//!
//! Written for this engine; no code was copied.

use crate::hull::{Hull, HullSettings};
use core::f64::consts::PI;
use engine_core::math::{DMat3, DQuat, DVec2, DVec3, Quat, Vec3};

/// An oriented box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrientedBox {
    /// Centre.
    pub center: Vec3,
    /// From the box's axes to the frame of its points.
    pub rotation: Quat,
    /// Half the size along each of its axes.
    pub half_extents: Vec3,
}

/// A sphere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sphere {
    /// Centre.
    pub center: Vec3,
    /// Radius.
    pub radius: f32,
}

/// A capsule: a segment along its local Y, swept by a sphere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Capsule {
    /// Centre.
    pub center: Vec3,
    /// From its local frame (axis along Y) to the frame of its points.
    pub rotation: Quat,
    /// Half the segment's length.
    pub half_height: f32,
    /// Radius.
    pub radius: f32,
}

/// A cylinder along its local Y.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cylinder {
    /// Centre.
    pub center: Vec3,
    /// From its local frame (axis along Y) to the frame of its points.
    pub rotation: Quat,
    /// Half its height.
    pub half_height: f32,
    /// Radius.
    pub radius: f32,
}

/// A fitted primitive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Primitive {
    /// An oriented box.
    Box(OrientedBox),
    /// A sphere.
    Sphere(Sphere),
    /// A capsule.
    Capsule(Capsule),
    /// A cylinder.
    Cylinder(Cylinder),
}

impl OrientedBox {
    /// Volume.
    pub fn volume(&self) -> f32 {
        8.0 * self.half_extents.x * self.half_extents.y * self.half_extents.z
    }
}

impl Sphere {
    /// Volume.
    pub fn volume(&self) -> f32 {
        (4.0 / 3.0 * PI) as f32 * self.radius.powi(3)
    }
}

impl Capsule {
    /// Volume.
    pub fn volume(&self) -> f32 {
        capsule_volume(f64::from(self.radius), f64::from(2.0 * self.half_height)) as f32
    }
}

impl Cylinder {
    /// Volume.
    pub fn volume(&self) -> f32 {
        PI as f32 * self.radius * self.radius * 2.0 * self.half_height
    }
}

impl Primitive {
    /// Volume.
    pub fn volume(&self) -> f32 {
        match self {
            Self::Box(b) => b.volume(),
            Self::Sphere(s) => s.volume(),
            Self::Capsule(c) => c.volume(),
            Self::Cylinder(c) => c.volume(),
        }
    }
}

fn capsule_volume(radius: f64, length: f64) -> f64 {
    PI * radius * radius * length + 4.0 / 3.0 * PI * radius.powi(3)
}

/// The rotation taking local Y onto `axis` (unit).
fn y_to(axis: DVec3) -> Quat {
    DQuat::from_rotation_arc(DVec3::Y, axis)
        .as_quat()
        .normalize()
}

/// Unit vectors `u`, `v` with `u`, `v`, `n` right-handed and orthonormal.
fn basis(n: DVec3) -> (DVec3, DVec3) {
    let u = n.any_orthonormal_vector();
    (u, n.cross(u))
}

// ---------------------------------------------------------------------------------------
// Boxes

/// Eigenvectors of a symmetric matrix (cyclic Jacobi rotations), as the columns of a
/// rotation.
fn eigenvectors(m: DMat3) -> DMat3 {
    // a[i][j]: row i, column j.
    let mut a = [[0.0f64; 3]; 3];
    for (i, row) in a.iter_mut().enumerate() {
        for (j, x) in row.iter_mut().enumerate() {
            *x = m.col(j)[i];
        }
    }
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let scale = (0..3)
        .map(|i| a[i][i].abs())
        .fold(0.0, f64::max)
        .max(1e-300);
    for _ in 0..50 {
        let off = a[0][1].abs() + a[0][2].abs() + a[1][2].abs();
        if off <= 1e-15 * scale {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            let apq = a[p][q];
            if apq.abs() <= 1e-300 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * apq);
            let t = if theta == 0.0 {
                1.0
            } else {
                theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt())
            };
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            a[p][p] -= t * apq;
            a[q][q] += t * apq;
            a[p][q] = 0.0;
            a[q][p] = 0.0;
            // The third row and column (3 × 3: one other index).
            let r = 3 - p - q;
            let (arp, arq) = (a[r][p], a[r][q]);
            a[r][p] = c * arp - s * arq;
            a[p][r] = a[r][p];
            a[r][q] = s * arp + c * arq;
            a[q][r] = a[r][q];
            for row in &mut v {
                let (vp, vq) = (row[p], row[q]);
                row[p] = c * vp - s * vq;
                row[q] = s * vp + c * vq;
            }
        }
    }
    let col = |k: usize| DVec3::new(v[0][k], v[1][k], v[2][k]).normalize();
    let (x, y) = (col(0), col(1));
    // Right-handed and orthonormal again after rounding.
    let z = x.cross(y).normalize();
    DMat3::from_cols(x, z.cross(x), z)
}

/// The covariance of the hull's surface (area-weighted, so the vertices' spacing does not
/// bias it).
fn surface_covariance(hull: &Hull) -> DMat3 {
    let (mut area, mut mean, mut second) = (0.0, DVec3::ZERO, DMat3::ZERO);
    for t in &hull.triangles {
        let [a, b, c] = t.map(|i| hull.points[i as usize].as_dvec3());
        let ta = 0.5 * (b - a).cross(c - a).length();
        if ta <= 0.0 {
            continue;
        }
        let m = (a + b + c) / 3.0;
        area += ta;
        mean += m * ta;
        // ∫ x xᵀ over the triangle = A/12 (9 m mᵀ + a aᵀ + b bᵀ + c cᵀ).
        let outer = |p: DVec3| DMat3::from_cols(p * p.x, p * p.y, p * p.z);
        second += (outer(m) * 9.0 + outer(a) + outer(b) + outer(c)) * (ta / 12.0);
    }
    if area <= 0.0 {
        return DMat3::IDENTITY;
    }
    let mean = mean / area;
    second * (1.0 / area) - DMat3::from_cols(mean * mean.x, mean * mean.y, mean * mean.z)
}

/// The box along the columns of `axes` around `points`.
fn box_along(axes: DMat3, points: &[DVec3]) -> (f64, DVec3, DVec3) {
    let (mut lo, mut hi) = (DVec3::splat(f64::MAX), DVec3::splat(f64::MIN));
    let t = axes.transpose();
    for &p in points {
        let q = t * p;
        lo = lo.min(q);
        hi = hi.max(q);
    }
    let size = hi - lo;
    (
        size.x * size.y * size.z,
        axes * ((lo + hi) * 0.5),
        size * 0.5,
    )
}

/// The convex hull of 2D points (Andrew's monotone chain), counter-clockwise.
fn hull_2d(points: &mut Vec<DVec2>) -> Vec<DVec2> {
    points.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    points.dedup();
    if points.len() < 3 {
        return points.clone();
    }
    let cross = |o: DVec2, a: DVec2, b: DVec2| (a - o).perp_dot(b - o);
    let mut out: Vec<DVec2> = Vec::with_capacity(points.len() * 2);
    let chain = |out: &mut Vec<DVec2>, p: DVec2, start: usize| {
        while out.len() >= start + 2 && cross(out[out.len() - 2], out[out.len() - 1], p) <= 0.0 {
            out.pop();
        }
        out.push(p);
    };
    for &p in points.iter() {
        chain(&mut out, p, 0);
    }
    out.pop();
    let start = out.len();
    for &p in points.iter().rev() {
        chain(&mut out, p, start);
    }
    out.pop();
    out
}

/// The direction of the smallest rectangle around a convex polygon: one of its edges.
fn smallest_rectangle(polygon: &[DVec2]) -> DVec2 {
    let mut best = (f64::MAX, DVec2::X);
    for i in 0..polygon.len() {
        let e = polygon[(i + 1) % polygon.len()] - polygon[i];
        let length = e.length();
        if length <= 0.0 {
            continue;
        }
        let e = e / length;
        let (mut lo, mut hi) = (DVec2::splat(f64::MAX), DVec2::splat(f64::MIN));
        for &p in polygon {
            let q = DVec2::new(p.dot(e), p.perp_dot(e));
            lo = lo.min(q);
            hi = hi.max(q);
        }
        let area = (hi.x - lo.x) * (hi.y - lo.y);
        if area < best.0 {
            best = (area, e);
        }
    }
    best.1
}

/// The smallest box found around `hull` (see the module's documentation).
pub fn oriented_box(hull: &Hull) -> OrientedBox {
    engine_profiler::zone!("collision::fit_box");
    let points: Vec<DVec3> = hull.points.iter().map(|p| p.as_dvec3()).collect();
    let mut best = box_along(DMat3::IDENTITY, &points);
    let mut best_axes = DMat3::IDENTITY;
    let consider = |axes: DMat3, best: &mut (f64, DVec3, DVec3), best_axes: &mut DMat3| {
        let candidate = box_along(axes, &points);
        if candidate.0 < best.0 {
            *best = candidate;
            *best_axes = axes;
        }
    };
    consider(
        eigenvectors(surface_covariance(hull)),
        &mut best,
        &mut best_axes,
    );
    // Directions from a hull of at most 64 vertices (its faces' normals); extents from
    // every point.
    let coarse;
    let directions = if hull.points.len() > 64 {
        coarse = Hull::with_settings(
            &hull.points,
            &HullSettings {
                max_vertices: 64,
                ..HullSettings::default()
            },
        )
        .unwrap_or_else(|_| hull.clone());
        &coarse
    } else {
        hull
    };
    let coarse_points: Vec<DVec3> = directions.points.iter().map(|p| p.as_dvec3()).collect();
    let mut tried: Vec<DVec3> = Vec::new();
    let mut flat = Vec::with_capacity(coarse_points.len());
    for (n, _) in directions.planes() {
        let n = n.as_dvec3().normalize();
        if tried.iter().any(|t| t.dot(n).abs() > 1.0 - 1e-9) {
            continue;
        }
        tried.push(n);
        let (u, v) = basis(n);
        flat.clear();
        flat.extend(coarse_points.iter().map(|p| DVec2::new(p.dot(u), p.dot(v))));
        let e = smallest_rectangle(&hull_2d(&mut flat));
        let a = (u * e.x + v * e.y).normalize();
        consider(
            DMat3::from_cols(a, n.cross(a), n),
            &mut best,
            &mut best_axes,
        );
    }
    let (_, center, half) = best;
    OrientedBox {
        center: center.as_vec3(),
        rotation: DQuat::from_mat3(&best_axes).as_quat().normalize(),
        half_extents: half.as_vec3(),
    }
}

/// A box around `points` along their principal axes, at least `min_thickness` thick: for
/// flat or degenerate sets, where no hull exists.
pub fn flat_box(points: &[Vec3], min_thickness: f32) -> OrientedBox {
    let p: Vec<DVec3> = points.iter().map(|q| q.as_dvec3()).collect();
    let n = p.len().max(1) as f64;
    let mean = p.iter().copied().sum::<DVec3>() / n;
    let mut covariance = DMat3::ZERO;
    for &q in &p {
        let d = q - mean;
        covariance += DMat3::from_cols(d * d.x, d * d.y, d * d.z);
    }
    let axes = eigenvectors(covariance * (1.0 / n));
    let (_, center, half) = box_along(axes, &p);
    OrientedBox {
        center: center.as_vec3(),
        rotation: DQuat::from_mat3(&axes).as_quat().normalize(),
        half_extents: half.as_vec3().max(Vec3::splat(0.5 * min_thickness)),
    }
}

// ---------------------------------------------------------------------------------------
// Spheres and circles (Welzl)

/// A deterministic shuffle (xorshift seeded by the length).
fn shuffled<T: Copy>(points: &[T]) -> Vec<T> {
    let mut out = points.to_vec();
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ out.len() as u64;
    for i in (1..out.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.swap(i, (state % (i as u64 + 1)) as usize);
    }
    out
}

const SLACK: f64 = 1e-12;

fn inside_sphere(c: DVec3, r2: f64, p: DVec3) -> bool {
    c.distance_squared(p) <= r2 * (1.0 + 1e-10) + SLACK
}

/// The circle through three points, as a sphere (its centre in their plane), or none if
/// they are collinear.
fn sphere3(a: DVec3, b: DVec3, c: DVec3) -> Option<(DVec3, f64)> {
    let (u, v) = (a - c, b - c);
    let w = u.cross(v);
    let w2 = w.length_squared();
    if w2 <= 1e-30 * (u.length_squared() * v.length_squared()).max(1e-300) {
        return None;
    }
    let center = c + (v * u.length_squared() - u * v.length_squared()).cross(w) / (2.0 * w2);
    Some((center, center.distance_squared(a)))
}

/// The sphere through four points, or none if they are coplanar.
fn sphere4(a: DVec3, b: DVec3, c: DVec3, d: DVec3) -> Option<(DVec3, f64)> {
    let (u, v, w) = (b - a, c - a, d - a);
    let m = DMat3::from_cols(u, v, w).transpose();
    let det = m.determinant();
    if det.abs() <= 1e-10 * (u.length() * v.length() * w.length()).max(1e-300) {
        return None;
    }
    let rhs = DVec3::new(u.length_squared(), v.length_squared(), w.length_squared()) * 0.5;
    let x = m.inverse() * rhs;
    Some((a + x, x.length_squared()))
}

/// The largest of the three points' pairwise diametral spheres: their sphere when they
/// are collinear.
fn widest_pair(a: DVec3, b: DVec3, c: DVec3) -> (DVec3, f64) {
    [diametral(a, b), diametral(a, c), diametral(b, c)]
        .into_iter()
        .fold((DVec3::ZERO, -1.0), |x, y| if y.1 > x.1 { y } else { x })
}

/// The smallest sphere with `a`, `b`, `c` on its surface that contains `d` too, when the
/// four are coplanar.
fn smallest_of_three(a: DVec3, b: DVec3, c: DVec3, d: DVec3) -> (DVec3, f64) {
    let mut best = (DVec3::ZERO, f64::MAX);
    for (p, q, r) in [(a, b, d), (a, c, d), (b, c, d), (a, b, c)] {
        let s = sphere3(p, q, r).unwrap_or_else(|| widest_pair(p, q, r));
        if s.1 < best.1 && [a, b, c, d].iter().all(|&x| inside_sphere(s.0, s.1, x)) {
            best = s;
        }
    }
    if best.1 == f64::MAX {
        // Rounding: the four points' bounding sphere.
        let center = (a + b + c + d) * 0.25;
        let r2 = [a, b, c, d]
            .iter()
            .map(|&x| center.distance_squared(x))
            .fold(0.0, f64::max);
        best = (center, r2);
    }
    best
}

fn diametral(a: DVec3, b: DVec3) -> (DVec3, f64) {
    let center = (a + b) * 0.5;
    (center, center.distance_squared(a))
}

/// The smallest sphere containing `points` (Welzl's algorithm with its recursion unrolled:
/// expected linear time on shuffled points).
pub fn bounding_sphere(points: &[Vec3]) -> Sphere {
    engine_profiler::zone!("collision::fit_sphere");
    let p: Vec<DVec3> = shuffled(points).iter().map(|q| q.as_dvec3()).collect();
    let Some(&first) = p.first() else {
        return Sphere {
            center: Vec3::ZERO,
            radius: 0.0,
        };
    };
    let (mut c, mut r2) = (first, 0.0);
    for i in 1..p.len() {
        if inside_sphere(c, r2, p[i]) {
            continue;
        }
        // p[i] on the surface.
        (c, r2) = (p[i], 0.0);
        for j in 0..i {
            if inside_sphere(c, r2, p[j]) {
                continue;
            }
            (c, r2) = diametral(p[i], p[j]);
            for k in 0..j {
                if inside_sphere(c, r2, p[k]) {
                    continue;
                }
                (c, r2) =
                    sphere3(p[i], p[j], p[k]).unwrap_or_else(|| widest_pair(p[i], p[j], p[k]));
                for l in 0..k {
                    if inside_sphere(c, r2, p[l]) {
                        continue;
                    }
                    (c, r2) = sphere4(p[i], p[j], p[k], p[l])
                        .unwrap_or_else(|| smallest_of_three(p[i], p[j], p[k], p[l]));
                }
            }
        }
    }
    Sphere {
        center: c.as_vec3(),
        radius: r2.sqrt() as f32,
    }
}

fn inside_circle(c: DVec2, r2: f64, p: DVec2) -> bool {
    c.distance_squared(p) <= r2 * (1.0 + 1e-10) + SLACK
}

fn circle3(a: DVec2, b: DVec2, c: DVec2) -> Option<(DVec2, f64)> {
    let (u, v) = (b - a, c - a);
    let d = 2.0 * u.perp_dot(v);
    if d.abs() <= 1e-30 * (u.length_squared() * v.length_squared()).sqrt().max(1e-300) {
        return None;
    }
    let (u2, v2) = (u.length_squared(), v.length_squared());
    let x = DVec2::new(v.y * u2 - u.y * v2, u.x * v2 - v.x * u2) / d;
    Some((a + x, x.length_squared()))
}

/// The smallest circle containing `points` (Welzl).
fn bounding_circle(points: &[DVec2]) -> (DVec2, f64) {
    let p = shuffled(points);
    let Some(&first) = p.first() else {
        return (DVec2::ZERO, 0.0);
    };
    let half = |a: DVec2, b: DVec2| ((a + b) * 0.5, (a - b).length_squared() * 0.25);
    let (mut c, mut r2) = (first, 0.0);
    for i in 1..p.len() {
        if inside_circle(c, r2, p[i]) {
            continue;
        }
        (c, r2) = (p[i], 0.0);
        for j in 0..i {
            if inside_circle(c, r2, p[j]) {
                continue;
            }
            (c, r2) = half(p[i], p[j]);
            for k in 0..j {
                if inside_circle(c, r2, p[k]) {
                    continue;
                }
                (c, r2) = circle3(p[i], p[j], p[k]).unwrap_or_else(|| {
                    // Collinear: the two farthest apart.
                    let candidates = [half(p[i], p[j]), half(p[i], p[k]), half(p[j], p[k])];
                    candidates
                        .into_iter()
                        .fold((DVec2::ZERO, -1.0), |a, b| if b.1 > a.1 { b } else { a })
                });
            }
        }
    }
    (c, r2)
}

// ---------------------------------------------------------------------------------------
// Capsules and cylinders

/// The points seen along `axis`: the smallest circle around them (centre in 3D, on the
/// plane through the origin), and each point's position along the axis and distance from
/// the circle's centre line.
fn along_axis(points: &[DVec3], axis: DVec3) -> (DVec3, f64, Vec<(f64, f64)>) {
    let (u, v) = basis(axis);
    let flat: Vec<DVec2> = points
        .iter()
        .map(|p| DVec2::new(p.dot(u), p.dot(v)))
        .collect();
    let (c, r2) = bounding_circle(&flat);
    let line = u * c.x + v * c.y;
    let samples = points
        .iter()
        .zip(&flat)
        .map(|(p, f)| (p.dot(axis), f.distance(c)))
        .collect();
    (line, r2.sqrt(), samples)
}

/// The capsule along `axis` (unit) around `points` with the smallest volume.
fn capsule_along(points: &[DVec3], axis: DVec3) -> Capsule {
    let (line, r0, samples) = along_axis(points, axis);
    let (t_lo, t_hi) = samples
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &(t, _)| {
            (lo.min(t), hi.max(t))
        });
    let middle = 0.5 * (t_lo + t_hi);
    let r1 = samples
        .iter()
        .map(|&(t, rho)| (rho * rho + (t - middle) * (t - middle)).sqrt())
        .fold(r0, f64::max);
    // The segment that radius `r` needs: from the farthest point's reach at each end.
    let segment = |r: f64| {
        let (mut a, mut b) = (f64::MAX, f64::MIN);
        for &(t, rho) in &samples {
            let w = (r * r - rho * rho).max(0.0).sqrt();
            a = a.min(t + w);
            b = b.max(t - w);
        }
        (a, b)
    };
    let volume = |r: f64| {
        let (a, b) = segment(r);
        capsule_volume(r, (b - a).max(0.0))
    };
    // Coarse samples, then a golden-section search around the best.
    let steps = 16;
    let mut best = (volume(r0), r0);
    let mut best_k = 0;
    for k in 1..=steps {
        let r = r0 + (r1 - r0) * k as f64 / steps as f64;
        let v = volume(r);
        if v < best.0 {
            best = (v, r);
            best_k = k;
        }
    }
    let width = (r1 - r0) / steps as f64;
    let (mut lo, mut hi) = (
        (r0 + width * (best_k as f64 - 1.0)).max(r0),
        (r0 + width * (best_k as f64 + 1.0)).min(r1),
    );
    let golden = 0.5 * (5f64.sqrt() - 1.0);
    for _ in 0..40 {
        let (m1, m2) = (hi - golden * (hi - lo), lo + golden * (hi - lo));
        if volume(m1) < volume(m2) {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    let r = 0.5 * (lo + hi);
    let r = if volume(r) < best.0 { r } else { best.1 };
    let (a, b) = segment(r);
    let centre = line + axis * (0.5 * (a + b));
    Capsule {
        center: centre.as_vec3(),
        rotation: y_to(axis),
        half_height: (0.5 * (b - a).max(0.0)) as f32,
        radius: r as f32,
    }
}

/// The cylinder along `axis` (unit) around `points`.
fn cylinder_along(points: &[DVec3], axis: DVec3) -> Cylinder {
    let (line, r, samples) = along_axis(points, axis);
    let (lo, hi) = samples
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &(t, _)| {
            (lo.min(t), hi.max(t))
        });
    Cylinder {
        center: (line + axis * (0.5 * (lo + hi))).as_vec3(),
        rotation: y_to(axis),
        half_height: (0.5 * (hi - lo)) as f32,
        radius: r as f32,
    }
}

/// The smallest capsule found around `hull` (along each of its box's axes).
pub fn capsule(hull: &Hull) -> Capsule {
    let b = oriented_box(hull);
    capsule_for(hull, &b)
}

fn capsule_for(hull: &Hull, b: &OrientedBox) -> Capsule {
    engine_profiler::zone!("collision::fit_capsule");
    let points: Vec<DVec3> = hull.points.iter().map(|p| p.as_dvec3()).collect();
    let axes = DMat3::from_quat(b.rotation.as_dquat());
    (0..3)
        .map(|k| capsule_along(&points, axes.col(k).normalize()))
        .min_by(|x, y| x.volume().total_cmp(&y.volume()))
        .unwrap_or(Capsule {
            center: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_height: 0.0,
            radius: 0.0,
        })
}

/// The smallest cylinder found around `hull` (along each of its box's axes).
pub fn cylinder(hull: &Hull) -> Cylinder {
    let b = oriented_box(hull);
    cylinder_for(hull, &b)
}

fn cylinder_for(hull: &Hull, b: &OrientedBox) -> Cylinder {
    engine_profiler::zone!("collision::fit_cylinder");
    let points: Vec<DVec3> = hull.points.iter().map(|p| p.as_dvec3()).collect();
    let axes = DMat3::from_quat(b.rotation.as_dquat());
    (0..3)
        .map(|k| cylinder_along(&points, axes.col(k).normalize()))
        .min_by(|x, y| x.volume().total_cmp(&y.volume()))
        .unwrap_or(Cylinder {
            center: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_height: 0.0,
            radius: 0.0,
        })
}

/// Which primitives to consider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kinds {
    /// Oriented boxes.
    pub boxes: bool,
    /// Spheres.
    pub spheres: bool,
    /// Capsules.
    pub capsules: bool,
    /// Cylinders.
    pub cylinders: bool,
}

impl Kinds {
    /// Every kind.
    pub const ALL: Self = Self {
        boxes: true,
        spheres: true,
        capsules: true,
        cylinders: true,
    };
}

/// The smallest primitive of `kinds` found around `hull`.
pub fn smallest(hull: &Hull, kinds: Kinds) -> Option<Primitive> {
    let b = oriented_box(hull);
    let mut candidates = Vec::with_capacity(4);
    if kinds.boxes {
        candidates.push(Primitive::Box(b));
    }
    if kinds.spheres {
        candidates.push(Primitive::Sphere(bounding_sphere(&hull.points)));
    }
    if kinds.capsules {
        candidates.push(Primitive::Capsule(capsule_for(hull, &b)));
    }
    if kinds.cylinders {
        candidates.push(Primitive::Cylinder(cylinder_for(hull, &b)));
    }
    candidates
        .into_iter()
        .min_by(|x, y| x.volume().total_cmp(&y.volume()))
}

/// The smallest primitive around `hull` if its volume is at most `max_ratio` times the
/// hull's (1.1: within 10 %), otherwise none (keep the hull).
pub fn fit(hull: &Hull, max_ratio: f32) -> Option<Primitive> {
    let best = smallest(hull, Kinds::ALL)?;
    (best.volume() <= max_ratio * hull.volume()).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Random(u64);

    impl Random {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }

        fn on_sphere(&mut self) -> Vec3 {
            loop {
                let p = Vec3::new(self.next(), self.next(), self.next()) * 2.0 - 1.0;
                let l = p.length();
                if l > 0.1 && l <= 1.0 {
                    return p / l;
                }
            }
        }
    }

    fn rotation() -> Quat {
        Quat::from_euler(engine_core::math::EulerRot::YXZ, 0.7, -0.4, 1.1)
    }

    fn contains_all(p: &Primitive, points: &[Vec3], tolerance: f32) -> bool {
        points.iter().all(|&q| match p {
            Primitive::Box(b) => {
                let l = b.rotation.inverse() * (q - b.center);
                (l.abs() - b.half_extents).max_element() <= tolerance
            }
            Primitive::Sphere(s) => q.distance(s.center) <= s.radius + tolerance,
            Primitive::Capsule(c) => {
                let l = c.rotation.inverse() * (q - c.center);
                let y = l.y.clamp(-c.half_height, c.half_height);
                (l - Vec3::new(0.0, y, 0.0)).length() <= c.radius + tolerance
            }
            Primitive::Cylinder(c) => {
                let l = c.rotation.inverse() * (q - c.center);
                l.y.abs() <= c.half_height + tolerance
                    && Vec3::new(l.x, 0.0, l.z).length() <= c.radius + tolerance
            }
        })
    }

    #[test]
    fn a_turned_box_is_found() {
        let r = rotation();
        let half = Vec3::new(0.3, 1.2, 0.05);
        let centre = Vec3::new(4.0, -2.0, 1.0);
        let mut points = Vec::new();
        let mut random = Random(9);
        for i in 0..8 {
            let corner = Vec3::new(
                if i & 1 == 0 { -1.0 } else { 1.0 },
                if i & 2 == 0 { -1.0 } else { 1.0 },
                if i & 4 == 0 { -1.0 } else { 1.0 },
            );
            points.push(centre + r * (corner * half));
        }
        for _ in 0..100 {
            let l = (Vec3::new(random.next(), random.next(), random.next()) * 2.0 - 1.0) * half;
            points.push(centre + r * l);
        }
        let hull = Hull::new(&points).unwrap();
        let b = oriented_box(&hull);
        assert!(
            (b.volume() - 8.0 * half.x * half.y * half.z).abs() < 1e-3,
            "{}",
            b.volume()
        );
        assert!(contains_all(&Primitive::Box(b), &points, 1e-4));
        let chosen = fit(&hull, 1.1).unwrap();
        assert!(matches!(chosen, Primitive::Box(_)), "{chosen:?}");
    }

    #[test]
    fn a_sphere_is_found() {
        let mut random = Random(3);
        let points: Vec<Vec3> = (0..2000)
            .map(|_| Vec3::new(1.0, 2.0, 3.0) + random.on_sphere() * 0.5)
            .collect();
        let s = bounding_sphere(&points);
        assert!(s.center.distance(Vec3::new(1.0, 2.0, 3.0)) < 1e-3);
        assert!((s.radius - 0.5).abs() < 1e-3);
        let hull = Hull::new(&points).unwrap();
        let chosen = fit(&hull, 1.1).unwrap();
        assert!(matches!(chosen, Primitive::Sphere(_)), "{chosen:?}");
        assert!(contains_all(&chosen, &points, 1e-4));
    }

    #[test]
    fn a_capsule_and_a_cylinder_are_found() {
        let r = rotation();
        let mut random = Random(5);
        let (half_height, radius) = (0.8, 0.25);
        let mut capsule_points = Vec::new();
        let mut cylinder_points = Vec::new();
        for _ in 0..3000 {
            let d = random.on_sphere();
            let y = if d.y > 0.0 { half_height } else { -half_height };
            capsule_points.push(r * (d * radius + Vec3::new(0.0, y, 0.0)));
            let a = random.next() * core::f32::consts::TAU;
            let h = (random.next() * 2.0 - 1.0) * half_height;
            cylinder_points.push(r * Vec3::new(radius * a.cos(), h, radius * a.sin()));
            // The caps' rims.
            cylinder_points.push(r * Vec3::new(radius * a.cos(), half_height, radius * a.sin()));
            cylinder_points.push(r * Vec3::new(radius * a.cos(), -half_height, radius * a.sin()));
        }
        let hull = Hull::new(&capsule_points).unwrap();
        let chosen = fit(&hull, 1.1).unwrap();
        let Primitive::Capsule(c) = chosen else {
            panic!("{chosen:?}");
        };
        assert!((c.radius - radius).abs() < 0.01, "{c:?}");
        assert!((c.half_height - half_height).abs() < 0.02, "{c:?}");
        assert!(contains_all(&chosen, &capsule_points, 1e-4));
        let hull = Hull::new(&cylinder_points).unwrap();
        let chosen = fit(&hull, 1.1).unwrap();
        let Primitive::Cylinder(c) = chosen else {
            panic!("{chosen:?}");
        };
        assert!((c.radius - radius).abs() < 0.01, "{c:?}");
        assert!((c.half_height - half_height).abs() < 1e-3, "{c:?}");
        assert!(contains_all(&chosen, &cylinder_points, 1e-4));
    }

    #[test]
    fn concave_or_odd_shapes_keep_their_hull() {
        // A tetrahedron: every primitive around it is much bigger.
        let points = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
        ];
        let hull = Hull::new(&points).unwrap();
        assert!(fit(&hull, 1.1).is_none());
        let smallest = smallest(&hull, Kinds::ALL).unwrap();
        assert!(contains_all(&smallest, &points, 1e-5));
    }

    #[test]
    fn a_flat_set_gets_a_thin_box() {
        let points: Vec<Vec3> = (0..20)
            .map(|i| rotation() * Vec3::new((i % 5) as f32, 0.0, (i / 5) as f32))
            .collect();
        assert!(Hull::new(&points).is_err());
        let b = flat_box(&points, 0.01);
        assert!((b.half_extents.min_element() - 0.005).abs() < 1e-6, "{b:?}");
        assert!(contains_all(&Primitive::Box(b), &points, 1e-4));
    }

    #[test]
    fn eigenvectors_of_a_diagonal_and_a_turned_matrix() {
        let d = DMat3::from_diagonal(DVec3::new(3.0, 1.0, 2.0));
        let e = eigenvectors(d);
        for k in 0..3 {
            let v = e.col(k);
            let mv = d * v;
            assert!(mv.cross(v).length() < 1e-9);
        }
        let r = DMat3::from_quat(rotation().as_dquat());
        let m = r * d * r.transpose();
        let e = eigenvectors(m);
        assert!((e.determinant() - 1.0).abs() < 1e-9);
        for k in 0..3 {
            let v = e.col(k);
            assert!((m * v).cross(v).length() < 1e-9);
        }
    }
}
