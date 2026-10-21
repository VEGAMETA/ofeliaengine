//! Exact arithmetic on planes (ADR 0110): integer planes, the points where three of them
//! meet, and which side of a plane such a point lies on, with no rounding in any decision.
//!
//! After Nehring-Wirxel, Trettner and Kobbelt, "Fast Exact Booleans for Iterated CSG using
//! Octree-Embedded BSPs" (2021): a point is never stored as rounded coordinates, only as
//! the three planes it lies on, in homogeneous coordinates; a side is the sign of one
//! integer expression. With normals of at most [`MAX_NORMAL`] and planes through grid
//! points within [`MAX_COORD`], every product and sum fits in `i128` (the bounds below).
//! Rounding happens only where an edit makes a plane ([`Plane::through`]).

use core::cmp::Ordering;
use engine_core::math::{DMat3, DVec3};

/// The largest normal component: 2^22. A normal rounded to it is within about 2·10⁻⁷ rad
/// of the one asked for.
pub const MAX_NORMAL: i64 = 1 << 22;

/// The largest coordinate of a grid point a plane may pass through: 2^26 grid units,
/// ±67 km on a 1 mm grid.
pub const MAX_COORD: i64 = 1 << 26;

/// The largest offset: |n·p| with |nᵢ| ≤ [`MAX_NORMAL`] and |pᵢ| ≤ [`MAX_COORD`].
pub const MAX_OFFSET: i64 = 3 * MAX_NORMAL * MAX_COORD;

// The bounds, in bits: a cross product of two normals is below 2^45; a point's x, y, z
// (three offsets times cross products) below 2^97 and its w (a normal times a cross
// product, three times) below 2^69; a side (three normals times x, y, z, and an offset
// times w) below 2^121, under i128's 2^127.
const _: () = assert!(MAX_OFFSET < 1 << 50);

/// A plane n·x = d, in grid units. The solid it bounds lies behind it (n·x ≤ d): `n`
/// points out.
///
/// Planes are kept reduced (the normal's components share no factor), so two equal planes
/// are equal values, and a plane turned around ([`Plane::flip`]) is the negated value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Plane {
    /// The normal, each component within ±[`MAX_NORMAL`], not all zero.
    pub n: [i32; 3],
    /// The offset, within ±[`MAX_OFFSET`].
    pub d: i64,
}

impl Plane {
    /// The plane `n·x = d`, reduced; `None` when the normal is zero or a component or the
    /// offset is out of bounds.
    pub fn new(n: [i64; 3], d: i64) -> Option<Self> {
        if n == [0; 3] || n.iter().any(|c| c.abs() > MAX_NORMAL) || d.abs() > MAX_OFFSET {
            return None;
        }
        let g = gcd(n[0].unsigned_abs(), n[1].unsigned_abs());
        let g = gcd(gcd(g, n[2].unsigned_abs()), d.unsigned_abs()).max(1) as i64;
        Some(Self {
            n: n.map(|c| (c / g) as i32),
            d: d / g,
        })
    }

    /// The plane through grid point `p` facing `normal` (any length): the normal scaled so
    /// that its largest component is [`MAX_NORMAL`] and rounded, unless it is a multiple of
    /// an axis or a diagonal, which stay exact. `None` for a zero or non-finite normal or a
    /// point out of bounds.
    pub fn through(normal: DVec3, p: [i64; 3]) -> Option<Self> {
        if p.iter().any(|c| c.abs() > MAX_COORD) {
            return None;
        }
        let largest = normal.abs().max_element();
        if !largest.is_finite() || largest == 0.0 {
            return None;
        }
        let scaled = normal * (MAX_NORMAL as f64 / largest);
        let n = [scaled.x, scaled.y, scaled.z].map(|c| c.round() as i64);
        let d = n[0] * p[0] + n[1] * p[1] + n[2] * p[2];
        Self::new(n, d)
    }

    /// The plane through `point` (grid units, any precision) facing `normal`: a normal of
    /// whole numbers within [`MAX_NORMAL`] stays as it is (so moves and quarter turns of
    /// integer planes are exact), any other is scaled so that its largest component is
    /// [`MAX_NORMAL`] and rounded; the offset is rounded (the plane moves less than a
    /// unit's 2^-22). `None` for a zero or non-finite normal or a point out of bounds.
    pub fn from_f64(normal: DVec3, point: DVec3) -> Option<Self> {
        let largest = normal.abs().max_element();
        if !largest.is_finite() || largest == 0.0 || !point.is_finite() {
            return None;
        }
        if point.abs().max_element() > MAX_COORD as f64 {
            return None;
        }
        let whole = |c: f64| (c - c.round()).abs() <= 1e-6 * c.abs().max(1.0);
        let n = if [normal.x, normal.y, normal.z].into_iter().all(whole)
            && largest <= MAX_NORMAL as f64
        {
            normal.round()
        } else {
            // Components far below the largest are rounding of an exact zero.
            let scaled = normal * (MAX_NORMAL as f64 / largest);
            scaled.round()
        };
        let d = n.dot(point).round();
        Self::new([n.x as i64, n.y as i64, n.z as i64], d as i64)
    }

