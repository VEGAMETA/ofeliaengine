//! Keyboard transforms (ADR 0107 E2; `docs/research/ux/viewport-tools.md` §2, §8): a key
//! moves, turns or scales the selection with the pointer at once, no handle to grab (Q,
//! R, E in our keys; Blender's G, R, S). While one runs:
//!
//! - **X, Y, Z** keep it to an axis: the gizmo's first (the world's, or the active
//!   entity's after Space), the other ones with the same key again, free a third time.
//!   With **Shift**, to the plane across that axis.
//! - **A typed value** takes the pointer's place: metres, degrees or a factor, and
//!   expressions (`2*1.5`, `-90`, `(1+2)/4`, [`crate::expr`]); Backspace edits it. Without
//!   an axis, a move goes along x, a turn about the view's axis, and a scale is uniform.
//! - The pointer: a move follows it in the plane through the pivot (the view's, the one
//!   holding the axis that faces the camera, or the plane kept to); a turn follows its
//!   angle about the pivot on the screen, past half a turn too; a scale follows its
//!   distance from the pivot.
//! - **The owner's snapping rules** as for the gizmo: Shift for steps from the start, Ctrl
//!   for the grid ([`crate::Snapping`]). Typed values are exact.
//!
//! The editor keeps it with Enter or a left click and takes it back with Escape or a right
//! click; the keys that start the transforms start the others over.

use crate::expr;
use crate::gizmo::{Change, Target, ViewProj, hit_plane};
use crate::snap::{Quantity, Snapping};
use core::fmt::Write as _;
use engine_core::math::{Quat, Vec2, Vec3};

/// The keys while a keyboard transform runs, for the status bar.
pub(crate) const HELP: &str = "X, Y, Z: axis (again: the other axes)  ·  Shift+X, Y, Z: plane  \
                               ·  type a value  ·  Enter or click: done  ·  Esc or right \
                               click: cancel";

/// What a keyboard transform does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Move,
    Turn,
    Scale,
}

impl Kind {
    /// The undo step's name.
    pub(crate) fn step(self) -> &'static str {
        match self {
            Self::Move => "Move",
            Self::Turn => "Rotate",
            Self::Scale => "Scale",
        }
    }

    /// The change that changes nothing.
    fn none(self) -> Change {
        match self {
            Self::Move => Change::Move(Vec3::ZERO),
            Self::Turn => Change::Turn(Quat::IDENTITY),
            Self::Scale => Change::Scale(Vec3::ONE),
        }
    }
}

/// What a keyboard transform is kept to; axes 0 (x), 1 (y), 2 (z) of the world's axes or,
/// `true`, the active entity's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Constraint {
    Free,
    Axis(usize, bool),
    /// The plane across the axis.
    Plane(usize, bool),
}

/// A keyboard transform while it runs.
#[derive(Debug, Clone)]
pub(crate) struct KeyTransform {
    pub(crate) kind: Kind,
    pub(crate) constraint: Constraint,
    /// The value typed, as typed.
    pub(crate) typed: String,
    /// What the typed text last evaluated to: kept while it does not (`2*` before `3`).
    value: Option<f64>,
    /// The typed text evaluates now.
    valid: bool,
    pub(crate) pivot: Vec3,
    /// The world's axes, and the active entity's.
    world: [Vec3; 3],
    own: [Vec3; 3],
    /// A first X, Y or Z takes the active entity's axes (the gizmo shows them).
    first_own: bool,
    /// The pointer at the start, physical pixels.
    start: Vec2,
    /// A turn so far: its angle on the screen, counterclockwise in radians, and the
    /// pointer's direction from the pivot when it was last measured.
    turned: f32,
    toward: Vec2,
    pub(crate) targets: Vec<Target>,
    /// Its undo step: one per transform.
    pub(crate) merge: u64,
    /// What it does now, applied when it changes; and its amount for the hint (metres
    /// along an axis, degrees, a factor).
    pub(crate) last: Change,
    amount: f32,
    /// The window lost the keyboard: it is taken back.
    pub(crate) cancel: bool,
}

