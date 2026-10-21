//! Planes held once per group ([`Planes`]) and convex polygons bounded by them
//! ([`Polygon`]): a polygon on one plane, each of its edges the line where another plane
//! meets it, each corner a [`Point`] of three planes. Clipping keeps every corner exact.

use crate::exact::{Plane, Point};
use core::cmp::Ordering;
use std::collections::HashMap;

/// A plane of a [`Planes`] table: its entry, and whether it is that entry turned around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlaneId(u32);

impl PlaneId {
    /// The same plane facing the other way.
    #[inline]
    pub fn flip(self) -> Self {
        Self(self.0 ^ 1)
    }

    /// Whether it is one of the table's bounding box planes ([`Planes::BOUND`]).
    #[inline]
    pub fn is_bound(self) -> bool {
        self.0 < 12
    }
}

/// The planes of a brush group, each once: equal planes get one id, and a plane turned
/// around gets the other id of the same entry. The first six entries are the planes of a
/// box of half-size [`Planes::BOUND`], beyond any brush, that polygons start from.
#[derive(Debug, Clone)]
pub struct Planes {
    /// By id: each entry facing its own way, then turned around.
    list: Vec<Plane>,
    lookup: HashMap<Plane, u32>,
}

impl Default for Planes {
    fn default() -> Self {
        Self::new()
    }
}

impl Planes {
    /// The half-size of the box polygons start from, in grid units: beyond
    /// [`crate::exact::MAX_COORD`].
    pub const BOUND: i64 = 1 << 27;

    /// A table with only the bounding box's planes.
    pub fn new() -> Self {
        let mut planes = Self {
            list: Vec::new(),
            lookup: HashMap::new(),
        };
        for axis in 0..3 {
            for sign in [1, -1] {
                let mut n = [0; 3];
                n[axis] = sign;
                if let Some(p) = Plane::new(n, Self::BOUND) {
                    planes.push(p);
                }
            }
        }
        planes
    }

    fn push(&mut self, p: Plane) -> PlaneId {
        let id = self.list.len() as u32;
        self.list.push(p);
        self.list.push(p.flip());
        self.lookup.insert(p, id);
        self.lookup.insert(p.flip(), id + 1);
        PlaneId(id)
    }

    /// The id of `p`, added if it is new.
    pub fn add(&mut self, p: Plane) -> PlaneId {
        match self.lookup.get(&p) {
            Some(&id) => PlaneId(id),
            None => self.push(p),
        }
    }

    /// The plane of `id`.
    #[inline]
    pub fn get(&self, id: PlaneId) -> &Plane {
        &self.list[id.0 as usize]
    }

    /// The bounding box's plane facing out along `axis` (0, 1, 2) in direction `sign`.
    fn bound(axis: usize, positive: bool) -> PlaneId {
        PlaneId((axis as u32 * 2 + u32::from(!positive)) * 2)
    }

    /// How many planes (each counted once) it holds, the bounding box's included.
    pub fn len(&self) -> usize {
        self.list.len() / 2
    }

    /// Whether it holds only the bounding box's planes.
    pub fn is_empty(&self) -> bool {
        self.len() <= 6
    }
}

/// A convex polygon on plane `on`, seen counter-clockwise from in front of it: edge `i`
/// runs from corner `i` to corner `i + 1` along the line where `on` meets `edges[i]`, and
/// corner `i` is where `on`, `edges[i - 1]` and `edges[i]` meet. Its inside is behind
/// every edge plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polygon {
    /// The plane it lies on.
    pub on: PlaneId,
    /// The planes of its edges, in order.
    pub edges: Vec<PlaneId>,
    /// Its corners, in order.
    pub corners: Vec<Point>,
}

/// What lies behind a plane of a polygon or a solid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Split<T> {
    /// All of it lies behind (or on) the plane.
    Behind,
    /// All of it lies in front (or on).
    Front,
    /// The parts behind and in front.
    Both(T, T),
}

impl Polygon {
    /// A square on `on`, bounded by the bounding box's planes across the axis it faces
    /// most, larger than anything a brush holds.
    pub fn square(on: PlaneId, planes: &Planes) -> Option<Self> {
        let n = planes.get(on).n;
        let k = (0..3).max_by_key(|&i| n[i].unsigned_abs())?;
        let (i, j) = ((k + 1) % 3, (k + 2) % 3);
        // Counter-clockwise about +k: +i, +j, -i, -j; about -k the other way.
        let mut edges = vec![
            Planes::bound(i, true),
            Planes::bound(j, true),
            Planes::bound(i, false),
            Planes::bound(j, false),
        ];
        if n[k] < 0 {
            edges = vec![edges[2], edges[1], edges[0], edges[3]];
        }
        Self::from_edges(on, edges, planes)
    }

    /// The polygon on `on` with these edges, its corners computed.
    fn from_edges(on: PlaneId, edges: Vec<PlaneId>, planes: &Planes) -> Option<Self> {
        let m = edges.len();
        let mut corners = Vec::with_capacity(m);
        for k in 0..m {
            let prev = edges[(k + m - 1) % m];
            corners.push(Point::meet(
                planes.get(on),
                planes.get(prev),
                planes.get(edges[k]),
            )?);
        }
        Some(Self { on, edges, corners })
    }