    /// This plane of a part in its brush's grid, in its group's: `linear` and `offset` take
    /// the brush's grid points to the group's (the brush's placement, in grid units).
    pub fn transformed(&self, linear: DMat3, offset: DVec3) -> Option<Self> {
        let n = DVec3::new(self.n[0] as f64, self.n[1] as f64, self.n[2] as f64);
        // A point of the plane: the one nearest the brush's origin.
        let on = n * (self.d as f64 / n.length_squared());
        let normal = linear.inverse().transpose() * n;
        // Normals of whole numbers keep their length through quarter turns.
        let normal = if linear.determinant().abs() > 0.0 {
            normal * (n.length() / normal.length())
        } else {
            return None;
        };
        Self::from_f64(normal, linear * on + offset)
    }

    /// The same plane facing the other way: what lies in front of this one is behind it.
    pub fn flip(self) -> Self {
        Self {
            n: self.n.map(|c| -c),
            d: -self.d,
        }
    }

    /// Its normal as a unit vector (for output).
    pub fn normal(&self) -> DVec3 {
        DVec3::new(self.n[0] as f64, self.n[1] as f64, self.n[2] as f64).normalize()
    }

    /// Whether `other` is parallel to it (facing either way).
    pub fn parallel(&self, other: &Self) -> bool {
        cross(self.n, other.n) == [0; 3]
    }

    /// Which side of it grid point `p` lies on: `Less` behind, `Equal` on, `Greater` in
    /// front.
    pub fn side_of_grid(&self, p: [i64; 3]) -> Ordering {
        let n = self.n.map(i128::from);
        let dot = n[0] * i128::from(p[0]) + n[1] * i128::from(p[1]) + n[2] * i128::from(p[2]);
        dot.cmp(&i128::from(self.d))
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn cross(a: [i32; 3], b: [i32; 3]) -> [i64; 3] {
    let [a0, a1, a2] = a.map(i64::from);
    let [b0, b1, b2] = b.map(i64::from);
    [a1 * b2 - a2 * b1, a2 * b0 - a0 * b2, a0 * b1 - a1 * b0]
}

/// A point where three planes meet: (x, y, z) / w in grid units, with w > 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    /// Homogeneous x.
    pub x: i128,
    /// Homogeneous y.
    pub y: i128,
    /// Homogeneous z.
    pub z: i128,
    /// The common denominator, positive.
    pub w: i128,
}

impl Point {
    /// Where planes `a`, `b` and `c` meet; `None` when they do not meet in one point (two
    /// of them parallel, or all three through one line). Their orientations do not matter.
    pub fn meet(a: &Plane, b: &Plane, c: &Plane) -> Option<Self> {
        // Cramer's rule: x = (d_a (n_b × n_c) + d_b (n_c × n_a) + d_c (n_a × n_b)) / w,
        // w = n_a · (n_b × n_c).
        let bc = cross(b.n, c.n).map(i128::from);
        let ca = cross(c.n, a.n).map(i128::from);
        let ab = cross(a.n, b.n).map(i128::from);
        let na = a.n.map(i128::from);
        let w = na[0] * bc[0] + na[1] * bc[1] + na[2] * bc[2];
        if w == 0 {
            return None;
        }
        let (da, db, dc) = (i128::from(a.d), i128::from(b.d), i128::from(c.d));
        let x = da * bc[0] + db * ca[0] + dc * ab[0];
        let y = da * bc[1] + db * ca[1] + dc * ab[1];
        let z = da * bc[2] + db * ca[2] + dc * ab[2];
        let s = w.signum();
        Some(Self {
            x: x * s,
            y: y * s,
            z: z * s,
            w: w * s,
        })
    }

    /// Which side of plane `p` it lies on: `Less` behind, `Equal` on, `Greater` in front.
    #[inline]
    pub fn side(&self, p: &Plane) -> Ordering {
        let n = p.n.map(i128::from);
        let dot = n[0] * self.x + n[1] * self.y + n[2] * self.z;
        dot.cmp(&(i128::from(p.d) * self.w))
    }

