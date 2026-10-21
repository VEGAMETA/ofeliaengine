//! Drawing brushes (ADR 0110 E2b-3; `docs/research/ux/blockout.md` §2): B draws a brush on
//! the surface under the pointer, Shift+B a cut.
//!
//! - **Before the first click** a mark shows where the first corner lands: on the surface
//!   under the pointer (the viewport's surface pick), else on the ground. B again takes the
//!   next shape; Shift+B makes it a cut, and B a brush again.
//! - **The base:** a click sets the first corner, and the base follows the pointer in the
//!   surface's plane. A click keeps it, and so does releasing a press dragged away.
//! - **The height** follows the pointer along the base's normal, either way. A click keeps
//!   it and makes the brush. The brush is there from the first height on, cut and cutting
//!   as it will be, so a doorway opens as it deepens.
//! - **Typed sizes** take the pointer's place: the first side, Tab, the second, Enter; the
//!   height, Enter. Metres and expressions as in keyboard transforms ([`crate::expr`]); a
//!   cut's typed height goes into the surface, a brush's out of it.
//! - **R** turns the shape a quarter about the base's normal: where wedges and stairs rise
//!   (away from the first corner along the second side, until turned).
//! - **Ctrl+H** makes it a room: what is drawn is its inside, its walls [`ROOM_WALLS`]
//!   thick around it, and rooms open into each other (ADR 0110).
//! - **The owner's snapping rules** ([`crate::Snapping`]): with snapping on (G), or Shift
//!   with it off, the first corner lands on the grid of the tool bar's position step in the
//!   surface's plane and sizes go in whole steps from it; Ctrl rounds the corner under the
//!   pointer to the grid. Free, corners and sizes still land on the brushes' millimetre
//!   grid, which their planes are held on.
//! - **Other brushes' corners and edges** come first (team decision, 2026-10-21; SketchUp's
//!   inference): within a few pixels of the pointer, the first corner lands on a corner or
//!   on an edge, the base's other corner on one as seen in the base's plane, and the height
//!   on a corner's height when the pointer is over that corner.
//!
//! Escape or a right click drops it; the brush is one undo step.

use crate::expr;
use crate::gizmo::{ViewProj, hit_plane};
use crate::snap::{Quantity, Snapping};
use crate::transform::short;
use core::fmt::Write as _;
use engine_app::{BrushOp, BrushShape};
use engine_core::math::{Mat3, Quat, Vec2, Vec3};

/// The keys while drawing, for the status bar.
pub(crate) const HELP: &str = "click: the first corner, the base, the height  ·  type a size, \
                               Tab: the other side  ·  Enter: next  ·  B: next shape  ·  R: \
                               turn  ·  Ctrl+H: room  ·  Esc or right click: cancel";

/// A room's walls, metres thick: drawn with Ctrl+H, or a brush made one (a guess of the UX
/// note's, `docs/research/ux/blockout.md` §1).
pub(crate) const ROOM_WALLS: f32 = 0.2;

/// A brush's grid, metres: corners and sizes land on it, and sizes below it are none.
const LEAST: f32 = 0.001;

/// `x` on the millimetre grid (the nearest `f32` to it: 2.1, not 2.1000001).
fn mm(x: f32) -> f32 {
    (x * 1000.0).round() / 1000.0
}

/// Drawn stairs get as many steps as make each about this high, in metres.
const STEP_HEIGHT: f32 = 0.2;

/// A normal this close to an axis (the cosine of 3°) is that axis: the normals of flat
/// faces measured from depths wobble.
const AXIS_COS: f32 = 0.9986;

/// A press dragged this far (physical pixels) and released keeps the base.
const DRAG_PIXELS: f32 = 4.0;

/// What a point landed on: another brush's corner, or a point of its edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Landed {
    Corner(Vec3),
    Edge(Vec3),
}

impl Landed {
    /// Where it landed.
    pub(crate) fn point(self) -> Vec3 {
        match self {
            Self::Corner(p) | Self::Edge(p) => p,
        }
    }
}

/// A shape B draws.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DrawShape {
    /// Its name in hints and the tree ("Box", "Stairs").
    pub(crate) name: &'static str,
    pub(crate) shape: BrushShape,
    /// A cone's top, a share of its base.
    pub(crate) top: f32,
}

const fn shape(name: &'static str, shape: BrushShape, top: f32) -> DrawShape {
    DrawShape { name, shape, top }
}

/// The shapes B goes through, in the add menu's order.
pub(crate) const SHAPES: [DrawShape; 8] = [
    shape("Box", BrushShape::Box, 0.0),
    shape("Wedge", BrushShape::Wedge, 0.0),
    shape("Stairs", BrushShape::Stairs, 0.0),
    shape("Cylinder", BrushShape::Cylinder, 0.0),
    shape("Cone", BrushShape::Cone, 0.0),
    shape("Truncated Cone", BrushShape::Cone, 0.5),
    shape("Sphere", BrushShape::Sphere, 0.0),
    shape("Torus", BrushShape::Torus, 0.0),
];

