//! A brush group's CSG (ADR 0110): Godot's order, each brush acting on what the brushes
//! before it made, evaluated over convex pieces; then the surface left, each polygon owned
//! by the brush face whose plane it lies on.

use crate::polygon::{PlaneId, Planes, Polygon, Split};
use crate::solid::{Owner, Solid, SolidError};
use core::cmp::Ordering;

/// What a brush does to what the brushes before it in its group made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Op {
    /// Adds itself.
    #[default]
    Add,
    /// Takes itself out.
    Cut,
    /// Keeps only what lies inside it.
    Intersect,
}

/// A brush as the CSG sees it: what it does, and its convex parts, each the planes that
/// bound it in the group's grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brush {
    /// What it does.
    pub op: Op,
    /// Its convex parts (stairs have one per step). Its faces are numbered through them,
    /// in order.
    pub parts: Vec<Vec<PlaneId>>,
}

/// A convex piece of the group's solid: what is left of an additive brush's part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// The brush it is left of.
    pub brush: u32,
    /// Its solid.
    pub solid: Solid,
}

/// A brush group evaluated.
#[derive(Debug, Clone, Default)]
pub struct Evaluated {
    /// The solid as convex pieces (they may overlap where additive brushes do).
    pub pieces: Vec<Piece>,
    /// Its surface: convex polygons facing out, each with the brush face it lies on.
    pub surface: Vec<(Owner, Polygon)>,
    /// The brushes whose planes make no closed solid, by their place in the group.
    pub invalid: Vec<(u32, SolidError)>,
}

/// Evaluates `brushes` in order.
///
/// Each additive brush's pieces are its parts cut and clipped by the brushes after it that
/// act on them: the cuts whose bounds touch its own, and every intersect (one that misses
/// it leaves nothing of it). So an edit redoes only the additive brushes it touches.
pub fn evaluate(brushes: &[Brush], table: &Planes) -> Evaluated {
    engine_profiler::zone!("brush::evaluate");
    let mut out = Evaluated::default();
    let solids: Vec<Vec<Solid>> = brushes
        .iter()
        .enumerate()
        .map(|(b, brush)| brush_solids(b as u32, brush, table, &mut out.invalid))
        .collect();
    let bounds: Vec<Option<Bounds>> = solids.iter().map(|s| Bounds::of(s)).collect();
    let near = touching(&bounds);
    let intersects: Vec<u32> = (0..brushes.len() as u32)
        .filter(|&j| brushes[j as usize].op == Op::Intersect)
        .collect();
    let mut sides = Vec::new();
    let mut later = Vec::new();
    for (a, brush) in brushes.iter().enumerate() {
        if brush.op != Op::Add {
            continue;
        }
        let Some(own) = bounds[a] else {
            continue;
        };
        later.clear();
        later.extend(
            near[a]
                .iter()
                .copied()
                .filter(|&j| j as usize > a && brushes[j as usize].op == Op::Cut),
        );
        later.extend(intersects.iter().copied().filter(|&j| j as usize > a));
        later.sort_unstable();
        let mut current = solids[a].clone();
        let mut next = Vec::new();
        for &j in &later {
            let j = j as usize;
            match brushes[j].op {
                Op::Cut => {
                    for cut in &solids[j] {
                        next.clear();
                        for p in current.drain(..) {
                            match p.subtract(cut, table, &mut sides) {
                                None => next.push(p),
                                Some(parts) => next.extend(parts),
                            }
                        }
                        core::mem::swap(&mut current, &mut next);
                    }
                }
                Op::Intersect => {
                    if !bounds[j].is_some_and(|b| b.touches(&own)) {
                        current.clear();
                        break;
                    }
                    next.clear();
                    for p in current.drain(..) {
                        next.extend(
                            solids[j]
                                .iter()
                                .filter_map(|k| p.intersect(k, table, &mut sides)),
                        );
                    }
                    core::mem::swap(&mut current, &mut next);
                }
                Op::Add => {}
            }
            if current.is_empty() {
                break;
            }
        }
        out.pieces.extend(current.into_iter().map(|solid| Piece {
            brush: a as u32,
            solid,
        }));
    }
    out.surface = surface(&out.pieces, table);
    out
}

