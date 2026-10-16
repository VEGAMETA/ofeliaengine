//! How far a part is from convex (ADR 0101), after CoACD's collision-aware concavity (Wei et
//! al. 2022): the larger of
//!
//! - **a volume term**: the radius of a sphere with the volume the hull adds to the part,
//!   times `k` (0.3);
//! - **a boundary Hausdorff distance** between the part's surface and its hull's, from
//!   points sampled on both: hull points to the part's surface (a BVH), part points to the
//!   hull's boundary (the nearest face plane: exact inside a convex hull).
//!
//! Samples are spread by area with Roberts's R2 sequence (2018, public domain) for the
//! points within each triangle: deterministic, no random state. CoACD's Sobol sequence is
//! LGPL and not used. Written for this engine; no code was copied.

use super::bvh::Bvh;
use super::clip::Solid;
use engine_core::math::DVec3;

/// The volume term's weight (CoACD's `rv_k`).
pub(crate) const VOLUME_WEIGHT: f64 = 0.3;

/// The volume term: `k` times the radius of a sphere of the hull's extra volume.
pub(crate) fn volume_term(part_volume: f64, hull_volume: f64) -> f64 {
    VOLUME_WEIGHT * (3.0 * (hull_volume - part_volume).abs() / (4.0 * core::f64::consts::PI)).cbrt()
}

/// Points spread over `solid`'s surface by area: about `count`, at least one on every
/// triangle larger than the average.
pub(crate) fn sample_surface(solid: &Solid, count: usize) -> Vec<DVec3> {
    let area = solid.area();
    let mut out = Vec::with_capacity(count + solid.triangles.len() / 2);
    if area <= 0.0 || count == 0 {
        return out;
    }
    // R2: the plastic number's powers.
    const G: f64 = 1.324_717_957_244_746;
    let (a1, a2) = (1.0 / G, 1.0 / (G * G));
    let mut n = 0u64;
    let mut carry = 0.0;
    for t in &solid.triangles {
        let [a, b, c] = t.map(|i| solid.points[i as usize]);
        let share = 0.5 * (b - a).cross(c - a).length() / area * count as f64 + carry;
        let k = share.floor();
        carry = share - k;
        for _ in 0..k as u64 {
            n += 1;
            let (mut u, mut v) = ((0.5 + a1 * n as f64).fract(), (0.5 + a2 * n as f64).fract());
            if u + v > 1.0 {
                (u, v) = (1.0 - u, 1.0 - v);
            }
            out.push(a + (b - a) * u + (c - a) * v);
        }
    }
    out
}

/// A convex hull's faces as planes (outward unit normal, offset).
pub(crate) fn planes(hull: &Solid) -> Vec<(DVec3, f64)> {
    hull.triangles
        .iter()
        .filter_map(|t| {
            let [a, b, c] = t.map(|i| hull.points[i as usize]);
            let n = (b - a).cross(c - a);
            let l = n.length();
            (l > 0.0).then(|| {
                let n = n / l;
                (n, n.dot(a))
            })
        })
        .collect()
}

/// Within this of another part's surface (in the normalized frame), a point on one part's
/// surface lies where the two touch: inside their union, not on its boundary.
const TOUCH: f64 = 1e-9;

/// The boundary Hausdorff distance between the union of `parts` (closed, each with its BVH;
/// touching, not overlapping) and their convex `hull`, from about `count` points on each
/// surface. Points of one part's surface on another's (the faces of a cut between them) are
/// inside the union and skipped: two halves of a box merge back with no concavity.
pub(crate) fn hausdorff(parts: &[(&Solid, &Bvh)], hull: &Solid, count: usize) -> f64 {
    let planes = planes(hull);
    let total: f64 = parts.iter().map(|(s, _)| s.area()).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let mut h = 0.0f64;
    // Part points to the hull's boundary: inside a convex hull, the nearest face plane.
    let touch = TOUCH * TOUCH;
    for (k, (part, _)) in parts.iter().enumerate() {
        let share = (count as f64 * part.area() / total).round() as usize;
        for p in sample_surface(part, share) {
            let inner = parts
                .iter()
                .enumerate()
                .any(|(m, (_, bvh))| m != k && bvh.distance_squared(p, touch) < touch);
            if inner {
                continue;
            }
            let d = planes
                .iter()
                .map(|&(n, o)| (o - n.dot(p)).abs())
                .fold(f64::MAX, f64::min);
            h = h.max(d);
        }
    }
    // Hull points to the union's surface: the nearest part's.
    for p in sample_surface(hull, count) {
        let d = parts
            .iter()
            .fold(f64::MAX, |best, (_, bvh)| bvh.distance_squared(p, best));
        h = h.max(d.sqrt());
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::clip::tests::cuboid;

    #[test]
    fn samples_cover_the_surface_by_area() {
        let box_ = cuboid(DVec3::ZERO, DVec3::new(4.0, 1.0, 1.0));
        let samples = sample_surface(&box_, 1800);
        assert!(
            (samples.len() as i64 - 1800).abs() <= 2,
            "{}",
            samples.len()
        );
        // The four long faces hold 16/18 of the area.
        let on_ends = samples
            .iter()
            .filter(|p| p.x < 1e-9 || p.x > 4.0 - 1e-9)
            .count();
        assert!(
            (on_ends as f64 / samples.len() as f64 - 2.0 / 18.0).abs() < 0.01,
            "{on_ends}"
        );
    }

    #[test]
    fn a_convex_part_has_no_concavity_and_an_l_has_some() {
        let box_ = cuboid(DVec3::ZERO, DVec3::ONE);
        let bvh = Bvh::new(&box_.points, &box_.triangles);
        assert!(hausdorff(&[(&box_, &bvh)], &box_, 2000) < 1e-9);
        assert!(volume_term(1.0, 1.0) == 0.0);
        // The volume term of a unit missing volume.
        let r = volume_term(1.0, 2.0) / VOLUME_WEIGHT;
        assert!((4.0 / 3.0 * core::f64::consts::PI * r.powi(3) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn two_halves_of_a_box_have_no_concavity_together() {
        // The cut between them is inside their union: its faces must not count.
        let left = cuboid(DVec3::ZERO, DVec3::new(0.5, 1.0, 1.0));
        let right = cuboid(DVec3::new(0.5, 0.0, 0.0), DVec3::ONE);
        let (lb, rb) = (
            Bvh::new(&left.points, &left.triangles),
            Bvh::new(&right.points, &right.triangles),
        );
        let whole = cuboid(DVec3::ZERO, DVec3::ONE);
        assert!(hausdorff(&[(&left, &lb), (&right, &rb)], &whole, 2000) < 1e-9);
        // Apart, the gap between them is concave.
        let far = cuboid(DVec3::new(0.7, 0.0, 0.0), DVec3::new(1.2, 1.0, 1.0));
        let fb = Bvh::new(&far.points, &far.triangles);
        let both = cuboid(DVec3::ZERO, DVec3::new(1.2, 1.0, 1.0));
        assert!(hausdorff(&[(&left, &lb), (&far, &fb)], &both, 2000) > 0.05);
    }
}
