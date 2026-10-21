//! A brush group kept evaluated through edits (ADR 0110). An edit redoes the additive
//! brushes it touches and the surface around them, nothing else:
//!
//! - each additive brush keeps its pieces: its parts cut and clipped by the brushes after
//!   it that act on them, a room's by every room's inside too;
//! - each piece's face keeps what of it is visible, in tiles when the face is large (a
//!   ground under a level), so only the tiles around an edit are clipped again;
//! - a grid of cells finds the pieces near a place.

use crate::csg::{Bounds, Brush, Op, brush_solids, inside_solids, remove, subtract_all};
use crate::exact::Plane;
use crate::polygon::{Planes, Polygon, Split};
use crate::solid::{Owner, Solid, SolidError};
use std::collections::HashMap;

/// A cell's side, in grid units (4.096 m at 1 mm).
const CELL: i64 = 1 << 12;

/// Faces larger than this along an axis are kept in tiles of this size (16.384 m at 1 mm).
const TILE: i64 = 1 << 14;

/// A piece over more cells than this is kept apart (a ground under a whole level).
const MAX_CELLS: i64 = 4096;

/// A piece's place: its brush, and its place among that brush's pieces. Of two faces on
/// one plane facing the same way, the earlier place's stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct PieceRef {
    brush: u32,
    piece: u32,
}

#[derive(Debug, Clone)]
struct Tile {
    polygon: Polygon,
    bounds: Bounds,
    visible: Vec<Polygon>,
}

#[derive(Debug, Clone)]
struct FaceTiles {
    owner: Owner,
    tiles: Vec<Tile>,
}

#[derive(Debug, Clone)]
struct PieceState {
    solid: Solid,
    faces: Vec<FaceTiles>,
}

#[derive(Debug, Clone, Default)]
struct BrushState {
    solids: Vec<Solid>,
    bounds: Option<Bounds>,
    /// A room's inside.
    inside: Vec<Solid>,
    inside_bounds: Option<Bounds>,
    errors: Vec<SolidError>,
    pieces: Vec<PieceState>,
}

/// A brush group, evaluated, kept so through edits.
#[derive(Debug, Clone, Default)]
pub struct Group {
    planes: Planes,
    brushes: Vec<Brush>,
    state: Vec<BrushState>,
    cells: HashMap<[i32; 3], Vec<PieceRef>>,
    /// Pieces over more than [`MAX_CELLS`] cells, looked at by every search.
    large: Vec<PieceRef>,
    /// Brushes whose polygons may have changed since [`Group::take_changed`].
    changed: Vec<bool>,
}

/// What an edit redid, to measure it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Redone {
    /// Additive brushes whose pieces were made again.
    pub brushes: usize,
    /// Tiles clipped again.
    pub tiles: usize,
}

impl Group {
    /// An empty group.
    pub fn new() -> Self {
        Self::default()
    }

    /// The group's planes, which brushes are made of.
    pub fn planes(&self) -> &Planes {
        &self.planes
    }

    /// The group's planes, to add a brush's.
    pub fn planes_mut(&mut self) -> &mut Planes {
        &mut self.planes
    }

    /// Its brushes, in order.
    pub fn brushes(&self) -> &[Brush] {
        &self.brushes
    }

    /// Why brush `index`'s parts make no solid, part by part (empty: they all do).
    pub fn errors(&self, index: usize) -> &[SolidError] {
        &self.state[index].errors
    }

    /// Adds a brush after the others.
    pub fn push(&mut self, brush: Brush) -> Redone {
        let index = self.brushes.len();
        self.brushes.push(brush);
        self.state.push(BrushState::default());
        self.changed.push(true);
        self.solids(index);
        let s = &self.state[index];
        let mut dirty = self.dirty(index, self.brushes[index].op, s.bounds, s.inside_bounds);
        dirty.sort_unstable();
        dirty.dedup();
        self.redo(&dirty)
    }