/// Where a base lies: its first corner, its normal (where a positive height goes), and
/// two directions along it, `u` and `v = u × normal`, so that (u, normal, v) are a
/// brush's (x, y, z).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BasePlane {
    pub(crate) corner: Vec3,
    pub(crate) normal: Vec3,
    pub(crate) u: Vec3,
    pub(crate) v: Vec3,
}

impl BasePlane {
    /// The plane through `corner` across `normal`: on a floor or a ceiling `u` runs along
    /// x; on a wall or a slope `v` runs up it.
    pub(crate) fn new(corner: Vec3, normal: Vec3) -> Self {
        let normal = normal.try_normalize().unwrap_or(Vec3::Y);
        let (u, v) = if normal.y.abs() > 0.9 {
            let u = (Vec3::X - normal * normal.x).normalize();
            (u, u.cross(normal))
        } else {
            let v = (Vec3::Y - normal * normal.y).normalize();
            (normal.cross(v), v)
        };
        Self {
            corner,
            normal,
            u,
            v,
        }
    }

    /// `p`'s place in the plane, from the first corner along `u` and `v`.
    pub(crate) fn coords(&self, p: Vec3) -> Vec2 {
        let d = p - self.corner;
        Vec2::new(d.dot(self.u), d.dot(self.v))
    }

    /// The point at `c` in the plane.
    pub(crate) fn point(&self, c: Vec2) -> Vec3 {
        self.corner + self.u * c.x + self.v * c.y
    }
}

/// Where a drawing is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Stage {
    /// Before the first click: the mark follows the pointer.
    Aim,
    /// The base, from the first corner to `to` in the plane; its first press at `press`.
    Base {
        plane: BasePlane,
        to: Vec2,
        press: Vec2,
    },
    /// The height over the base from the first corner to `to`.
    Height {
        plane: BasePlane,
        to: Vec2,
        height: f32,
    },
}

/// One frame of the pointer, in physical pixels, and what it hit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pointer<'a> {
    pub(crate) view: &'a ViewProj,
    pub(crate) at: Vec2,
    /// The surface under it, the latest the viewport found: its point and normal.
    pub(crate) surface: Option<(Vec3, Vec3)>,
    /// The left button went down this frame, and up.
    pub(crate) press: bool,
    pub(crate) release: bool,
    pub(crate) snap: &'a Snapping,
    /// Shift and Ctrl, held.
    pub(crate) invert: bool,
    pub(crate) round: bool,
    /// Other brushes' corners and edges in the world, and how near the pointer (physical
    /// pixels) takes one.
    pub(crate) corners: &'a [Vec3],
    pub(crate) edges: &'a [(Vec3, Vec3)],
    pub(crate) reach: f32,
}

impl Pointer<'_> {
    /// The corner nearest the pointer on the screen within reach, else the point of an
    /// edge; each mapped by `onto` first (into a plane).
    fn land(&self, onto: impl Fn(Vec3) -> Vec3, edges: bool) -> Option<Landed> {
        let mut best: Option<(f32, Vec3)> = None;
        for &c in self.corners {
            let c = onto(c);
            if let Some(d) = self.view.screen(c).map(|s| s.distance(self.at))
                && d <= self.reach
                && best.is_none_or(|(b, _)| d < b)
            {
                best = Some((d, c));
            }
        }
        if let Some((_, c)) = best {
            return Some(Landed::Corner(c));
        }
        if !edges {
            return None;
        }
        let ray = self.view.ray(self.at);
        for &(a, b) in self.edges {
            let (a, b) = (onto(a), onto(b));
            let p = nearest_on_segment(ray, a, b);
            if let Some(d) = self.view.screen(p).map(|s| s.distance(self.at))
                && d <= self.reach
                && best.is_none_or(|(b, _)| d < b)
            {
                best = Some((d, self.along_grid(p, a, b)));
            }
        }
        best.map(|(_, p)| Landed::Edge(p))
    }

    /// `p` on the edge `a`–`b` with its place along an edge that runs along an axis on the
    /// grid, as the rules say (the millimetres when free), within the edge.
    fn along_grid(&self, mut p: Vec3, a: Vec3, b: Vec3) -> Vec3 {
        let Some(i) = (0..3).find(|&i| (b - a).normalize_or_zero()[i].abs() >= AXIS_COS) else {
            return p;
        };
        let on = if self.round || self.snap.on != self.invert {
            let step = self.snap.step(Quantity::Position) as f32;
            (p[i] / step).round() * step
        } else {
            mm(p[i])
        };
        p[i] = on.clamp(a[i].min(b[i]), a[i].max(b[i]));
        p
    }
}

/// The point of segment `a`–`b` nearest the ray `(origin, direction)`.
fn nearest_on_segment((o, d): (Vec3, Vec3), a: Vec3, b: Vec3) -> Vec3 {
    let e = b - a;
    let (dd, de, ee) = (d.dot(d), d.dot(e), e.dot(e));
    let w = o - a;
    let denominator = dd * ee - de * de;
    let t = if denominator.abs() < 1e-9 || ee < 1e-12 {
        0.0
    } else {
        ((dd * e.dot(w) - de * d.dot(w)) / denominator).clamp(0.0, 1.0)
    };
    a + e * t
}