    /// Its coordinates in grid units, rounded to f64 (for output and bounds: within a few
    /// units in the last place).
    pub fn to_f64(&self) -> DVec3 {
        let w = approx(self.w);
        DVec3::new(approx(self.x) / w, approx(self.y) / w, approx(self.z) / w)
    }

    /// Whether it is the same point as `other`, exactly (the cross products in 256 bits).
    pub fn same(&self, other: &Self) -> bool {
        mul_eq(self.x, other.w, other.x, self.w)
            && mul_eq(self.y, other.w, other.y, self.w)
            && mul_eq(self.z, other.w, other.z, self.w)
    }
}

/// `v` as f64 from its two 64-bit halves: two hardware conversions instead of the
/// software one for `i128` (a unit or two in the last place).
#[inline]
fn approx(v: i128) -> f64 {
    let a = v.unsigned_abs();
    let f = (a >> 64) as u64 as f64 * 18_446_744_073_709_551_616.0 + a as u64 as f64;
    if v < 0 { -f } else { f }
}

/// Whether a · b = c · d, exactly, for values below 2^97 and 2^69 (products up to 2^166).
fn mul_eq(a: i128, b: i128, c: i128, d: i128) -> bool {
    let (ah, al) = wide_mul(a, b);
    let (ch, cl) = wide_mul(c, d);
    ah == ch && al == cl
}

/// a · b as (high, low) 128-bit halves of the 256-bit product (two's complement).
fn wide_mul(a: i128, b: i128) -> (i128, u128) {
    let negative = (a < 0) != (b < 0);
    let (a, b) = (a.unsigned_abs(), b.unsigned_abs());
    let (a1, a0) = (a >> 64, a & u128::from(u64::MAX));
    let (b1, b0) = (b >> 64, b & u128::from(u64::MAX));
    let low = a0 * b0;
    let mid1 = a1 * b0;
    let mid2 = a0 * b1;
    let high = a1 * b1;
    let (mid, carry_mid) = mid1.overflowing_add(mid2);
    let (low, carry_low) = low.overflowing_add(mid << 64);
    let high = high + (mid >> 64) + (u128::from(carry_mid) << 64) + u128::from(carry_low);
    if negative {
        // Two's complement of the 256-bit value.
        let low_neg = (!low).wrapping_add(1);
        let high_neg = (!high).wrapping_add(u128::from(low == 0));
        (high_neg as i128, low_neg)
    } else {
        (high as i128, low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(n: [i64; 3], d: i64) -> Plane {
        Plane::new(n, d).unwrap_or_else(|| panic!("plane {n:?} {d}"))
    }

    #[test]
    fn planes_are_reduced_so_equal_planes_are_equal_values() {
        assert_eq!(plane([0, 0, 4], 8), plane([0, 0, 1], 2));
        assert_eq!(plane([2, 2, 0], 6), plane([1, 1, 0], 3));
        assert_eq!(plane([0, 0, 1], 2).flip(), plane([0, 0, -1], -2));
        // No grid point on it: kept as given.
        assert_eq!(plane([0, 0, 2], 3).n, [0, 0, 2]);
        assert!(Plane::new([0, 0, 0], 1).is_none());
        assert!(Plane::new([MAX_NORMAL + 1, 0, 0], 0).is_none());
        assert!(Plane::new([1, 0, 0], MAX_OFFSET + 1).is_none());
    }

    #[test]
    fn a_plane_through_a_grid_point_contains_it_and_keeps_axes_exact() {
        let p = [1000, -2000, 3000];
        let axis = Plane::through(DVec3::new(0.0, 0.0, 5.0), p).unwrap_or_else(|| panic!());
        assert_eq!(axis, plane([0, 0, 1], 3000));
        let diagonal = Plane::through(DVec3::new(1.0, 1.0, 0.0), p).unwrap_or_else(|| panic!());
        assert_eq!(diagonal, plane([1, 1, 0], -1000));
        let tilted = Plane::through(DVec3::new(0.3, 0.7, -0.2), p).unwrap_or_else(|| panic!());
        assert_eq!(tilted.side_of_grid(p), Ordering::Equal);
        let dir = tilted.normal();
        let want = DVec3::new(0.3, 0.7, -0.2).normalize();
        assert!(
            dir.angle_between(want) < 3e-7,
            "{}",
            dir.angle_between(want)
        );
        assert!(Plane::through(DVec3::ZERO, p).is_none());
        assert!(Plane::through(DVec3::X, [MAX_COORD + 1, 0, 0]).is_none());
    }

    #[test]
    fn three_planes_meet_at_one_point_whatever_their_orientation() {
        let x = plane([1, 0, 0], 10);
        let y = plane([0, 1, 0], -20);
        let z = plane([0, 0, 1], 30);
        let p = Point::meet(&x, &y, &z).unwrap_or_else(|| panic!());
        assert_eq!(p.to_f64(), DVec3::new(10.0, -20.0, 30.0));
        let q = Point::meet(&x.flip(), &z, &y.flip()).unwrap_or_else(|| panic!());
        assert!(p.same(&q));
        assert!(p.w > 0 && q.w > 0);
        // Parallel planes, or three through one line, meet nowhere in particular.
        assert!(Point::meet(&x, &x.flip(), &z).is_none());
        assert!(Point::meet(&x, &y, &plane([1, 1, 0], -10)).is_none());
    }

    #[test]
    fn sides_are_exact_where_floats_would_round() {
        // A point of three tilted planes with large offsets lies exactly on the sum of two
        // of them, which its rounded coordinates need not.
        let m = MAX_NORMAL;
        let a = plane([m, 1, 0], 123_456_789_012);
        let b = plane([1, m - 1, 3], -98_765_432_101);
        let c = plane([2, -5, m], 55_555_555_555);
        let p = Point::meet(&a, &b, &c).unwrap_or_else(|| panic!());
        for s in [a, b, c] {
            assert_eq!(p.side(&s), Ordering::Equal);
            assert_eq!(p.side(&s.flip()), Ordering::Equal);
        }
        let on_both = Plane {
            n: [a.n[0] + b.n[0], a.n[1] + b.n[1], a.n[2] + b.n[2]],
            d: a.d + b.d,
        };
        assert_eq!(p.side(&on_both), Ordering::Equal);
        let nudged = Plane {
            d: on_both.d + 1,
            ..on_both
        };
        assert_eq!(p.side(&nudged), Ordering::Less);
        // A plane through a far corner of the bounds: the corner is on it.
        let corner = [MAX_COORD, -MAX_COORD, MAX_COORD];
        let tilted =
            Plane::through(DVec3::new(1.0, 1.0, 1.0 + 1e-6), corner).unwrap_or_else(|| panic!());
        let q = Point::meet(
            &plane([1, 0, 0], MAX_COORD),
            &plane([0, 1, 0], -MAX_COORD),
            &tilted,
        )
        .unwrap_or_else(|| panic!());
        assert_eq!(q.side(&plane([0, 0, 1], MAX_COORD)), Ordering::Equal);
        assert_eq!(q.side(&plane([0, 0, 1], MAX_COORD - 1)), Ordering::Greater);
        assert_eq!(q.side(&plane([0, 0, 1], MAX_COORD + 1)), Ordering::Less);
    }

    #[test]
    fn the_largest_values_do_not_overflow() {
        // Planes at the bounds in every direction: debug builds check every operation.
        let m = MAX_NORMAL;
        let planes = [
            Plane::through(DVec3::new(1.0, 0.999_999, -0.999_998), [MAX_COORD; 3]),
            Plane::through(DVec3::new(-0.999_997, 1.0, 0.999_996), [-MAX_COORD; 3]),
            Plane::through(
                DVec3::new(0.999_995, -0.999_994, 1.0),
                [MAX_COORD, -MAX_COORD, MAX_COORD],
            ),
        ]
        .map(|p| p.unwrap_or_else(|| panic!()));
        let p = Point::meet(&planes[0], &planes[1], &planes[2]).unwrap_or_else(|| panic!());
        for s in planes {
            assert_eq!(p.side(&s), Ordering::Equal);
        }
        // Not reducible: the normal's components share no factor.
        let far = plane([m, m - 1, -m], MAX_OFFSET);
        assert_eq!(far.n, [m as i32, (m - 1) as i32, -m as i32]);
        let _ = p.side(&far);
        let _ = p.side(&far.flip());
    }

    #[test]
    fn wide_products_compare_exactly() {
        let big = 1i128 << 100;
        assert!(mul_eq(big, 3, 3, big));
        assert!(mul_eq(-big, 5, big, -5));
        assert!(!mul_eq(big, 3, big + 1, 3));
        assert!(!mul_eq(big, -3, big, 3));
        let (h, l) = wide_mul(-1, 1);
        assert_eq!((h, l), (-1, u128::MAX));
    }
}