impl KeyTransform {
    /// A transform of `targets` about `pivot`, to [`KeyTransform::begin`]; `own` are the
    /// active entity's axes, which a first X, Y or Z takes when `first_own`.
    pub(crate) fn new(pivot: Vec3, own: [Vec3; 3], first_own: bool, targets: Vec<Target>) -> Self {
        Self {
            kind: Kind::Move,
            constraint: Constraint::Free,
            typed: String::new(),
            value: None,
            valid: false,
            pivot,
            world: [Vec3::X, Vec3::Y, Vec3::Z],
            own,
            first_own,
            start: Vec2::ZERO,
            turned: 0.0,
            toward: Vec2::ZERO,
            targets,
            merge: 0,
            last: Kind::Move.none(),
            amount: 0.0,
            cancel: false,
        }
    }

    /// Starts as `kind` from the pointer at `start`, or over (the targets back where they
    /// were): free, with nothing typed, as undo step `merge`.
    pub(crate) fn begin(&mut self, kind: Kind, view: &ViewProj, start: Vec2, merge: u64) {
        self.kind = kind;
        self.constraint = Constraint::Free;
        self.typed.clear();
        self.value = None;
        self.valid = false;
        self.start = start;
        self.turned = 0.0;
        self.toward = view.screen(self.pivot).map_or(Vec2::ZERO, |c| start - c);
        self.merge = merge;
        self.last = kind.none();
        self.amount = if kind == Kind::Scale { 1.0 } else { 0.0 };
    }

    /// The world's axes, or the active entity's.
    fn axes(&self, own: bool) -> [Vec3; 3] {
        if own { self.own } else { self.world }
    }

    /// X, Y or Z (`axis`), with Shift (`plane`) the plane across it: the gizmo's axes
    /// first, then the other ones, then free.
    pub(crate) fn constrain(&mut self, axis: usize, plane: bool) {
        let kept = |own| {
            if plane {
                Constraint::Plane(axis, own)
            } else {
                Constraint::Axis(axis, own)
            }
        };
        let (first, second) = (kept(self.first_own), kept(!self.first_own));
        self.constraint = if self.constraint == first {
            second
        } else if self.constraint == second {
            Constraint::Free
        } else {
            first
        };
    }

    /// Characters typed for the value: digits, a decimal point (`,` too), `+ - * / ( ) ^`;
    /// the rest are the keys' business.
    pub(crate) fn type_text(&mut self, text: &str) {
        let before = self.typed.len();
        for c in text.chars() {
            match c {
                '0'..='9' | '.' | '+' | '-' | '*' | '/' | '(' | ')' | '^' => self.typed.push(c),
                ',' => self.typed.push('.'),
                _ => {}
            }
        }
        if self.typed.len() != before {
            self.evaluate();
        }
    }

    /// Backspace: the typed value's last character.
    pub(crate) fn backspace(&mut self) {
        if self.typed.pop().is_some() {
            self.evaluate();
        }
    }

    fn evaluate(&mut self) {
        if self.typed.is_empty() {
            self.value = None;
            self.valid = false;
            return;
        }
        // Relative to nothing moved, turned or scaled: `*2` scales twice as much.
        let current = if self.kind == Kind::Scale { 1.0 } else { 0.0 };
        match expr::evaluate(&self.typed, current) {
            Ok(v) => {
                self.value = Some(v);
                self.valid = true;
            }
            Err(_) => self.valid = false,
        }
    }

    /// The axis a typed move goes along, a turn turns about, and the axes a scale's
    /// factors are along.
    fn kept_axes(&self) -> [Vec3; 3] {
        match self.constraint {
            Constraint::Free => self.axes(self.first_own),
            Constraint::Axis(_, own) | Constraint::Plane(_, own) => self.axes(own),
        }
    }