/// What a drawing makes: the shape and its operation, where its base's middle is and how
/// it turns in the world, its size (x, height, z), for stairs its steps, and a room's
/// walls (0: solid).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Made {
    pub(crate) shape: DrawShape,
    pub(crate) op: BrushOp,
    pub(crate) at: Vec3,
    pub(crate) rotation: Quat,
    pub(crate) size: Vec3,
    pub(crate) steps: Option<u32>,
    pub(crate) hollow: f32,
}

/// A brush being drawn.
#[derive(Debug, Clone)]
pub(crate) struct Draw {
    /// The shape: an index in [`SHAPES`].
    pub(crate) shape: usize,
    pub(crate) op: BrushOp,
    pub(crate) stage: Stage,
    /// Quarter turns about the base's normal (R).
    pub(crate) turns: u8,
    /// A room (Ctrl+H); a cut never is one.
    pub(crate) room: bool,
    /// Where the first corner lands now, and the normal of its surface.
    pub(crate) mark: Option<(Vec3, Vec3)>,
    /// What the point the pointer moves landed on this frame, if another brush's.
    pub(crate) landed: Option<Landed>,
    /// What is typed for the first side, the second and the height.
    typed: [String; 3],
    /// The one being typed: 0 or 1 the base's sides, 2 the height.
    field: usize,
    /// Its undo step.
    pub(crate) merge: u64,
    /// The window lost the keyboard: it is dropped.
    pub(crate) cancel: bool,
}

impl Draw {
    /// A drawing of `SHAPES[shape]` as `op`, made as undo step `merge`.
    pub(crate) fn new(op: BrushOp, shape: usize, merge: u64) -> Self {
        Self {
            shape: shape % SHAPES.len(),
            op,
            stage: Stage::Aim,
            turns: 0,
            room: false,
            mark: None,
            landed: None,
            typed: Default::default(),
            field: 0,
            merge,
            cancel: false,
        }
    }

    /// B (or Shift+B, `op`) again: the other key switches the operation; the same one goes
    /// to the next shape before the first click.
    pub(crate) fn again(&mut self, op: BrushOp) {
        if self.op != op {
            self.op = op;
        } else if self.stage == Stage::Aim {
            self.shape = (self.shape + 1) % SHAPES.len();
        }
    }

    /// R: a quarter turn about the base's normal.
    pub(crate) fn turn(&mut self) {
        self.turns = (self.turns + 1) % 4;
    }

    /// Ctrl+H: a room, or solid again.
    pub(crate) fn toggle_room(&mut self) {
        self.room = !self.room;
    }

    /// Whether it makes a room now.
    fn is_room(&self) -> bool {
        self.room && self.op == BrushOp::Add
    }

    /// Tab: the base's other side.
    pub(crate) fn tab(&mut self) {
        if self.field < 2 {
            self.field = 1 - self.field;
        }
    }

    /// Characters typed for a size: digits, a decimal point (`,` too), `+ - * / ( ) ^`.
    pub(crate) fn type_text(&mut self, text: &str) {
        if self.stage == Stage::Aim {
            return;
        }
        for c in text.chars() {
            match c {
                '0'..='9' | '.' | '+' | '-' | '*' | '/' | '(' | ')' | '^' => {
                    self.typed[self.field].push(c);
                }
                ',' => self.typed[self.field].push('.'),
                _ => {}
            }
        }
    }

    /// Backspace: the typed size's last character.
    pub(crate) fn backspace(&mut self) {
        self.typed[self.field].pop();
    }

    /// The typed size `i`, when it evaluates.
    fn typed(&self, i: usize) -> Option<f32> {
        let text = &self.typed[i];
        if text.is_empty() {
            return None;
        }
        expr::evaluate(text, 0.0).ok().map(|v| v as f32)
    }

    /// Enter: the base kept, then the height; true when the brush is to be made.
    pub(crate) fn enter(&mut self) -> bool {
        match self.stage {
            Stage::Aim => false,
            Stage::Base { plane, to, .. } => {
                self.stage = Stage::Height {
                    plane,
                    to,
                    height: 0.0,
                };
                self.field = 2;
                false
            }
            Stage::Height { .. } => true,
        }
    }