/// The solids of `brush`'s parts, its faces numbered through them; the parts that make
/// none go to `invalid`.
pub(crate) fn brush_solids(
    b: u32,
    brush: &Brush,
    table: &Planes,
    invalid: &mut Vec<(u32, SolidError)>,
) -> Vec<Solid> {
    let mut face = 0;
    let mut solids = Vec::with_capacity(brush.parts.len());
    for part in &brush.parts {
        let planes: Vec<(PlaneId, Owner)> = part
            .iter()
            .enumerate()
            .map(|(k, &p)| {
                (
                    p,
                    Owner {
                        brush: b,
                        face: face + k as u32,
                    },
                )
            })
            .collect();
        face += part.len() as u32;
        match Solid::from_planes(&planes, table) {
            Ok(s) => solids.push(s),
            Err(e) => invalid.push((b, e)),
        }
    }
    solids
}

/// Bounds in grid units, rounded outwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) lo: [i64; 3],
    pub(crate) hi: [i64; 3],
}

impl Bounds {
    pub(crate) fn of(solids: &[Solid]) -> Option<Self> {
        let first = solids.first()?;
        let mut b = Self {
            lo: first.lo,
            hi: first.hi,
        };
        for s in &solids[1..] {
            for i in 0..3 {
                b.lo[i] = b.lo[i].min(s.lo[i]);
                b.hi[i] = b.hi[i].max(s.hi[i]);
            }
        }
        Some(b)
    }

    pub(crate) fn polygon(p: &Polygon) -> Self {
        let (mut lo, mut hi) = ([i64::MAX; 3], [i64::MIN; 3]);
        for c in &p.corners {
            let v = c.to_f64();
            for (i, x) in [v.x, v.y, v.z].into_iter().enumerate() {
                lo[i] = lo[i].min(x.floor() as i64 - 1);
                hi[i] = hi[i].max(x.ceil() as i64 + 1);
            }
        }
        Self { lo, hi }
    }

    #[inline]
    pub(crate) fn touches(&self, other: &Self) -> bool {
        (0..3).all(|i| self.lo[i] <= other.hi[i] && other.lo[i] <= self.hi[i])
    }

    #[inline]
    pub(crate) fn touches_solid(&self, s: &Solid) -> bool {
        (0..3).all(|i| self.lo[i] <= s.hi[i] && s.lo[i] <= self.hi[i])
    }
}

/// For each of `bounds`, the others that touch it, in order (sort and sweep along x).
fn touching(bounds: &[Option<Bounds>]) -> Vec<Vec<u32>> {
    let mut order: Vec<u32> = (0..bounds.len() as u32)
        .filter(|&i| bounds[i as usize].is_some())
        .collect();
    let lo = |i: u32| bounds[i as usize].map_or(i64::MAX, |b| b.lo[0]);
    order.sort_by_key(|&i| lo(i));
    let mut near = vec![Vec::new(); bounds.len()];
    for (k, &i) in order.iter().enumerate() {
        let Some(a) = bounds[i as usize] else {
            continue;
        };
        for &j in &order[k + 1..] {
            let Some(b) = bounds[j as usize] else {
                continue;
            };
            if b.lo[0] > a.hi[0] {
                break;
            }
            if a.touches(&b) {
                near[i as usize].push(j);
                near[j as usize].push(i);
            }
        }
    }
    for n in &mut near {
        n.sort_unstable();
    }
    near
}