    /// Which side of `s` each corner lies on.
    pub fn sides(&self, s: PlaneId, planes: &Planes, out: &mut Vec<Ordering>) {
        let plane = planes.get(s);
        out.clear();
        out.extend(self.corners.iter().map(|c| c.side(plane)));
    }

    /// The part of it behind `s`, and the part in front.
    pub fn split(&self, s: PlaneId, planes: &Planes, sides: &mut Vec<Ordering>) -> Split<Self> {
        self.sides(s, planes, sides);
        match mixed(sides) {
            Some(split) => split,
            None => {
                let behind = self.part_behind(s, sides, planes);
                for x in sides.iter_mut() {
                    *x = x.reverse();
                }
                let front = self.part_behind(s.flip(), sides, planes);
                match (behind, front) {
                    (Some(b), Some(f)) => Split::Both(b, f),
                    // Only rounding could get here, and there is none.
                    (Some(_), None) => Split::Behind,
                    _ => Split::Front,
                }
            }
        }
    }

    /// The part of it behind `s`: `None` when nothing is.
    pub fn clip(&self, s: PlaneId, planes: &Planes, sides: &mut Vec<Ordering>) -> Clipped<Self> {
        self.sides(s, planes, sides);
        match mixed(sides) {
            Some(Split::Behind) => Clipped::Whole,
            Some(_) => Clipped::Gone,
            None => match self.part_behind(s, sides, planes) {
                Some(p) => Clipped::Part(p),
                None => Clipped::Gone,
            },
        }
    }

    /// The part behind `s` of a polygon with corners on both sides of it (`sides`).
    fn part_behind(&self, s: PlaneId, sides: &[Ordering], planes: &Planes) -> Option<Self> {
        const NONE: usize = usize::MAX;
        let m = self.edges.len();
        let mut edges = Vec::with_capacity(m + 1);
        let mut from = Vec::with_capacity(m + 1);
        for i in 0..m {
            let j = (i + 1) % m;
            // Edge i keeps a part behind s.
            if sides[i].min(sides[j]) == Ordering::Less {
                edges.push(self.edges[i]);
                from.push(i);
                // Its end is the last behind: s follows (one corner in front between two
                // kept edges, or the edges after it gone).
                let next_gone = sides[j].min(sides[(j + 1) % m]) != Ordering::Less;
                if sides[j] == Ordering::Greater || next_gone {
                    edges.push(s);
                    from.push(NONE);
                }
            }
        }
        let n = edges.len();
        if n < 3 {
            return None;
        }
        let mut corners = Vec::with_capacity(n);
        for k in 0..n {
            let p = (k + n - 1) % n;
            let c = if from[p] != NONE && from[k] != NONE && (from[p] + 1) % m == from[k] {
                self.corners[from[k]]
            } else {
                Point::meet(
                    planes.get(self.on),
                    planes.get(edges[p]),
                    planes.get(edges[k]),
                )?
            };
            corners.push(c);
        }
        Some(Self {
            on: self.on,
            edges,
            corners,
        })
    }

    /// The same polygon seen from behind: on the plane turned around, its corners the other
    /// way.
    pub fn reversed(&self) -> Self {
        let m = self.edges.len();
        Self {
            on: self.on.flip(),
            edges: (0..m).map(|k| self.edges[(2 * m - 2 - k) % m]).collect(),
            corners: (0..m).map(|k| self.corners[m - 1 - k]).collect(),
        }
    }

    /// Whether an edge lies on the bounding box: the polygon reached it, so what it bounds
    /// was open there.
    pub fn touches_bound(&self) -> bool {
        self.edges.iter().any(|e| e.is_bound())
    }
}

/// What lies behind a plane of a polygon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clipped<T> {
    /// All of it.
    Whole,
    /// None of it.
    Gone,
    /// A part.
    Part(T),
}