    /// The pointer's frame: the mark, the base or the height follows it, and a click (or
    /// a drag's release) goes on; true when the brush is to be made.
    pub(crate) fn pointer(&mut self, p: &Pointer) -> bool {
        self.landed = None;
        match self.stage {
            Stage::Aim => {
                self.mark = aim(p).map(|(corner, normal, landed)| {
                    self.landed = landed;
                    (corner, normal)
                });
                if p.press
                    && let Some((corner, normal)) = self.mark
                {
                    self.stage = Stage::Base {
                        plane: BasePlane::new(corner, normal),
                        to: Vec2::ZERO,
                        press: p.at,
                    };
                    self.field = 0;
                }
                false
            }
            Stage::Base { plane, to, press } => {
                let to = self.base_to(&plane, p).unwrap_or(to);
                let dragged = p.release && p.at.distance(press) > DRAG_PIXELS;
                self.stage = Stage::Base { plane, to, press };
                if p.press || dragged {
                    self.enter();
                }
                false
            }
            Stage::Height { plane, to, height } => {
                let height = self.height(&plane, to, p).unwrap_or(height);
                self.stage = Stage::Height { plane, to, height };
                p.press
            }
        }
    }

    /// The base's other corner for the pointer: where its ray meets the plane, in whole
    /// steps from the first corner or rounded to the grid as the rules say; typed sides
    /// take the pointer's lengths, its directions kept.
    fn base_to(&mut self, plane: &BasePlane, p: &Pointer) -> Option<Vec2> {
        // Another brush's corner or edge, as seen in the base's plane, first.
        if self.typed.iter().take(2).all(String::is_empty)
            && let Some(landed) = p.land(
                |c| c - plane.normal * (c - plane.corner).dot(plane.normal),
                true,
            )
        {
            self.landed = Some(landed);
            return Some(plane.coords(landed.point()).map(mm));
        }
        let free = plane
            .coords(hit_plane(p.view.ray(p.at), plane.corner, plane.normal)?)
            .to_array();
        let mut to = [0.0; 2];
        for (i, axis) in [plane.u, plane.v].into_iter().enumerate() {
            let start = f64::from(plane.corner.dot(axis));
            let value = p.snap.snap(
                Quantity::Position,
                start,
                start + f64::from(free[i]),
                p.invert,
                p.round,
            );
            to[i] = mm((value - start) as f32);
            if let Some(length) = self.typed(i) {
                to[i] = mm(if free[i] < 0.0 { -length } else { length });
            }
        }
        Some(Vec2::from_array(to))
    }

    /// The height for the pointer: along the normal through the base's middle, on the
    /// plane holding that line that faces the camera most, in whole steps as the rules
    /// say; a typed one out of the surface for a brush, into it for a cut.
    fn height(&mut self, plane: &BasePlane, to: Vec2, p: &Pointer) -> Option<f32> {
        if let Some(h) = self.typed(2) {
            return Some(mm(if self.op == BrushOp::Cut { -h } else { h }));
        }
        // The pointer over another brush's corner: its height.
        if let Some(landed) = p.land(|c| c, false) {
            self.landed = Some(landed);
            return Some(mm((landed.point() - plane.corner).dot(plane.normal)));
        }
        let middle = plane.point(to * 0.5);
        let n = plane.normal;
        let toward = (p.view.eye - middle).normalize_or_zero();
        let across = n.cross(toward.cross(n)).try_normalize()?;
        let free = (hit_plane(p.view.ray(p.at), middle, across)? - middle).dot(n);
        let value = p
            .snap
            .snap(Quantity::Position, 0.0, f64::from(free), p.invert, p.round);
        Some(mm(value as f32))
    }

    /// The brush the drawing makes now: none before its height, or while a size is none.
    pub(crate) fn made(&self) -> Option<Made> {
        let Stage::Height { plane, to, height } = self.stage else {
            return None;
        };
        if to.x.abs() < LEAST || to.y.abs() < LEAST || height.abs() < LEAST {
            return None;
        }
        let sign = |x: f32| if x < 0.0 { -1.0 } else { 1.0 };
        // Where it rises (a wedge's or stairs' +z), and its lengths along that and across.
        let (forward, along, across) = match self.turns {
            0 => (plane.v * sign(to.y), to.y.abs(), to.x.abs()),
            1 => (plane.u * sign(to.x), to.x.abs(), to.y.abs()),
            2 => (-plane.v * sign(to.y), to.y.abs(), to.x.abs()),
            _ => (-plane.u * sign(to.x), to.x.abs(), to.y.abs()),
        };
        let up = plane.normal * sign(height);
        let rotation = Quat::from_mat3(&Mat3::from_cols(up.cross(forward), up, forward));
        let shape = SHAPES[self.shape];
        let steps = (shape.shape == BrushShape::Stairs)
            .then(|| ((height.abs() / STEP_HEIGHT).round() as u32).max(1));
        Some(Made {
            shape,
            op: self.op,
            at: plane.point(to * 0.5),
            rotation: rotation.normalize(),
            size: Vec3::new(across, height.abs(), along),
            steps,
            hollow: if self.is_room() { ROOM_WALLS } else { 0.0 },
        })
    }