/// The surface of the union of `pieces`: each piece's faces less what other pieces cover.
/// On a plane two pieces share, faces turned towards each other both go, and of two faces
/// facing the same way the earlier piece's stays.
///
/// A face is clipped by the pieces whose bounds touch it, one at a time; each part left
/// goes on with only the remaining pieces that touch it, so a large face (a ground under a
/// level) costs about what its parts do.
pub fn surface(pieces: &[Piece], table: &Planes) -> Vec<(Owner, Polygon)> {
    engine_profiler::zone!("brush::surface");
    let bounds: Vec<Option<Bounds>> = pieces
        .iter()
        .map(|p| {
            Some(Bounds {
                lo: p.solid.lo,
                hi: p.solid.hi,
            })
        })
        .collect();
    let near = touching(&bounds);
    let mut sides = Vec::new();
    let mut out = Vec::new();
    let mut stack: Vec<(Polygon, usize, usize)> = Vec::new();
    let mut candidates: Vec<u32> = Vec::new();
    let mut parts = Vec::new();
    for (i, x) in pieces.iter().enumerate() {
        for face in &x.solid.faces {
            candidates.clear();
            let b = Bounds::polygon(&face.polygon);
            candidates.extend(
                near[i]
                    .iter()
                    .copied()
                    .filter(|&j| b.touches_solid(&pieces[j as usize].solid)),
            );
            stack.push((face.polygon.clone(), 0, candidates.len()));
            while let Some((poly, from, to)) = stack.pop() {
                let b = Bounds::polygon(&poly);
                let mut kept = true;
                for k in from..to {
                    let j = candidates[k] as usize;
                    let y = &pieces[j].solid;
                    if !b.touches_solid(y) {
                        continue;
                    }
                    parts.clear();
                    if remove(&poly, y, j < i, table, &mut sides, &mut parts) {
                        for part in parts.drain(..) {
                            let pb = Bounds::polygon(&part);
                            let start = candidates.len();
                            for m in k + 1..to {
                                let c = candidates[m];
                                if pb.touches_solid(&pieces[c as usize].solid) {
                                    candidates.push(c);
                                }
                            }
                            stack.push((part, start, candidates.len()));
                        }
                        kept = false;
                        break;
                    }
                }
                if kept {
                    out.push((face.owner, poly));
                }
            }
        }
    }
    out
}