    /// What the transform does with the pointer at `at` (physical pixels), snapped by the
    /// rules with Shift (`invert`) and Ctrl (`round`) as held; none when the pointer's ray
    /// misses the plane it moves in (keep the last change).
    pub(crate) fn change(
        &mut self,
        view: &ViewProj,
        at: Vec2,
        snap: &Snapping,
        invert: bool,
        round: bool,
    ) -> Option<Change> {
        let snapped = |q: Quantity, start: f32, free: f32| {
            snap.snap(q, f64::from(start), f64::from(free), invert, round) as f32
        };
        let toward = (view.eye - self.pivot).normalize_or_zero();
        let axes = self.kept_axes();
        let typed = self.value.map(|v| v as f32);
        let change = match self.kind {
            Kind::Move => {
                // A move along `axis` by `d`: the pivot's coordinate on it snapped.
                let along = |axis: Vec3, d: f32| {
                    let start = self.pivot.dot(axis);
                    axis * (snapped(Quantity::Position, start, start + d) - start)
                };
                let line = match self.constraint {
                    Constraint::Free => axes[0],
                    Constraint::Axis(i, _) => axes[i],
                    Constraint::Plane(i, _) => axes[(i + 1) % 3],
                };
                if let Some(d) = typed {
                    self.amount = d;
                    Change::Move(line * d)
                } else {
                    // The plane the pointer moves in through the pivot.
                    let normal = match self.constraint {
                        Constraint::Free => toward,
                        Constraint::Axis(i, _) => {
                            let a = axes[i];
                            let n = a.cross(toward.cross(a)).normalize_or_zero();
                            if n == Vec3::ZERO { toward } else { n }
                        }
                        Constraint::Plane(i, _) => axes[i],
                    };
                    let from = hit_plane(view.ray(self.start), self.pivot, normal)?;
                    let to = hit_plane(view.ray(at), self.pivot, normal)?;
                    let v = to - from;
                    let d = match self.constraint {
                        Constraint::Axis(i, _) => along(axes[i], v.dot(axes[i])),
                        Constraint::Plane(i, _) => {
                            let (a, b) = (axes[(i + 1) % 3], axes[(i + 2) % 3]);
                            along(a, v.dot(a)) + along(b, v.dot(b))
                        }
                        Constraint::Free => axes.iter().map(|&a| along(a, v.dot(a))).sum(),
                    };
                    self.amount = d.dot(line);
                    Change::Move(d)
                }
            }
            Kind::Turn => {
                // About the view's axis (toward the camera: counterclockwise on the
                // screen), or the axis kept to, its sign by which way it faces.
                let (axis, facing) = match self.constraint {
                    Constraint::Free => (toward, 1.0),
                    Constraint::Axis(i, _) | Constraint::Plane(i, _) => {
                        let a = axes[i];
                        (a, if a.dot(toward) < 0.0 { -1.0 } else { 1.0 })
                    }
                };
                let degrees = match typed {
                    Some(d) => d,
                    None => {
                        self.track(view, at);
                        snapped(Quantity::Angle, 0.0, (self.turned * facing).to_degrees())
                    }
                };
                self.amount = degrees;
                Change::Turn(Quat::from_axis_angle(axis, degrees.to_radians()))
            }
            Kind::Scale => {
                let factor = match typed {
                    Some(f) => f,
                    None => {
                        let center = view.screen(self.pivot)?;
                        let from = (self.start - center).length();
                        if from < 1.0 {
                            return None;
                        }
                        snapped(Quantity::Scale, 1.0, (at - center).length() / from)
                    }
                };
                self.amount = factor;
                let mut f = Vec3::splat(factor);
                match self.constraint {
                    Constraint::Free => {}
                    Constraint::Axis(i, _) => {
                        f = Vec3::ONE;
                        f[i] = factor;
                    }
                    Constraint::Plane(i, _) => f[i] = 1.0,
                }
                Change::Scale(f)
            }
        };
        Some(change)
    }