    /// Replaces brush `index`.
    pub fn set(&mut self, index: usize, brush: Brush) -> Redone {
        let old = &self.state[index];
        let (old_bounds, old_inside) = (old.bounds, old.inside_bounds);
        let old_op = self.brushes[index].op;
        self.brushes[index] = brush;
        self.changed[index] = true;
        self.solids(index);
        let mut dirty = self.dirty(index, old_op, old_bounds, old_inside);
        let s = &self.state[index];
        dirty.extend(self.dirty(index, self.brushes[index].op, s.bounds, s.inside_bounds));
        dirty.sort_unstable();
        dirty.dedup();
        self.redo(&dirty)
    }

    /// Takes brush `index` out (the brushes after it move up: everything is redone).
    pub fn remove(&mut self, index: usize) -> Brush {
        let brush = self.brushes.remove(index);
        let brushes = core::mem::take(&mut self.brushes);
        self.rebuild(brushes);
        brush
    }

    /// Puts `brush` at `index` (the brushes after it move down: everything is redone).
    pub fn insert(&mut self, index: usize, brush: Brush) {
        let mut brushes = core::mem::take(&mut self.brushes);
        brushes.insert(index, brush);
        self.rebuild(brushes);
    }

    /// Evaluates `brushes` from nothing.
    pub fn rebuild(&mut self, brushes: Vec<Brush>) {
        engine_profiler::zone!("brush::Group::rebuild");
        self.cells.clear();
        self.large.clear();
        self.state = vec![BrushState::default(); brushes.len()];
        self.changed = vec![true; brushes.len()];
        self.brushes = brushes;
        for i in 0..self.brushes.len() {
            self.solids(i);
        }
        let adds: Vec<usize> = (0..self.brushes.len())
            .filter(|&i| self.brushes[i].op == Op::Add)
            .collect();
        self.redo(&adds);
    }

    /// The brushes whose polygons may have changed since the last call, and forgets them.
    pub fn take_changed(&mut self) -> Vec<u32> {
        let out = (0..self.changed.len() as u32)
            .filter(|&i| self.changed[i as usize])
            .collect();
        self.changed.iter_mut().for_each(|c| *c = false);
        out
    }

    /// The visible polygons on brush `index`'s faces, each with the face.
    pub fn polygons_of(&self, index: usize, out: &mut Vec<(Owner, Polygon)>) {
        out.clear();
        let Some(bounds) = self.state.get(index).and_then(|s| s.bounds) else {
            return;
        };
        let mut near = Vec::new();
        self.near(&bounds, &mut near);
        for r in near {
            for face in &self.piece(r).faces {
                if face.owner.brush as usize == index {
                    for t in &face.tiles {
                        out.extend(t.visible.iter().map(|p| (face.owner, p.clone())));
                    }
                }
            }
        }
    }

    /// Calls `f` with every visible polygon and the brush face it lies on.
    pub fn for_each_polygon(&self, mut f: impl FnMut(Owner, &Polygon)) {
        for s in &self.state {
            for p in &s.pieces {
                for face in &p.faces {
                    for t in &face.tiles {
                        for v in &t.visible {
                            f(face.owner, v);
                        }
                    }
                }
            }
        }
    }

    /// The pieces of the group's solid, each with its brush.
    pub fn pieces(&self) -> impl Iterator<Item = (u32, &Solid)> {
        self.state
            .iter()
            .enumerate()
            .flat_map(|(b, s)| s.pieces.iter().map(move |p| (b as u32, &p.solid)))
    }

    fn piece(&self, r: PieceRef) -> &PieceState {
        &self.state[r.brush as usize].pieces[r.piece as usize]
    }

    /// Makes brush `index`'s solids and bounds, and a room's inside.
    fn solids(&mut self, index: usize) {
        let mut invalid = Vec::new();
        let brush = &self.brushes[index];
        let solids = brush_solids(index as u32, brush, &self.planes, &mut invalid);
        let inside = inside_solids(index as u32, brush, &self.planes, &mut invalid);
        let s = &mut self.state[index];
        s.bounds = Bounds::of(&solids);
        s.solids = solids;
        s.inside_bounds = Bounds::of(&inside);
        s.inside = inside;
        s.errors = invalid.into_iter().map(|(_, e)| e).collect();
    }