/// Pushes onto `out` what of `poly` solid `y` does not cover, and says whether `y`
/// covers any of it (otherwise nothing is pushed). `y_first`: `y` comes first, so of two
/// faces on one plane facing the same way, `y`'s stays.
pub(crate) fn remove(
    poly: &Polygon,
    y: &Solid,
    y_first: bool,
    table: &Planes,
    sides: &mut Vec<Ordering>,
    out: &mut Vec<Polygon>,
) -> bool {
    let f = poly.on;
    let plane = table.get(f);
    // y's bounds wholly on one side of the plane: it neither crosses nor touches it (exact
    // in i64: normals below 2^23 times coordinates below 2^28).
    let (mut lowest, mut highest) = (0i64, 0i64);
    for k in 0..3 {
        let n = i64::from(plane.n[k]);
        let (a, b) = (n * y.lo[k], n * y.hi[k]);
        lowest += a.min(b);
        highest += a.max(b);
    }
    if lowest > plane.d || highest < plane.d {
        return false;
    }
    // Does y reach across the polygon's plane, or touch it with a face?
    let (mut behind, mut front) = (false, false);
    for c in y.corners() {
        match c.side(plane) {
            Ordering::Less => behind = true,
            Ordering::Greater => front = true,
            Ordering::Equal => {}
        }
        if behind && front {
            break;
        }
    }
    if !(behind && front) {
        let same = y.faces.iter().any(|g| g.polygon.on == f);
        let opposite = y.faces.iter().any(|g| g.polygon.on == f.flip());
        // Touching along an edge or a corner covers nothing; y behind the plane with a
        // face on it covers the polygon only if y's face stays.
        if !(opposite || same && y_first) {
            return false;
        }
    }
    let start = out.len();
    let mut inside: Option<Polygon> = None;
    for g in &y.faces {
        let s = g.polygon.on;
        if s == f || s == f.flip() {
            continue;
        }
        let current = inside.as_ref().unwrap_or(poly);
        match current.split(s, table, sides) {
            Split::Behind => {}
            Split::Front => {
                // They do not overlap.
                out.truncate(start);
                return false;
            }
            Split::Both(b, fr) => {
                out.push(fr);
                inside = Some(b);
            }
        }
    }
    // What is left lies inside y.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exact::Plane;
    use crate::shape::box_planes;
    use engine_core::math::DVec3;

    fn cuboid(op: Op, lo: [i64; 3], hi: [i64; 3], table: &mut Planes) -> Brush {
        Brush {
            op,
            parts: vec![box_planes(lo, hi, table).unwrap_or_else(|| panic!())],
        }
    }

    /// The volume a surface encloses, seen from `origin` (the same from anywhere when the
    /// surface is closed), and its area.
    fn measure(surface: &[(Owner, Polygon)], origin: DVec3) -> (f64, f64) {
        let (mut volume, mut area) = (0.0, 0.0);
        for (_, p) in surface {
            let c: Vec<DVec3> = p.corners.iter().map(|c| c.to_f64() - origin).collect();
            let mut n = DVec3::ZERO;
            for i in 1..c.len() - 1 {
                volume += c[0].dot(c[i].cross(c[i + 1])) / 6.0;
                n += (c[i] - c[0]).cross(c[i + 1] - c[0]);
            }
            area += n.length() / 2.0;
        }
        // The corners are exact; only these sums round.
        let round = |x: f64| (x * 1e6).round() / 1e6;
        (round(volume), round(area))
    }

    /// Evaluates and checks the surface is closed: the volume, the area and the owners'
    /// brushes.
    fn closed(brushes: &[Brush], table: &Planes) -> (f64, f64, Evaluated) {
        let e = evaluate(brushes, table);
        assert!(e.invalid.is_empty(), "{:?}", e.invalid);
        let (v0, a) = measure(&e.surface, DVec3::ZERO);
        let (v1, _) = measure(&e.surface, DVec3::new(1234.5, -987.25, 4321.0));
        assert!(
            (v0 - v1).abs() < 1e-6 * v0.abs().max(1.0),
            "not closed: {v0} against {v1}"
        );
        (v0, a, e)
    }

    #[test]
    fn a_box_is_its_six_faces() {
        let mut t = Planes::new();
        let b = [cuboid(Op::Add, [0, 0, 0], [10, 20, 30], &mut t)];
        let (v, a, e) = closed(&b, &t);
        assert_eq!((v, a), (6000.0, 2200.0));
        assert_eq!((e.pieces.len(), e.surface.len()), (1, 6));
    }

    #[test]
    fn a_cut_through_a_box_leaves_a_hole_whose_walls_are_the_cuts() {
        let mut t = Planes::new();
        let b = [
            cuboid(Op::Add, [0, 0, 0], [10, 10, 10], &mut t),
            cuboid(Op::Cut, [3, 3, -1], [7, 7, 11], &mut t),
        ];
        let (v, a, e) = closed(&b, &t);
        assert_eq!((v, a), (840.0, 728.0));
        // The hole's four walls, 4 × 10 each, belong to the cut.
        let (_, walls) = measure(
            &e.surface
                .iter()
                .filter(|(o, _)| o.brush == 1)
                .cloned()
                .collect::<Vec<_>>(),
            DVec3::ZERO,
        );
        assert_eq!(walls, 160.0);
        // Four slabs, axis planes first.
        assert_eq!(e.pieces.len(), 4);
    }

    #[test]
    fn touching_boxes_lose_the_faces_between_them_and_overlapping_ones_keep_one_of_each() {
        let mut t = Planes::new();
        let touching = [
            cuboid(Op::Add, [0, 0, 0], [10, 10, 10], &mut t),
            cuboid(Op::Add, [10, 0, 0], [20, 10, 10], &mut t),
        ];
        let (v, a, _) = closed(&touching, &t);
        assert_eq!((v, a), (2000.0, 1000.0));
        let overlapping = [
            cuboid(Op::Add, [0, 0, 0], [10, 10, 10], &mut t),
            cuboid(Op::Add, [5, 0, 0], [15, 10, 10], &mut t),
        ];
        let (v, a, e) = closed(&overlapping, &t);
        assert_eq!((v, a), (1500.0, 800.0));
        // Of the faces both have on one plane, the first brush's stays.
        let first: f64 = measure(
            &e.surface
                .iter()
                .filter(|(o, _)| o.brush == 0)
                .cloned()
                .collect::<Vec<_>>(),
            DVec3::ZERO,
        )
        .1;
        assert_eq!(first, 4.0 * 100.0 + 100.0);
    }

    #[test]
    fn each_brush_acts_on_what_the_brushes_before_it_made() {
        let mut t = Planes::new();
        // A box, a cube cut out of it, a smaller cube added inside the hole.
        let b = [
            cuboid(Op::Add, [0, 0, 0], [10, 10, 10], &mut t),
            cuboid(Op::Cut, [2, 2, 2], [8, 8, 8], &mut t),
            cuboid(Op::Add, [4, 4, 4], [6, 6, 6], &mut t),
        ];
        let (v, _, _) = closed(&b, &t);
        assert_eq!(v, 1000.0 - 216.0 + 8.0);
        // Intersecting keeps what lies inside it of everything before it.
        let i = [
            cuboid(Op::Add, [0, 0, 0], [10, 10, 10], &mut t),
            cuboid(Op::Add, [20, 0, 0], [30, 10, 10], &mut t),
            cuboid(Op::Intersect, [5, -5, 0], [15, 5, 10], &mut t),
        ];
        let (v, _, e) = closed(&i, &t);
        assert_eq!(v, 250.0);
        assert_eq!(e.pieces.len(), 1);
        // A cut first cuts nothing.
        let c = [
            cuboid(Op::Cut, [0, 0, 0], [10, 10, 10], &mut t),
            cuboid(Op::Add, [0, 0, 0], [1, 1, 1], &mut t),
        ];
        assert_eq!(closed(&c, &t).0, 1.0);
    }

    #[test]
    fn tilted_cuts_stay_closed_and_exact() {
        let mut t = Planes::new();
        let mut tilted = Vec::new();
        for (n, p) in [
            (DVec3::new(1.0, 0.3, 0.1), [700, 0, 0]),
            (DVec3::new(-1.0, 0.2, -0.05), [300, 0, 0]),
            (DVec3::new(0.1, 1.0, 0.2), [0, 1100, 0]),
            (DVec3::new(0.0, -1.0, 0.1), [0, -100, 0]),
            (DVec3::new(0.05, 0.1, 1.0), [0, 0, 600]),
            (DVec3::new(-0.1, 0.0, -1.0), [0, 0, 400]),
        ] {
            tilted.push(t.add(Plane::through(n, p).unwrap_or_else(|| panic!())));
        }
        let b = [
            cuboid(Op::Add, [0, 0, 0], [1000, 1000, 1000], &mut t),
            Brush {
                op: Op::Cut,
                parts: vec![tilted],
            },
        ];
        let (v, _, e) = closed(&b, &t);
        assert!(v > 0.0 && v < 1e9, "{v}");
        assert!(e.pieces.len() >= 2);
        // The pieces and the cut fill the box: the pieces' volumes add up.
        let pieces: f64 = e
            .pieces
            .iter()
            .map(|p| {
                let s: Vec<(Owner, Polygon)> = p
                    .solid
                    .faces
                    .iter()
                    .map(|f| (f.owner, f.polygon.clone()))
                    .collect();
                measure(&s, DVec3::ZERO).0
            })
            .sum();
        assert!((pieces - v).abs() < 1e-6 * v, "{pieces} against {v}");
    }

    #[test]
    fn open_or_empty_planes_make_no_brush() {
        let mut t = Planes::new();
        let x = t.add(Plane::new([1, 0, 0], 10).unwrap_or_else(|| panic!()));
        let y = t.add(Plane::new([0, 1, 0], 10).unwrap_or_else(|| panic!()));
        let open = Brush {
            op: Op::Add,
            parts: vec![vec![x, y]],
        };
        let e = evaluate(&[open], &t);
        assert_eq!(e.invalid, [(0, SolidError::Open)]);
        let slab = Brush {
            op: Op::Add,
            parts: vec![vec![x, x.flip(), y]],
        };
        assert_eq!(evaluate(&[slab], &t).invalid, [(0, SolidError::Empty)]);
    }
}