    /// A turn follows the pointer around the pivot on the screen: the angle since it was
    /// last measured is added, so it goes on past half a turn.
    fn track(&mut self, view: &ViewProj, at: Vec2) {
        let Some(center) = view.screen(self.pivot) else {
            return;
        };
        let v = at - center;
        // Too near the pivot to tell an angle.
        if v.length() < 2.0 {
            return;
        }
        if self.toward.length() >= 2.0 {
            // Counterclockwise as seen: the screen's y runs down.
            self.turned -= self.toward.perp_dot(v).atan2(self.toward.dot(v));
        }
        self.toward = v;
    }

    /// The pivot and the axes the transform's change is about (`gizmo::apply`).
    pub(crate) fn frame(&self) -> (Vec3, [Vec3; 3]) {
        (self.pivot, self.kept_axes())
    }

    /// The lines that show what it does, on the screen: the axis it is kept to, or the
    /// plane's two, through the pivot (an axis' color index each); for a turn or a scale, a
    /// line from the pivot to the pointer at `at` too (index 3).
    pub(crate) fn guides(
        &self,
        view: &ViewProj,
        at: Option<Vec2>,
        out: &mut Vec<(Vec2, Vec2, usize)>,
    ) {
        out.clear();
        if self.kind != Kind::Move
            && let (Some(center), Some(at)) = (view.screen(self.pivot), at)
        {
            out.push((center, at, 3));
        }
        let (lines, own): (&[usize], bool) = match self.constraint {
            Constraint::Free => return,
            Constraint::Axis(i, own) => (&[i][..], own),
            Constraint::Plane(i, own) => match i {
                0 => (&[1, 2][..], own),
                1 => (&[0, 2][..], own),
                _ => (&[0, 1][..], own),
            },
        };
        let axes = self.axes(own);
        for &i in lines {
            if let Some((a, b)) = line_on_screen(view, self.pivot, axes[i]) {
                out.push((a, b, i));
            }
        }
    }

    /// The hint by the pointer: what it does, kept to what, how much; and whether it
    /// snaps in steps (`steps`) or to the grid (`grid`).
    pub(crate) fn hint(&self, out: &mut String, steps: bool, grid: bool) {
        out.clear();
        let names = ["X", "Y", "Z"];
        let planes = ["YZ", "XZ", "XY"];
        out.push_str(self.kind.step());
        let (word, own) = match (self.kind, self.constraint) {
            (Kind::Turn, Constraint::Free) => (String::from(" about the view"), false),
            (_, Constraint::Free) => (String::new(), false),
            (Kind::Turn, Constraint::Axis(i, own) | Constraint::Plane(i, own)) => {
                (format!(" about {}", names[i]), own)
            }
            (_, Constraint::Axis(i, own)) => (format!(" along {}", names[i]), own),
            (_, Constraint::Plane(i, own)) => (format!(" on {}", planes[i]), own),
        };
        out.push_str(&word);
        if own {
            out.push_str(" (local)");
        }
        out.push_str(": ");
        let (before, after) = match self.kind {
            Kind::Move => ("", " m"),
            Kind::Turn => ("", "°"),
            Kind::Scale => ("×", ""),
        };
        match self.value {
            _ if self.typed.is_empty() => match self.last {
                Change::Move(d) if !matches!(self.constraint, Constraint::Axis(..)) => {
                    let _ = write!(out, "({}, {}, {})", short(d.x), short(d.y), short(d.z));
                }
                _ => {
                    let _ = write!(out, "{before}{}{after}", short(self.amount));
                }
            },
            Some(v) if self.valid => {
                let v = short(v as f32);
                if v != self.typed {
                    let _ = write!(out, "{} = ", self.typed);
                }
                let _ = write!(out, "{before}{v}{after}");
            }
            _ => {
                let _ = write!(out, "{} …", self.typed);
            }
        }
        // A typed value is exact.
        let note = match (self.typed.is_empty(), grid, steps) {
            (true, true, _) => "  ·  on the grid",
            (true, false, true) => "  ·  in steps",
            _ => "",
        };
        out.push_str(note);
    }
}