    /// The additive brushes whose pieces brush `index` acts on, doing `op` within
    /// `bounds`, its inside (a room's) within `inside`: the rooms that inside touches.
    fn dirty(
        &self,
        index: usize,
        op: Op,
        bounds: Option<Bounds>,
        inside: Option<Bounds>,
    ) -> Vec<usize> {
        let mut out = self.acted_on(index, op, bounds);
        if op == Op::Add
            && let Some(b) = inside
        {
            out.extend((0..self.brushes.len()).filter(|&r| {
                r != index
                    && self.brushes[r].is_room()
                    && self.state[r].bounds.is_some_and(|x| x.touches(&b))
            }));
        }
        out
    }

    /// The additive brushes whose pieces brush `index` acts on, doing `op` within `bounds`.
    fn acted_on(&self, index: usize, op: Op, bounds: Option<Bounds>) -> Vec<usize> {
        match op {
            Op::Add => vec![index],
            Op::Cut => {
                let Some(b) = bounds else {
                    return Vec::new();
                };
                (0..index)
                    .filter(|&a| {
                        self.brushes[a].op == Op::Add
                            && self.state[a].bounds.is_some_and(|x| x.touches(&b))
                    })
                    .collect()
            }
            Op::Intersect => (0..index)
                .filter(|&a| self.brushes[a].op == Op::Add)
                .collect(),
        }
    }

    /// Makes the pieces of additive brushes `dirty` again, then clips again the faces
    /// they and their old pieces touch.
    fn redo(&mut self, dirty: &[usize]) -> Redone {
        engine_profiler::zone!("brush::Group::redo");
        let mut redone = Redone::default();
        let mut region: Vec<Bounds> = Vec::new();
        let mut sides = Vec::new();
        for &a in dirty {
            redone.brushes += 1;
            // The old pieces go, and what they showed changes.
            let old = core::mem::take(&mut self.state[a].pieces);
            for (k, p) in old.iter().enumerate() {
                region.push(solid_bounds(&p.solid));
                self.unregister(
                    PieceRef {
                        brush: a as u32,
                        piece: k as u32,
                    },
                    &p.solid,
                );
                self.mark(p);
            }
            if self.brushes[a].op != Op::Add {
                continue;
            }
            let pieces = self.pieces_of(a, &mut sides);
            for (k, solid) in pieces.iter().enumerate() {
                region.push(solid_bounds(solid));
                self.register(
                    PieceRef {
                        brush: a as u32,
                        piece: k as u32,
                    },
                    solid,
                );
            }
            self.state[a].pieces = pieces
                .into_iter()
                .map(|solid| PieceState {
                    faces: Vec::new(),
                    solid,
                })
                .collect();
        }
        // The new pieces' faces, in tiles.
        for &a in dirty {
            for k in 0..self.state[a].pieces.len() {
                let faces: Vec<FaceTiles> = self.state[a].pieces[k]
                    .solid
                    .faces
                    .iter()
                    .map(|f| FaceTiles {
                        owner: f.owner,
                        tiles: tiles(&f.polygon, &mut self.planes)
                            .into_iter()
                            .map(|polygon| Tile {
                                bounds: Bounds::polygon(&polygon),
                                polygon,
                                visible: Vec::new(),
                            })
                            .collect(),
                    })
                    .collect();
                self.state[a].pieces[k].faces = faces;
            }
        }
        // Every tile the region touches is clipped again: the new pieces' all, the others'
        // around them.
        let mut todo: Vec<(PieceRef, usize, usize)> = Vec::new();
        let mut near = Vec::new();
        for b in &region {
            self.near(b, &mut near);
            for &r in &near {
                for (f, face) in self.piece(r).faces.iter().enumerate() {
                    for (t, tile) in face.tiles.iter().enumerate() {
                        if tile.bounds.touches(b)
                            || dirty.binary_search(&(r.brush as usize)).is_ok()
                        {
                            todo.push((r, f, t));
                        }
                    }
                }
            }
        }
        todo.sort_unstable();
        todo.dedup();
        let mut candidates = Vec::new();
        for &(r, f, t) in &todo {
            redone.tiles += 1;
            let tile = &self.piece(r).faces[f].tiles[t];
            self.near(&tile.bounds, &mut candidates);
            candidates.retain(|&c| c != r);
            let mut visible = Vec::new();
            clip_tile(
                self,
                r,
                &tile.polygon,
                &candidates,
                &mut sides,
                &mut visible,
            );
            let face = &self.piece(r).faces[f];
            let owner = face.owner.brush as usize;
            self.changed[owner] = true;
            let tile = &mut self.state[r.brush as usize].pieces[r.piece as usize].faces[f].tiles[t];
            tile.visible = visible;
        }
        redone
    }