    /// The drawn box's edges in the world: the base's from the first corner around and,
    /// with a height, the top's and the four between.
    pub(crate) fn edges(&self, out: &mut Vec<(Vec3, Vec3)>) {
        out.clear();
        let (plane, to, height) = match self.stage {
            Stage::Aim => return,
            Stage::Base { plane, to, .. } => (plane, to, 0.0),
            Stage::Height { plane, to, height } => (plane, to, height),
        };
        let base =
            [Vec2::ZERO, Vec2::new(to.x, 0.0), to, Vec2::new(0.0, to.y)].map(|c| plane.point(c));
        let up = plane.normal * height;
        for i in 0..4 {
            let (a, b) = (base[i], base[(i + 1) % 4]);
            out.push((a, b));
            if height != 0.0 {
                out.push((a + up, b + up));
                out.push((a, a + up));
            }
        }
    }

    /// The sizes to show on the edges: the first side's, the second's and, with a
    /// height, the height's, each at its edge's middle, in metres.
    pub(crate) fn labels(&self, out: &mut Vec<(Vec3, f32)>) {
        out.clear();
        let (plane, to, height) = match self.stage {
            Stage::Aim => return,
            Stage::Base { plane, to, .. } => (plane, to, 0.0),
            Stage::Height { plane, to, height } => (plane, to, height),
        };
        out.push((plane.point(Vec2::new(to.x * 0.5, 0.0)), to.x.abs()));
        out.push((plane.point(Vec2::new(to.x, to.y * 0.5)), to.y.abs()));
        if height != 0.0 {
            let corner = plane.point(Vec2::new(to.x, 0.0));
            out.push((corner + plane.normal * height * 0.5, height.abs()));
        }
    }

    /// The hint by the pointer: the shape, its operation and sizes, the size being typed
    /// marked ("Stairs 1 × 1.8 × 1.2 m, 6 steps").
    pub(crate) fn hint(&self, out: &mut String) {
        out.clear();
        let shape = SHAPES[self.shape];
        out.push_str(shape.name);
        if self.op == BrushOp::Cut {
            out.push_str(" cut");
        } else if self.room {
            out.push_str(" room");
        }
        let sizes: [f32; 3] = match self.stage {
            Stage::Aim => {
                out.push_str(": click the first corner  ·  B: next shape");
                self.landed_note(out);
                return;
            }
            Stage::Base { to, .. } => [to.x.abs(), to.y.abs(), 0.0],
            Stage::Height { to, height, .. } => [to.x.abs(), to.y.abs(), height.abs()],
        };
        let shown = if self.field == 2 { 3 } else { 2 };
        for (i, size) in sizes.iter().enumerate().take(shown) {
            out.push_str(if i == 0 { " " } else { " × " });
            match (&self.typed[i], i == self.field) {
                (t, true) if !t.is_empty() => {
                    let _ = write!(out, "[{t}]");
                }
                (_, true) => {
                    let _ = write!(out, "[{}]", short(*size));
                }
                _ => out.push_str(&short(*size)),
            }
        }
        out.push_str(" m");
        if let Some(made) = self.made()
            && let Some(steps) = made.steps
        {
            let _ = write!(
                out,
                ", {steps} steps of {} m",
                short(made.size.y / steps as f32)
            );
        }
        self.landed_note(out);
    }

    /// Where the point landed, for the hint.
    fn landed_note(&self, out: &mut String) {
        match self.landed {
            Some(Landed::Corner(_)) => out.push_str("  ·  on a corner"),
            Some(Landed::Edge(_)) => out.push_str("  ·  on an edge"),
            None => {}
        }
    }
}

/// Where the first corner lands for the pointer: on the surface under it, else where its
/// ray meets the ground (y = 0). A normal near an axis is that axis, and the corner then
/// sits on the brushes' millimetre grid along it. In the plane it goes on the grid when
/// the rules say so (there is no start to step from, so steps round too), else on the
/// millimetre grid.
fn aim(p: &Pointer) -> Option<(Vec3, Vec3, Option<Landed>)> {
    let landed = p.land(|c| c, true);
    let (mut point, normal) = match p.surface {
        Some(hit) => hit,
        None => (hit_plane(p.view.ray(p.at), Vec3::ZERO, Vec3::Y)?, Vec3::Y),
    };
    let (normal, axis) = to_axis(normal);
    if let Some(landed) = landed {
        return Some((landed.point(), normal, Some(landed)));
    }
    if let Some(i) = axis {
        point[i] = (point[i] * 1000.0).round() / 1000.0;
    }
    let snapped = p.round || p.snap.on != p.invert;
    let step = p.snap.step(Quantity::Position) as f32;
    let plane = BasePlane::new(point, normal);
    for axis in [plane.u, plane.v] {
        let c = point.dot(axis);
        let on = if snapped {
            (c / step).round() * step
        } else {
            mm(c)
        };
        point += axis * (on - c);
    }
    Some((point, normal, None))
}