/// `None` when the corners lie on both sides; otherwise which side holds it all (on the
/// plane counts as behind when nothing is in front).
fn mixed(sides: &[Ordering]) -> Option<Split<Polygon>> {
    let (mut behind, mut front) = (false, false);
    for s in sides {
        match s {
            Ordering::Less => behind = true,
            Ordering::Greater => front = true,
            Ordering::Equal => {}
        }
    }
    match (behind, front) {
        (true, true) => None,
        (_, false) => Some(Split::Behind),
        (false, true) => Some(Split::Front),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::DVec3;

    fn id(planes: &mut Planes, n: [i64; 3], d: i64) -> PlaneId {
        planes.add(Plane::new(n, d).unwrap_or_else(|| panic!()))
    }

    /// The polygon's corners as floats, and its normal from them (Newell's method).
    fn corners_and_normal(p: &Polygon) -> (Vec<DVec3>, DVec3) {
        let c: Vec<DVec3> = p.corners.iter().map(|c| c.to_f64()).collect();
        let mut n = DVec3::ZERO;
        for i in 0..c.len() {
            n += c[i].cross(c[(i + 1) % c.len()]);
        }
        (c, n.normalize())
    }

    #[test]
    fn planes_are_kept_once_and_turned_around_share_an_entry() {
        let mut planes = Planes::new();
        let a = id(&mut planes, [0, 0, 2], 4);
        let b = id(&mut planes, [0, 0, 1], 2);
        let c = id(&mut planes, [0, 0, -1], -2);
        assert_eq!(a, b);
        assert_eq!(c, a.flip());
        assert_eq!(planes.len(), 7);
        assert!(!a.is_bound());
        assert!(Planes::bound(2, false).is_bound());
    }

    #[test]
    fn a_square_faces_its_plane_whichever_way_it_faces() {
        let mut planes = Planes::new();
        for n in [
            [0, 0, 1],
            [0, 0, -1],
            [1, 0, 0],
            [-1, 0, 0],
            [0, 1, 0],
            [0, -1, 0],
            [3, -4, 12],
        ] {
            let on = id(&mut planes, n, 0);
            let square = Polygon::square(on, &planes).unwrap_or_else(|| panic!());
            let (_, normal) = corners_and_normal(&square);
            let want = planes.get(on).normal();
            assert!(normal.dot(want) > 0.999, "{n:?}: {normal} against {want}");
            for c in &square.corners {
                assert_eq!(c.side(planes.get(on)), Ordering::Equal);
            }
        }
    }

    #[test]
    fn clipping_keeps_the_part_behind_with_exact_corners() {
        let mut planes = Planes::new();
        let on = id(&mut planes, [0, 0, 1], 0);
        let mut sides = Vec::new();
        let mut p = Polygon::square(on, &planes).unwrap_or_else(|| panic!());
        // A unit square from four clips.
        for (n, d) in [
            ([1, 0, 0], 1),
            ([-1, 0, 0], 0),
            ([0, 1, 0], 1),
            ([0, -1, 0], 0),
        ] {
            let s = id(&mut planes, n, d);
            match p.clip(s, &planes, &mut sides) {
                Clipped::Part(q) => p = q,
                other => panic!("{other:?}"),
            }
        }
        let (c, normal) = corners_and_normal(&p);
        assert_eq!(c.len(), 4);
        assert!(normal.dot(DVec3::Z) > 0.999);
        for want in [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]] {
            assert!(c.iter().any(|x| x.x == want[0] && x.y == want[1]), "{c:?}");
        }
        // Through a corner: the corner is cut off; along an edge's plane: whole or gone.
        let diagonal = id(&mut planes, [1, 1, 0], 1);
        let Clipped::Part(tri) = p.clip(diagonal, &planes, &mut sides) else {
            panic!()
        };
        assert_eq!(tri.corners.len(), 3);
        let edge = id(&mut planes, [1, 0, 0], 1);
        assert_eq!(p.clip(edge, &planes, &mut sides), Clipped::Whole);
        assert_eq!(p.clip(edge.flip(), &planes, &mut sides), Clipped::Gone);
        // A corner on the plane, the rest behind: whole.
        let touch = id(&mut planes, [1, 1, 0], 2);
        assert_eq!(p.clip(touch, &planes, &mut sides), Clipped::Whole);
    }

    #[test]
    fn a_split_gives_both_parts_and_reversing_turns_a_polygon_over() {
        let mut planes = Planes::new();
        let on = id(&mut planes, [0, 0, 1], 0);
        let mut sides = Vec::new();
        let mut p = Polygon::square(on, &planes).unwrap_or_else(|| panic!());
        for (n, d) in [
            ([1, 0, 0], 4),
            ([-1, 0, 0], 0),
            ([0, 1, 0], 2),
            ([0, -1, 0], 0),
        ] {
            let s = id(&mut planes, n, d);
            if let Clipped::Part(q) = p.clip(s, &planes, &mut sides) {
                p = q;
            }
        }
        let half = id(&mut planes, [1, 0, 0], 1);
        let Split::Both(behind, front) = p.split(half, &planes, &mut sides) else {
            panic!()
        };
        let area = |q: &Polygon| {
            let (c, _) = corners_and_normal(q);
            let mut a = 0.0;
            for i in 0..c.len() {
                a += c[i].cross(c[(i + 1) % c.len()]).z;
            }
            a / 2.0
        };
        assert_eq!(area(&behind), 2.0);
        assert_eq!(area(&front), 6.0);
        let r = front.reversed();
        assert_eq!(r.on, on.flip());
        assert_eq!(area(&r), -6.0);
        // Its corners still meet its edges.
        let m = r.edges.len();
        for k in 0..m {
            let want = Point::meet(
                planes.get(r.on),
                planes.get(r.edges[(k + m - 1) % m]),
                planes.get(r.edges[k]),
            )
            .unwrap_or_else(|| panic!());
            assert!(want.same(&r.corners[k]));
        }
    }
}