    /// Additive brush `a`'s pieces: its parts cut and clipped by the brushes after it.
    fn pieces_of(&self, a: usize, sides: &mut Vec<core::cmp::Ordering>) -> Vec<Solid> {
        let Some(own) = self.state[a].bounds else {
            return Vec::new();
        };
        let mut current = self.state[a].solids.clone();
        let mut next = Vec::new();
        // A room: every room's inside first, wherever it is in the order.
        if self.brushes[a].is_room() {
            for r in 0..self.brushes.len() {
                let s = &self.state[r];
                if self.brushes[r].is_room() && s.inside_bounds.is_some_and(|b| b.touches(&own)) {
                    subtract_all(&mut current, &s.inside, &self.planes, sides, &mut next);
                }
            }
        }
        for j in a + 1..self.brushes.len() {
            let op = self.brushes[j].op;
            if op == Op::Add {
                continue;
            }
            let touches = self.state[j].bounds.is_some_and(|b| b.touches(&own));
            match op {
                Op::Cut if touches => {
                    let cuts = &self.state[j].solids;
                    subtract_all(&mut current, cuts, &self.planes, sides, &mut next);
                }
                Op::Intersect if !touches => current.clear(),
                Op::Intersect => {
                    next.clear();
                    for p in current.drain(..) {
                        next.extend(
                            self.state[j]
                                .solids
                                .iter()
                                .filter_map(|k| p.intersect(k, &self.planes, sides)),
                        );
                    }
                    core::mem::swap(&mut current, &mut next);
                }
                _ => {}
            }
            if current.is_empty() {
                break;
            }
        }
        current
    }

    /// The brushes whose polygons `p` showed change.
    fn mark(&mut self, p: &PieceState) {
        for f in &p.faces {
            for t in &f.tiles {
                if !t.visible.is_empty() {
                    self.changed[f.owner.brush as usize] = true;
                }
            }
        }
    }

    fn cell_range(b: &Bounds) -> ([i32; 3], [i32; 3], i64) {
        let lo = b.lo.map(|v| v.div_euclid(CELL) as i32);
        let hi = b.hi.map(|v| v.div_euclid(CELL) as i32);
        let count = (0..3).map(|i| i64::from(hi[i] - lo[i]) + 1).product();
        (lo, hi, count)
    }

    fn cells_of(lo: [i32; 3], hi: [i32; 3]) -> impl Iterator<Item = [i32; 3]> {
        (lo[0]..=hi[0]).flat_map(move |x| {
            (lo[1]..=hi[1]).flat_map(move |y| (lo[2]..=hi[2]).map(move |z| [x, y, z]))
        })
    }

    fn register(&mut self, r: PieceRef, s: &Solid) {
        let (lo, hi, count) = Self::cell_range(&solid_bounds(s));
        if count > MAX_CELLS {
            self.large.push(r);
            return;
        }
        for c in Self::cells_of(lo, hi) {
            self.cells.entry(c).or_default().push(r);
        }
    }

    fn unregister(&mut self, r: PieceRef, s: &Solid) {
        let (lo, hi, count) = Self::cell_range(&solid_bounds(s));
        if count > MAX_CELLS {
            self.large.retain(|&x| x != r);
            return;
        }
        for c in Self::cells_of(lo, hi) {
            if let Some(list) = self.cells.get_mut(&c) {
                list.retain(|&x| x != r);
                if list.is_empty() {
                    self.cells.remove(&c);
                }
            }
        }
    }