/// A number for the hint: at most four decimals, no trailing zeros, no `-0`.
fn short(v: f32) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() {
        String::from("0")
    } else {
        String::from(s)
    }
}

/// The line through `p` along `direction` on the screen, as far as the viewport shows it:
/// its part in front of the camera, clipped to the viewport; none when none is in view.
pub(crate) fn line_on_screen(view: &ViewProj, p: Vec3, direction: Vec3) -> Option<(Vec2, Vec2)> {
    let reach = view.per_pixel(p) * 1e5;
    let depth = (p - view.eye).dot(view.forward);
    let k = direction.dot(view.forward);
    // In front of the eye: a hundredth of the pivot's depth at least.
    let near = (depth * 0.01).max(1e-3);
    let (mut t0, mut t1) = (-reach, reach);
    if k > 1e-6 {
        t0 = t0.max((near - depth) / k);
    } else if k < -1e-6 {
        t1 = t1.min((near - depth) / k);
    } else if depth < near {
        return None;
    }
    if t0 >= t1 {
        return None;
    }
    let a = view.screen(p + direction * t0)?;
    let b = view.screen(p + direction * t1)?;
    clip(a, b, view.rect)
}

/// The part of the segment `a`–`b` inside `rect` (x, y, width, height), Liang and Barsky's
/// way.
fn clip(a: Vec2, b: Vec2, rect: [f32; 4]) -> Option<(Vec2, Vec2)> {
    let d = b - a;
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for (p, q) in [
        (-d.x, a.x - rect[0]),
        (d.x, rect[0] + rect[2] - a.x),
        (-d.y, a.y - rect[1]),
        (d.y, rect[1] + rect[3] - a.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
        }
    }
    (t0 <= t1).then(|| (a + d * t0, a + d * t1))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::camera::OrbitCamera;
    use engine_core::math::Affine3A;
    use engine_ecs::{Entity, Transform};

    fn view() -> ViewProj {
        // Looking at the origin from +Z, level: x to the right, y up on the screen.
        let camera = OrbitCamera {
            target: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            distance: 10.0,
            ..OrbitCamera::default()
        };
        ViewProj::new(&camera, [0, 0, 1000, 800])
    }

    fn transform(kind: Kind, start: Vec2) -> (KeyTransform, ViewProj) {
        let view = view();
        let target = Target {
            entity: Entity::NULL,
            world: Affine3A::IDENTITY,
            parent_inverse: Affine3A::IDENTITY,
            local: Transform::IDENTITY,
        };
        // The active entity turned a quarter about y: its x lies along the world's −z.
        let q = Quat::from_rotation_y(core::f32::consts::FRAC_PI_2);
        let own = [q * Vec3::X, q * Vec3::Y, q * Vec3::Z];
        let mut t = KeyTransform::new(Vec3::ZERO, own, false, vec![target]);
        t.begin(kind, &view, start, 1);
        (t, view)
    }

    #[test]
    fn an_axis_key_takes_the_gizmos_axes_then_the_others_then_none() {
        let (mut t, _) = transform(Kind::Move, Vec2::new(600.0, 400.0));
        t.constrain(0, false);
        assert_eq!(t.constraint, Constraint::Axis(0, false));
        t.constrain(0, false);
        assert_eq!(
            t.constraint,
            Constraint::Axis(0, true),
            "again: the entity's"
        );
        t.constrain(0, false);
        assert_eq!(t.constraint, Constraint::Free, "a third time: free");
        t.constrain(1, false);
        t.constrain(2, true);
        assert_eq!(
            t.constraint,
            Constraint::Plane(2, false),
            "another key takes over"
        );
        // With the gizmo along the entity's axes, a first press takes them.
        t.first_own = true;
        t.constraint = Constraint::Free;
        t.constrain(1, false);
        assert_eq!(t.constraint, Constraint::Axis(1, true));
    }

    #[test]
    fn typed_values_are_exact_and_expressions_work_as_typed() {
        let (mut t, view) = transform(Kind::Move, Vec2::new(600.0, 400.0));
        let snap = Snapping::default();
        let at = Vec2::new(650.0, 370.0);
        t.constrain(2, false);
        t.type_text("2");
        // `2*` does not evaluate: the last value stays until it does again.
        t.type_text("*");
        assert_eq!(
            t.change(&view, at, &snap, false, false),
            Some(Change::Move(Vec3::new(0.0, 0.0, 2.0)))
        );
        let mut hint = String::new();
        t.hint(&mut hint, false, false);
        assert_eq!(hint, "Move along Z: 2* …");
        // Letters are the keys' business.
        t.type_text("x1.5");
        assert_eq!(
            t.change(&view, at, &snap, false, false),
            Some(Change::Move(Vec3::new(0.0, 0.0, 3.0)))
        );
        t.hint(&mut hint, false, false);
        assert_eq!(hint, "Move along Z: 2*1.5 = 3 m");
        for _ in 0..4 {
            t.backspace();
        }
        t.hint(&mut hint, false, false);
        assert_eq!(hint, "Move along Z: 2 m");
        // Along the entity's z (the world's x) the second time; a comma is a point.
        t.constrain(2, false);
        t.typed.clear();
        t.type_text("-0,5");
        let Some(Change::Move(d)) = t.change(&view, at, &snap, false, false) else {
            panic!("a move");
        };
        assert!((d - Vec3::new(-0.5, 0.0, 0.0)).length() < 1e-6, "{d}");
        // Without an axis: along x.
        t.constrain(2, false);
        assert_eq!(
            t.change(&view, at, &snap, false, false),
            Some(Change::Move(Vec3::new(-0.5, 0.0, 0.0)))
        );
    }

    #[test]
    fn the_pointer_moves_along_the_axis_in_the_plane_that_faces_the_camera() {
        let (mut t, view) = transform(Kind::Move, Vec2::new(600.0, 400.0));
        let snap = Snapping::default();
        t.constrain(0, false);
        let per_pixel = view.per_pixel(Vec3::ZERO);
        let Some(Change::Move(d)) = t.change(&view, Vec2::new(650.0, 330.0), &snap, false, false)
        else {
            panic!("a move");
        };
        assert!((d.x - 50.0 * per_pixel).abs() < 1e-3, "{d}");
        assert_eq!((d.y, d.z), (0.0, 0.0), "along x only");
        // Free: in the view's plane; Ctrl puts the pivot on the grid.
        t.constrain(0, false);
        t.constrain(0, false);
        let Some(Change::Move(d)) = t.change(&view, Vec2::new(650.0, 330.0), &snap, false, true)
        else {
            panic!("a move");
        };
        assert!(d.y > 0.1 && d.z == 0.0, "{d}");
        assert!((d.x * 10.0 - (d.x * 10.0).round()).abs() < 1e-4, "{d}");
        assert!((d.y * 10.0 - (d.y * 10.0).round()).abs() < 1e-4, "{d}");
    }

    #[test]
    fn a_turn_follows_the_pointers_angle_past_half_a_turn() {
        // The pointer starts right of the pivot and goes around it counterclockwise.
        let (mut t, view) = transform(Kind::Turn, Vec2::new(600.0, 400.0));
        let snap = Snapping::default();
        let around = |degrees: f32| {
            let r = degrees.to_radians();
            Vec2::new(500.0 + 100.0 * r.cos(), 400.0 - 100.0 * r.sin())
        };
        let mut last = None;
        for step in 1..=27 {
            last = t.change(&view, around(step as f32 * 10.0), &snap, false, false);
        }
        // 270° about the view's axis (+z, toward the camera).
        let Some(Change::Turn(q)) = last else {
            panic!("a turn");
        };
        assert!(
            (q * Vec3::X - Vec3::new(0.0, -1.0, 0.0)).length() < 1e-3,
            "{q}"
        );
        assert!((t.amount - 270.0).abs() < 1e-2, "{}", t.amount);
        // About y, seen edge-on: the same angle, with Shift in 45° steps.
        t.constrain(1, false);
        let Some(Change::Turn(q)) = t.change(&view, around(280.0), &snap, true, false) else {
            panic!("a turn");
        };
        let (axis, _) = q.to_axis_angle();
        assert!(axis.y.abs() > 0.999, "{axis}");
        assert_eq!(t.amount, 270.0, "280° in 45° steps");
        // Typed: right-handed about the axis.
        t.type_text("90");
        let Some(Change::Turn(q)) = t.change(&view, around(10.0), &snap, false, false) else {
            panic!("a turn");
        };
        assert!((q * Vec3::Z - Vec3::X).length() < 1e-5, "{q}");
    }

    #[test]
    fn a_scale_follows_the_pointers_distance_and_a_plane_keeps_its_axis() {
        let (mut t, view) = transform(Kind::Scale, Vec2::new(600.0, 400.0));
        let snap = Snapping::default();
        assert_eq!(
            t.change(&view, Vec2::new(700.0, 400.0), &snap, false, false),
            Some(Change::Scale(Vec3::splat(2.0)))
        );
        t.constrain(2, true);
        let Some(Change::Scale(f)) = t.change(&view, Vec2::new(500.0, 250.0), &snap, false, false)
        else {
            panic!("a scale");
        };
        assert!((f - Vec3::new(1.5, 1.5, 1.0)).length() < 1e-5, "{f}");
        t.type_text("*3");
        assert_eq!(
            t.change(&view, Vec2::new(500.0, 250.0), &snap, false, false),
            Some(Change::Scale(Vec3::new(3.0, 3.0, 1.0))),
            "relative to a factor of one"
        );
        let mut hint = String::new();
        t.hint(&mut hint, true, false);
        assert_eq!(hint, "Scale on XY: *3 = ×3", "a typed value is exact");
        t.backspace();
        t.backspace();
        t.change(&view, Vec2::new(500.0, 250.0), &snap, false, false);
        t.hint(&mut hint, true, false);
        assert_eq!(hint, "Scale on XY: ×1.5  ·  in steps");
    }

    #[test]
    fn guides_run_across_the_viewport_through_the_pivot() {
        let (mut t, view) = transform(Kind::Move, Vec2::new(600.0, 400.0));
        let mut out = Vec::new();
        t.guides(&view, None, &mut out);
        assert!(out.is_empty(), "none while free");
        t.constrain(0, false);
        t.guides(&view, None, &mut out);
        let [(a, b, 0)] = out[..] else {
            panic!("{out:?}");
        };
        assert!((a.x.min(b.x) - 0.0).abs() < 1e-2 && (a.x.max(b.x) - 1000.0).abs() < 1e-2);
        assert!((a.y - 400.0).abs() < 1e-2 && (b.y - 400.0).abs() < 1e-2);
        // The plane across y: x and z; z points at the camera, its line a point or less.
        t.constrain(1, true);
        t.guides(&view, None, &mut out);
        assert_eq!(out.iter().map(|g| g.2).collect::<Vec<_>>(), [0, 2]);
        // A turn: the pivot's line to the pointer first.
        t.begin(Kind::Turn, &view, Vec2::new(600.0, 400.0), 2);
        t.guides(&view, Some(Vec2::new(620.0, 380.0)), &mut out);
        assert_eq!(out, [(Vec2::new(500.0, 400.0), Vec2::new(620.0, 380.0), 3)]);
        assert!(clip(Vec2::new(-5.0, -5.0), Vec2::new(-1.0, -9.0), view.rect).is_none());
    }
}