/// `n` as the axis it is nearly along, with that axis' index; or as it is.
fn to_axis(n: Vec3) -> (Vec3, Option<usize>) {
    let n = n.try_normalize().unwrap_or(Vec3::Y);
    for i in 0..3 {
        if n[i].abs() >= AXIS_COS {
            let mut axis = Vec3::ZERO;
            axis[i] = n[i].signum();
            return (axis, Some(i));
        }
    }
    (n, None)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::camera::OrbitCamera;

    /// Looking down at the origin from above and in front, at a slant.
    fn view() -> ViewProj {
        let camera = OrbitCamera {
            target: Vec3::ZERO,
            yaw: 0.3,
            pitch: 0.9,
            distance: 12.0,
            ..OrbitCamera::default()
        };
        ViewProj::new(&camera, [0, 0, 1200, 900])
    }

    fn pointer<'a>(view: &'a ViewProj, snap: &'a Snapping, at: Vec3) -> Pointer<'a> {
        Pointer {
            view,
            at: view.screen(at).unwrap(),
            surface: None,
            press: false,
            release: false,
            snap,
            invert: false,
            round: false,
            corners: &[],
            edges: &[],
            reach: 8.0,
        }
    }

    fn near(a: Vec3, b: Vec3) -> bool {
        a.distance(b) < 2e-3
    }

    #[test]
    fn a_base_plane_runs_along_x_on_floors_and_up_walls() {
        let floor = BasePlane::new(Vec3::ZERO, Vec3::Y);
        assert_eq!((floor.u, floor.v), (Vec3::X, Vec3::Z));
        let wall = BasePlane::new(Vec3::ZERO, Vec3::X);
        assert_eq!(wall.v, Vec3::Y, "up the wall");
        assert!(
            near(wall.u.cross(wall.normal), wall.v),
            "(u, n, v) as (x, y, z)"
        );
        let ceiling = BasePlane::new(Vec3::ZERO, Vec3::NEG_Y);
        assert!(near(ceiling.u.cross(ceiling.normal), ceiling.v));
        let slope = BasePlane::new(Vec3::ZERO, Vec3::new(0.0, 1.0, 1.0));
        assert!(slope.v.y > 0.0 && slope.u.dot(slope.normal).abs() < 1e-6);
        assert!(near(slope.u.cross(slope.normal), slope.v));
    }

    #[test]
    fn three_clicks_draw_a_box_on_the_ground_rising_from_its_first_corner() {
        let (view, snap) = (view(), Snapping::default());
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        // Over nothing: the ground; the first click sets the corner.
        let mut p = pointer(&view, &snap, Vec3::new(1.0, 0.0, 2.0));
        assert!(!d.pointer(&p));
        assert!(near(d.mark.unwrap().0, Vec3::new(1.0, 0.0, 2.0)));
        assert_eq!(d.mark.unwrap().1, Vec3::Y);
        p.press = true;
        d.pointer(&p);
        assert!(matches!(d.stage, Stage::Base { .. }));
        // The base follows the pointer; the second click keeps it.
        let mut p = pointer(&view, &snap, Vec3::new(3.5, 0.0, 0.5));
        d.pointer(&p);
        let Stage::Base { to, .. } = d.stage else {
            panic!()
        };
        assert!((to - Vec2::new(2.5, -1.5)).length() < 2e-3, "{to}");
        p.press = true;
        d.pointer(&p);
        assert!(matches!(d.stage, Stage::Height { .. }));
        assert!(d.made().is_none(), "no height yet");
        // The height: the pointer over the base's middle, 3 m up.
        let middle = Vec3::new(2.25, 0.0, 1.25);
        let p = pointer(&view, &snap, middle + Vec3::Y * 3.0);
        d.pointer(&p);
        let made = d.made().unwrap();
        assert!(near(made.at, middle), "{}", made.at);
        assert!(
            (made.size - Vec3::new(2.5, 3.0, 1.5)).length() < 5e-3,
            "{}",
            made.size
        );
        // It rises away from the first corner: toward −z here.
        assert!(near(made.rotation * Vec3::Z, Vec3::NEG_Z));
        assert!(near(made.rotation * Vec3::Y, Vec3::Y));
        let mut p = p;
        p.press = true;
        assert!(d.pointer(&p), "the third click makes it");
    }

    #[test]
    fn a_press_dragged_away_and_released_keeps_the_base() {
        let (view, snap) = (view(), Snapping::default());
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        let mut p = pointer(&view, &snap, Vec3::ZERO);
        p.press = true;
        d.pointer(&p);
        // Released where it was pressed: a click, the base still follows.
        let mut p = pointer(&view, &snap, Vec3::ZERO);
        p.release = true;
        d.pointer(&p);
        assert!(matches!(d.stage, Stage::Base { .. }));
        let mut p = pointer(&view, &snap, Vec3::new(2.0, 0.0, 2.0));
        p.release = true;
        d.pointer(&p);
        assert!(matches!(d.stage, Stage::Height { .. }));
    }

    #[test]
    fn snapping_puts_the_corner_on_the_grid_and_sizes_in_steps() {
        let view = view();
        let snap = Snapping {
            on: true,
            position: 0.5,
            ..Snapping::default()
        };
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        let mut p = pointer(&view, &snap, Vec3::new(1.2, 0.0, 2.3));
        p.press = true;
        d.pointer(&p);
        assert!(near(d.mark.unwrap().0, Vec3::new(1.0, 0.0, 2.5)));
        let p = pointer(&view, &snap, Vec3::new(2.9, 0.0, 4.1));
        d.pointer(&p);
        let Stage::Base { to, .. } = d.stage else {
            panic!()
        };
        assert_eq!(to, Vec2::new(2.0, 1.5));
        // Shift drags freely while snapping is on.
        let mut p = pointer(&view, &snap, Vec3::new(2.9, 0.0, 4.1));
        p.invert = true;
        d.pointer(&p);
        let Stage::Base { to, .. } = d.stage else {
            panic!()
        };
        assert!((to - Vec2::new(1.9, 1.6)).length() < 2e-3, "{to}");
        assert_eq!(to * 1000.0, (to * 1000.0).round(), "on the millimetres");
    }

    #[test]
    fn a_surface_nearly_on_an_axis_is_on_it_and_on_the_millimetre_grid() {
        let (view, snap) = (view(), Snapping::default());
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        let mut p = pointer(&view, &snap, Vec3::new(1.0, 2.0, 0.0));
        // A wall facing +x, its normal measured 2° off.
        let off = Quat::from_rotation_z(2f32.to_radians()) * Vec3::X;
        p.surface = Some((Vec3::new(3.000_4, 1.234_56, 0.5), off));
        d.pointer(&p);
        let (corner, normal) = d.mark.unwrap();
        assert_eq!(normal, Vec3::X);
        assert_eq!(corner.x, 3.0);
        assert!(
            (corner.y - 1.235).abs() < 1e-6,
            "free in the plane, on the millimetres"
        );
    }

    #[test]
    fn typed_sizes_take_the_pointers_place_and_a_cut_goes_in() {
        let (view, snap) = (view(), Snapping::default());
        let mut d = Draw::new(BrushOp::Cut, 0, 1);
        let mut p = pointer(&view, &snap, Vec3::ZERO);
        // A wall facing +z.
        p.surface = Some((Vec3::new(0.0, 1.0, 0.0), Vec3::Z));
        p.press = true;
        d.pointer(&p);
        d.type_text("1");
        d.tab();
        d.type_text("2,1");
        // On a wall facing +z the first side runs to −x: the pointer there and above the
        // corner, both sides go its way.
        let p = pointer(&view, &snap, Vec3::new(-0.3, 1.5, 0.0));
        d.pointer(&p);
        let Stage::Base { to, .. } = d.stage else {
            panic!()
        };
        assert_eq!(to, Vec2::new(1.0, 2.1));
        assert!(!d.enter());
        d.type_text("0.3");
        let p = pointer(&view, &snap, Vec3::new(5.0, 0.0, 5.0));
        d.pointer(&p);
        let made = d.made().unwrap();
        assert!(near(made.size, Vec3::new(1.0, 0.3, 2.1)), "{}", made.size);
        // A cut's height goes into the wall: its +y along −z.
        assert!(near(made.rotation * Vec3::Y, Vec3::NEG_Z));
        assert!(near(made.at, Vec3::new(-0.5, 2.05, 0.0)), "{}", made.at);
        let mut hint = String::new();
        d.hint(&mut hint);
        assert_eq!(hint, "Box cut 1 × 2.1 × [0.3] m");
        assert!(d.enter(), "Enter makes it");
    }

    #[test]
    fn r_turns_where_stairs_rise_and_their_steps_are_about_20_cm() {
        let (view, snap) = (view(), Snapping::default());
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        d.again(BrushOp::Add);
        d.again(BrushOp::Add);
        assert_eq!(SHAPES[d.shape].name, "Stairs");
        let mut p = pointer(&view, &snap, Vec3::ZERO);
        p.press = true;
        d.pointer(&p);
        d.type_text("1");
        d.tab();
        d.type_text("1.8");
        d.pointer(&pointer(&view, &snap, Vec3::new(1.0, 0.0, 2.0)));
        d.enter();
        d.type_text("1.2");
        d.pointer(&pointer(&view, &snap, Vec3::ZERO));
        let made = d.made().unwrap();
        assert_eq!(made.steps, Some(6));
        assert!(
            near(made.rotation * Vec3::Z, Vec3::Z),
            "along the second side"
        );
        assert!(near(made.size, Vec3::new(1.0, 1.2, 1.8)));
        let mut hint = String::new();
        d.hint(&mut hint);
        assert_eq!(hint, "Stairs 1 × 1.8 × [1.2] m, 6 steps of 0.2 m");
        // A quarter turn: rising along the first side, the lengths swapped.
        d.turn();
        let made = d.made().unwrap();
        assert!(near(made.rotation * Vec3::Z, Vec3::X));
        assert!(near(made.size, Vec3::new(1.8, 1.2, 1.0)));
        assert!(near(made.rotation * Vec3::Y, Vec3::Y));
    }

    #[test]
    fn b_again_takes_the_next_shape_and_shift_b_makes_it_a_cut() {
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        d.again(BrushOp::Add);
        assert_eq!((SHAPES[d.shape].name, d.op), ("Wedge", BrushOp::Add));
        d.again(BrushOp::Cut);
        assert_eq!((SHAPES[d.shape].name, d.op), ("Wedge", BrushOp::Cut));
        for _ in 0..7 {
            d.again(BrushOp::Cut);
        }
        assert_eq!(SHAPES[d.shape].name, "Box", "around");
        let mut hint = String::new();
        d.hint(&mut hint);
        assert_eq!(hint, "Box cut: click the first corner  ·  B: next shape");
    }

    #[test]
    fn ctrl_h_draws_a_room_and_a_cut_is_never_one() {
        let (view, snap) = (view(), Snapping::default());
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        d.toggle_room();
        let mut p = pointer(&view, &snap, Vec3::ZERO);
        p.press = true;
        d.pointer(&p);
        d.pointer(&pointer(&view, &snap, Vec3::new(4.0, 0.0, 3.0)));
        d.enter();
        d.type_text("3");
        d.pointer(&pointer(&view, &snap, Vec3::ZERO));
        let made = d.made().unwrap();
        assert_eq!(made.hollow, ROOM_WALLS);
        assert!(
            near(made.size, Vec3::new(4.0, 3.0, 3.0)),
            "the drawn size is its inside"
        );
        let mut hint = String::new();
        d.hint(&mut hint);
        assert!(hint.starts_with("Box room 4 × 3 × [3] m"), "{hint}");
        d.again(BrushOp::Cut);
        assert_eq!(d.made().unwrap().hollow, 0.0);
    }

    #[test]
    fn corners_and_edges_of_other_brushes_come_before_the_grid() {
        let view = view();
        let snap = Snapping {
            on: true,
            ..Snapping::default()
        };
        // A box from (2, 0, 2) to (3, 1.5, 3), off the 0.1 m grid by a little.
        let (lo, hi) = (Vec3::new(2.013, 0.0, 2.027), Vec3::new(3.013, 1.5, 3.027));
        let corners = [
            lo,
            Vec3::new(hi.x, 0.0, lo.z),
            Vec3::new(hi.x, 0.0, hi.z),
            Vec3::new(lo.x, 0.0, hi.z),
            Vec3::new(lo.x, 1.5, lo.z),
            hi,
        ];
        let edges = [(lo, Vec3::new(lo.x, 0.0, hi.z))];
        let with = |at: Vec3| Pointer {
            corners: &corners,
            edges: &edges,
            ..pointer(&view, &snap, at)
        };
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        // A few millimetres from its corner: the corner, not the grid.
        let mut p = with(lo + Vec3::new(0.004, 0.0, -0.003));
        p.press = true;
        d.pointer(&p);
        assert_eq!(d.mark.unwrap().0, lo);
        assert_eq!(d.landed, Some(Landed::Corner(lo)));
        // The base's other corner on the box's −x edge near its middle: on the edge, at a
        // whole step along it (z = 2.5).
        let half = Vec3::new(lo.x, 0.0, (lo.z + hi.z) * 0.5);
        d.pointer(&with(half + Vec3::new(0.003, 0.0, 0.0)));
        let Stage::Base { to, .. } = d.stage else {
            panic!()
        };
        assert!(matches!(d.landed, Some(Landed::Edge(_))), "{:?}", d.landed);
        assert!((to - Vec2::new(0.0, 2.5 - lo.z)).length() < 1e-5, "{to}");
        // Elsewhere, the grid again.
        d.pointer(&with(Vec3::new(1.0, 0.0, 0.0)));
        assert!(d.landed.is_none());
        // The height: the pointer over the box's top corner takes its 1.5 m.
        d.pointer(&with(Vec3::new(1.0, 0.0, 0.5)));
        d.enter();
        d.pointer(&with(hi));
        let Stage::Height { height, .. } = d.stage else {
            panic!()
        };
        assert!((height - 1.5).abs() < 1e-6, "{height}");
    }

    #[test]
    fn edges_and_labels_follow_the_stage() {
        let (view, snap) = (view(), Snapping::default());
        let mut d = Draw::new(BrushOp::Add, 0, 1);
        let (mut edges, mut labels) = (Vec::new(), Vec::new());
        d.edges(&mut edges);
        assert!(edges.is_empty());
        let mut p = pointer(&view, &snap, Vec3::ZERO);
        p.press = true;
        d.pointer(&p);
        d.pointer(&pointer(&view, &snap, Vec3::new(2.0, 0.0, 1.0)));
        d.edges(&mut edges);
        d.labels(&mut labels);
        assert_eq!((edges.len(), labels.len()), (4, 2));
        d.enter();
        d.type_text("3");
        d.pointer(&pointer(&view, &snap, Vec3::ZERO));
        d.edges(&mut edges);
        d.labels(&mut labels);
        assert_eq!((edges.len(), labels.len()), (12, 3));
        assert!((labels[2].1 - 3.0).abs() < 1e-6);
    }
}