    /// The pieces whose bounds touch `b`, in order.
    fn near(&self, b: &Bounds, out: &mut Vec<PieceRef>) {
        out.clear();
        let touching = |r: &PieceRef| b.touches_solid(&self.piece(*r).solid);
        let (lo, hi, count) = Self::cell_range(b);
        if count > self.cells.len() as i64 {
            for list in self.cells.values() {
                out.extend(list.iter().copied().filter(touching));
            }
        } else {
            for c in Self::cells_of(lo, hi) {
                if let Some(list) = self.cells.get(&c) {
                    out.extend(list.iter().copied().filter(touching));
                }
            }
        }
        out.extend(self.large.iter().copied().filter(touching));
        out.sort_unstable();
        out.dedup();
    }
}

fn solid_bounds(s: &Solid) -> Bounds {
    Bounds { lo: s.lo, hi: s.hi }
}

/// A face's tiles: split along every multiple of [`TILE`] across the axes it spans more
/// than a tile along.
fn tiles(polygon: &Polygon, planes: &mut Planes) -> Vec<Polygon> {
    let b = Bounds::polygon(polygon);
    let mut parts = vec![polygon.clone()];
    let mut sides = Vec::new();
    for axis in 0..3 {
        if b.hi[axis] - b.lo[axis] <= TILE {
            continue;
        }
        let first = b.lo[axis].div_euclid(TILE) + 1;
        let last = b.hi[axis].div_euclid(TILE);
        for k in first..=last {
            let mut n = [0; 3];
            n[axis] = 1;
            let Some(plane) = Plane::new(n, k * TILE) else {
                continue;
            };
            let s = planes.add(plane);
            let mut next = Vec::with_capacity(parts.len() + 1);
            for p in parts.drain(..) {
                match p.split(s, planes, &mut sides) {
                    Split::Both(behind, front) => {
                        next.push(behind);
                        next.push(front);
                    }
                    _ => next.push(p),
                }
            }
            parts = next;
        }
    }
    parts
}

/// What of `polygon`, a tile of piece `own`'s face, the `candidates` do not cover.
fn clip_tile(
    group: &Group,
    own: PieceRef,
    polygon: &Polygon,
    candidates: &[PieceRef],
    sides: &mut Vec<core::cmp::Ordering>,
    out: &mut Vec<Polygon>,
) {
    let mut stack: Vec<(Polygon, usize, usize)> = vec![(polygon.clone(), 0, candidates.len())];
    let mut list: Vec<PieceRef> = candidates.to_vec();
    let mut parts = Vec::new();
    while let Some((poly, from, to)) = stack.pop() {
        let b = Bounds::polygon(&poly);
        let mut kept = true;
        for k in from..to {
            let r = list[k];
            let y = &group.piece(r).solid;
            if !b.touches_solid(y) {
                continue;
            }
            parts.clear();
            if remove(&poly, y, r < own, &group.planes, sides, &mut parts) {
                for part in parts.drain(..) {
                    let pb = Bounds::polygon(&part);
                    let start = list.len();
                    for m in k + 1..to {
                        let c = list[m];
                        if pb.touches_solid(&group.piece(c).solid) {
                            list.push(c);
                        }
                    }
                    stack.push((part, start, list.len()));
                }
                kept = false;
                break;
            }
        }
        if kept {
            out.push(poly);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::evaluate;
    use crate::shape::box_planes;
    use engine_core::math::DVec3;
    use std::collections::BTreeMap;

    fn cuboid(op: Op, lo: [i64; 3], hi: [i64; 3], planes: &mut Planes) -> Brush {
        Brush {
            op,
            parts: vec![box_planes(lo, hi, planes).unwrap_or_else(|| panic!())],
            inside: Vec::new(),
        }
    }

    /// Volume and area of polygons, and the area on each brush's faces.
    fn measure<'a>(
        polygons: impl Iterator<Item = (Owner, &'a Polygon)>,
    ) -> (i64, i64, BTreeMap<u32, i64>) {
        let (mut volume, mut area) = (0.0, 0.0);
        let mut by_brush = BTreeMap::new();
        for (o, p) in polygons {
            let c: Vec<DVec3> = p.corners.iter().map(|c| c.to_f64()).collect();
            let mut n = DVec3::ZERO;
            for i in 1..c.len() - 1 {
                volume += c[0].dot(c[i].cross(c[i + 1])) / 6.0;
                n += (c[i] - c[0]).cross(c[i + 1] - c[0]);
            }
            area += n.length() / 2.0;
            *by_brush.entry(o.brush).or_insert(0.0) += n.length() / 2.0;
        }
        let round = |x: f64| (x * 1e3).round() as i64;
        (
            round(volume),
            round(area),
            by_brush.into_iter().map(|(b, a)| (b, round(a))).collect(),
        )
    }

    fn of_group(g: &Group) -> (i64, i64, BTreeMap<u32, i64>) {
        let mut all = Vec::new();
        g.for_each_polygon(|o, p| all.push((o, p.clone())));
        measure(all.iter().map(|(o, p)| (*o, p)))
    }

    /// Rooms on a ground 60 m across (larger than a tile), as the bench has them.
    fn rooms(n: i64, planes: &mut Planes) -> Vec<Brush> {
        let mm = |v: f64| (v * 1000.0) as i64;
        let mut out = vec![cuboid(
            Op::Add,
            [-1000, -500, -1000],
            [mm(12.0 * n as f64 + 1.0), 0, mm(10.0 * n as f64 + 1.0)],
            planes,
        )];
        for i in 0..n {
            for j in 0..n {
                let (x, z) = (12.0 * i as f64, 10.0 * j as f64);
                let b = |lo: [f64; 3], hi: [f64; 3]| (lo.map(mm), hi.map(mm));
                for (op, (lo, hi)) in [
                    (Op::Add, b([x, 0.0, z], [x + 10.4, 3.2, z + 8.4])),
                    (
                        Op::Cut,
                        b([x + 0.2, 0.2, z + 0.2], [x + 10.2, 3.0, z + 8.2]),
                    ),
                    (
                        Op::Cut,
                        b([x + 10.1, 0.2, z + 3.7], [x + 10.5, 2.3, z + 4.7]),
                    ),
                    (Op::Add, b([x + 5.0, 0.2, z + 4.0], [x + 5.4, 3.0, z + 4.4])),
                ] {
                    out.push(cuboid(op, lo, hi, planes));
                }
            }
        }
        out
    }

    /// A room as `csg`'s tests make it: its inside from `lo` to `hi`, walls `wall` thick.
    fn room(lo: [i64; 3], hi: [i64; 3], wall: i64, planes: &mut Planes) -> Brush {
        let inside = crate::shape::cuboid(lo, hi).unwrap_or_else(|| panic!());
        let walls =
            crate::shape::grown(std::slice::from_ref(&inside), wall).unwrap_or_else(|| panic!());
        Brush {
            op: Op::Add,
            parts: walls
                .into_iter()
                .map(|p| p.into_iter().map(|q| planes.add(q)).collect())
                .collect(),
            inside: vec![inside.into_iter().map(|q| planes.add(q)).collect()],
        }
    }

    #[test]
    fn rooms_moved_through_each_other_end_where_a_rebuild_does() {
        let mut g = Group::new();
        // A row of rooms 10 m apart on a ground, a crate in the first.
        let mut brushes = vec![cuboid(
            Op::Add,
            [-5000, -500, -5000],
            [60000, 0, 15000],
            g.planes_mut(),
        )];
        for i in 0..5 {
            let x = 10_000 * i;
            brushes.push(room([x, 0, 0], [x + 8000, 3000, 8000], 200, g.planes_mut()));
        }
        brushes.push(cuboid(
            Op::Add,
            [1000, 0, 1000],
            [2000, 1000, 2000],
            g.planes_mut(),
        ));
        g.rebuild(brushes);
        let check = |g: &Group, step: &str| {
            let mut fresh = Group::new();
            fresh.planes = g.planes().clone();
            fresh.rebuild(g.brushes().to_vec());
            assert_eq!(of_group(g), of_group(&fresh), "{step}");
        };
        check(&g, "built");
        // A corridor drawn from the second room through the third's wall, step by step.
        for (k, reach) in [9000, 11_000, 13_000, 15_000].into_iter().enumerate() {
            let corridor = room(
                [16_000, 0, 3000],
                [16_000 + reach, 2500, 5000],
                200,
                g.planes_mut(),
            );
            if k == 0 {
                g.push(corridor);
            } else {
                let last = g.brushes().len() - 1;
                let redone = g.set(last, corridor);
                assert!(
                    redone.brushes <= 4,
                    "the corridor and the rooms it reaches: {redone:?}"
                );
            }
            check(&g, &format!("reach {reach}"));
        }
        // The third room made solid, then a room again.
        let third = g.brushes()[3].clone();
        let solid = Brush {
            inside: Vec::new(),
            ..third.clone()
        };
        g.set(3, solid);
        check(&g, "solid");
        g.set(3, third);
        check(&g, "a room again");
    }

    #[test]
    fn a_group_shows_what_evaluating_from_nothing_does() {
        let mut g = Group::new();
        let brushes = rooms(5, g.planes_mut());
        let e = evaluate(&brushes, g.planes());
        g.rebuild(brushes);
        let fresh = measure(e.surface.iter().map(|(o, p)| (*o, p)));
        assert_eq!(of_group(&g), fresh);
        // The ground was tiled, so it has more polygons than from nothing.
        let mut n = 0;
        g.for_each_polygon(|_, _| n += 1);
        assert!(n > e.surface.len(), "{n} against {}", e.surface.len());
        assert_eq!(g.pieces().count(), e.pieces.len());
    }

    #[test]
    fn edits_redo_only_what_they_touch_and_end_where_a_rebuild_does() {
        let mut g = Group::new();
        let brushes = rooms(4, g.planes_mut());
        g.rebuild(brushes);
        let _ = g.take_changed();
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |m: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % m
        };
        for step in 0..40 {
            let count = g.brushes().len();
            let index = 1 + next(count as u64 - 1) as usize;
            let dx = next(3001) as i64 - 1500;
            let dz = next(3001) as i64 - 1500;
            // Move brush `index` by (dx, 0, dz), sometimes turning an add into a cut.
            let planes: Vec<Plane> = g.brushes()[index].parts[0]
                .iter()
                .map(|&id| *g.planes().get(id))
                .collect();
            let moved: Vec<_> = planes
                .iter()
                .map(|p| {
                    let d = p.d + i64::from(p.n[0]) * dx + i64::from(p.n[2]) * dz;
                    let q = Plane::new(p.n.map(i64::from), d).unwrap_or_else(|| panic!());
                    g.planes_mut().add(q)
                })
                .collect();
            let mut op = g.brushes()[index].op;
            if step % 7 == 3 {
                op = if op == Op::Add { Op::Cut } else { Op::Add };
            }
            let redone = g.set(
                index,
                Brush {
                    op,
                    parts: vec![moved],
                    inside: Vec::new(),
                },
            );
            assert!(g.take_changed().contains(&(index as u32)));
            // Local: far fewer additive brushes redone than there are.
            assert!(redone.brushes < count / 4, "step {step}: {redone:?}");
            let mut fresh = Group::new();
            fresh.planes = g.planes().clone();
            fresh.rebuild(g.brushes().to_vec());
            assert_eq!(of_group(&g), of_group(&fresh), "step {step}");
        }
        // Brushes taken out and put back in the middle.
        let b = g.remove(3);
        g.insert(3, b);
        let mut fresh = Group::new();
        fresh.planes = g.planes().clone();
        fresh.rebuild(g.brushes().to_vec());
        assert_eq!(of_group(&g), of_group(&fresh));
    }
}
