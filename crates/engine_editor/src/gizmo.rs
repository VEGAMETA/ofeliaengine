//! The viewport's transform gizmo (ADR 0107 E2; `docs/research/ux/viewport-tools.md` §2,
//! §3): handles drawn over the scene at the selection's pivot, found under the pointer in
//! screen space, and what a drag of one does: a move, a turn or a scale in world space,
//! with the owner's snapping rules ([`crate::Snapping`]).
//!
//! - **Move:** an arrow per axis, a square per plane, a dot in the middle (the view's
//!   plane).
//! - **Rotate:** a ring per axis, its half that faces the camera.
//! - **Scale:** a line and a box per axis, a box in the middle for all three.
//! - **All** (the select tool): the arrows, the squares and the rings, the scale boxes
//!   halfway along the arrows.
//!
//! The handles keep their size on the screen ([`SIZE`] pixels) at any distance.

use crate::camera::OrbitCamera;
use crate::snap::{Quantity, Snapping};
use engine_core::math::{Affine3A, Mat4, Quat, Vec2, Vec3, Vec4};
use engine_ecs::{Entity, Transform};

/// The handles' length on the screen, in logical pixels.
pub(crate) const SIZE: f32 = 110.0;

/// How near the pointer must be to a handle to take it, in logical pixels.
const REACH: f32 = 7.0;

/// Segments of a rotation ring.
const RING: usize = 64;

/// Which handles the gizmo has: the tool bar's tool's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    All,
    Move,
    Rotate,
    Scale,
}

impl Mode {
    /// The gizmo of the tool bar's tool (0 select, 1 move, 2 rotate, 3 scale).
    pub(crate) fn of_tool(tool: usize) -> Self {
        match tool {
            1 => Self::Move,
            2 => Self::Rotate,
            3 => Self::Scale,
            _ => Self::All,
        }
    }

    fn moves(self) -> bool {
        matches!(self, Self::All | Self::Move)
    }

    fn turns(self) -> bool {
        matches!(self, Self::All | Self::Rotate)
    }

    fn scales(self) -> bool {
        matches!(self, Self::All | Self::Scale)
    }
}

/// A handle of the gizmo; axes are 0 (x), 1 (y), 2 (z) of its axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handle {
    /// Moves along an axis.
    Axis(usize),
    /// Moves in the plane across an axis.
    Plane(usize),
    /// Moves in the view's plane.
    View,
    /// Turns about an axis.
    Ring(usize),
    /// Scales along an axis.
    Stretch(usize),
    /// Scales along all three.
    Grow,
}

impl Handle {
    /// Its axis' color index, none for the view's and the uniform handles.
    pub(crate) fn axis(self) -> Option<usize> {
        match self {
            Self::Axis(i) | Self::Plane(i) | Self::Ring(i) | Self::Stretch(i) => Some(i),
            Self::View | Self::Grow => None,
        }
    }

    /// Whether it moves the selection (an arrow, a square, the middle).
    pub(crate) fn moves(self) -> bool {
        matches!(self, Self::Axis(_) | Self::Plane(_) | Self::View)
    }

    /// The undo step's name.
    pub(crate) fn step(self) -> &'static str {
        match self {
            Self::Axis(_) | Self::Plane(_) | Self::View => "Move",
            Self::Ring(_) => "Rotate",
            Self::Stretch(_) | Self::Grow => "Scale",
        }
    }
}

/// World and screen for the editor's camera and the viewport's rectangle (physical
/// pixels): the projection the renderer draws with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewProj {
    clip: Mat4,
    world: Mat4,
    /// The viewport: x, y, width, height.
    pub(crate) rect: [f32; 4],
    pub(crate) eye: Vec3,
    pub(crate) forward: Vec3,
    /// 2 tan(fov / 2) / height: world units per pixel at a depth of 1.
    spread: f32,
    /// A 2D view's world units per pixel, the same at every depth.
    flat: Option<f32>,
}

impl ViewProj {
    pub(crate) fn new(camera: &OrbitCamera, rect: [u32; 4]) -> Self {
        let r = rect.map(|v| v as f32);
        let clip = camera.clip_from_world(r[2] / r[3].max(1.0));
        let eye = camera.eye();
        Self {
            clip,
            world: clip.inverse(),
            rect: r,
            eye,
            forward: (camera.target - eye).normalize_or_zero(),
            spread: 2.0 * (camera.fov_y * 0.5).tan() / r[3].max(1.0),
            flat: camera.flat.map(|height| height / r[3].max(1.0)),
        }
    }

    /// Where `p` is on the screen; none behind the camera.
    pub(crate) fn screen(&self, p: Vec3) -> Option<Vec2> {
        let c = self.clip * p.extend(1.0);
        if c.w <= 1e-6 {
            return None;
        }
        Some(Vec2::new(
            self.rect[0] + (c.x / c.w * 0.5 + 0.5) * self.rect[2],
            self.rect[1] + (c.y / c.w * 0.5 + 0.5) * self.rect[3],
        ))
    }

    /// The ray from the eye through pixel `at`: its origin and direction.
    pub(crate) fn ray(&self, at: Vec2) -> (Vec3, Vec3) {
        let x = (at.x - self.rect[0]) / self.rect[2].max(1.0) * 2.0 - 1.0;
        let y = (at.y - self.rect[1]) / self.rect[3].max(1.0) * 2.0 - 1.0;
        let near = self.world * Vec4::new(x, y, 0.0, 1.0);
        let far = self.world * Vec4::new(x, y, 1.0, 1.0);
        let near = near.truncate() / near.w;
        let far = far.truncate() / far.w;
        (near, (far - near).normalize_or_zero())
    }

    /// World units a pixel spans at `p`'s depth.
    pub(crate) fn per_pixel(&self, p: Vec3) -> f32 {
        if let Some(flat) = self.flat {
            return flat;
        }
        (p - self.eye).dot(self.forward).max(1e-4) * self.spread
    }

    /// Whether `p` is inside the viewport.
    pub(crate) fn contains(&self, p: Vec2) -> bool {
        p.x >= self.rect[0]
            && p.y >= self.rect[1]
            && p.x < self.rect[0] + self.rect[2]
            && p.y < self.rect[1] + self.rect[3]
    }
}

/// Where a ray meets the plane through `point` across `normal`; none when it runs along
/// the plane or the plane is behind it.
pub(crate) fn hit_plane(ray: (Vec3, Vec3), point: Vec3, normal: Vec3) -> Option<Vec3> {
    let d = ray.1.dot(normal);
    if d.abs() < 1e-6 {
        return None;
    }
    let t = (point - ray.0).dot(normal) / d;
    (t >= 0.0).then(|| ray.0 + ray.1 * t)
}

/// The distance from `p` to the segment `a`–`b`.
fn to_segment(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    (a + ab * t).distance(p)
}

/// Whether `p` is inside the convex quad `q`.
fn in_quad(p: Vec2, q: &[Vec2; 4]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        let cross = (b - a).perp_dot(p - a);
        if cross != 0.0 {
            if sign != 0.0 && cross.signum() != sign {
                return false;
            }
            sign = cross.signum();
        }
    }
    true
}

/// The gizmo this frame: where it is and how its handles lie on the screen.
#[derive(Debug, Default)]
pub(crate) struct Gizmo {
    pub(crate) mode: Mode,
    pub(crate) pivot: Vec3,
    pub(crate) axes: [Vec3; 3],
    /// The handles' world length here.
    pub(crate) length: f32,
    /// The pivot on the screen.
    pub(crate) center: Vec2,
    /// Lines, each a handle's piece: arrows, rings, scale lines.
    pub(crate) lines: Vec<(Handle, Vec2, Vec2)>,
    /// Arrow tips: a handle, the tip and the arrow's direction on the screen.
    pub(crate) tips: Vec<(Handle, Vec2, Vec2)>,
    /// Boxes and dots: a handle, the center and the half size.
    pub(crate) boxes: Vec<(Handle, Vec2, f32)>,
    /// The planes' squares, as their corners on the screen.
    pub(crate) quads: Vec<(Handle, [Vec2; 4])>,
}

impl Gizmo {
    /// Lays the gizmo out at `pivot` along `axes` for `view`, the UI at `scale` (physical
    /// pixels per logical pixel). False when the pivot is behind the camera.
    pub(crate) fn layout(
        &mut self,
        view: &ViewProj,
        mode: Mode,
        pivot: Vec3,
        axes: [Vec3; 3],
        scale: f32,
    ) -> bool {
        self.lines.clear();
        self.tips.clear();
        self.boxes.clear();
        self.quads.clear();
        let Some(center) = view.screen(pivot) else {
            return false;
        };
        self.mode = mode;
        self.pivot = pivot;
        self.axes = axes;
        self.center = center;
        let length = view.per_pixel(pivot) * SIZE * scale;
        self.length = length;
        let at = |p: Vec3| view.screen(p);
        // An axis that points at the camera: a dot over the middle, not a handle.
        let seen = |axis: Vec3| {
            at(pivot + axis * length).is_some_and(|tip| tip.distance(center) >= 0.25 * SIZE * scale)
        };
        if mode.moves() {
            for (i, &axis) in axes.iter().enumerate() {
                let (Some(from), Some(tip)) =
                    (at(pivot + axis * length * 0.15), at(pivot + axis * length))
                else {
                    continue;
                };
                // Its square stays: across an axis that faces the camera is the plane in view.
                if seen(axis) {
                    self.lines.push((Handle::Axis(i), from, tip));
                    self.tips
                        .push((Handle::Axis(i), tip, (tip - from).normalize_or_zero()));
                }
                // The square across this axis, between the other two.
                let (a, b) = (axes[(i + 1) % 3], axes[(i + 2) % 3]);
                let corners = [(0.2, 0.2), (0.4, 0.2), (0.4, 0.4), (0.2, 0.4)]
                    .map(|(u, v)| at(pivot + (a * u + b * v) * length));
                // Not one seen edge-on: a sliver along an arrow.
                if let [Some(c0), Some(c1), Some(c2), Some(c3)] = corners
                    && ((c2 - c0).perp_dot(c3 - c1)).abs() * 0.5 >= 30.0 * scale * scale
                {
                    self.quads.push((Handle::Plane(i), [c0, c1, c2, c3]));
                }
            }
            self.boxes.push((Handle::View, center, 5.0 * scale));
        }
        if mode.turns() {
            let radius = length * if mode == Mode::All { 1.08 } else { 0.9 };
            let back = (view.eye - pivot).normalize_or_zero();
            for (i, &axis) in axes.iter().enumerate() {
                let (a, b) = (axes[(i + 1) % 3], axes[(i + 2) % 3]);
                // Not one seen edge-on: a line across the gizmo, its turn unstable.
                if axis.dot(back).abs() < 0.15 {
                    continue;
                }
                let point = |k: usize| {
                    let t = k as f32 / RING as f32 * core::f32::consts::TAU;
                    a * t.cos() + b * t.sin()
                };
                for k in 0..RING {
                    let (u, v) = (point(k), point(k + 1));
                    // The half facing the camera (all of it when the ring faces it).
                    if (u + v).dot(back) < -0.05 {
                        continue;
                    }
                    if let (Some(p), Some(q)) = (at(pivot + u * radius), at(pivot + v * radius)) {
                        self.lines.push((Handle::Ring(i), p, q));
                    }
                }
            }
        }
        if mode.scales() {
            let reach = if mode == Mode::All { 0.6 } else { 1.0 };
            for (i, &axis) in axes.iter().enumerate() {
                let Some(end) = at(pivot + axis * length * reach) else {
                    continue;
                };
                if !seen(axis) {
                    continue;
                }
                if mode == Mode::Scale
                    && let Some(from) = at(pivot + axis * length * 0.15)
                {
                    self.lines.push((Handle::Stretch(i), from, end));
                }
                self.boxes.push((Handle::Stretch(i), end, 4.5 * scale));
            }
            if mode == Mode::Scale {
                self.boxes.push((Handle::Grow, center, 6.0 * scale));
            }
        }
        true
    }

    /// The handle under `at` (physical pixels), the UI at `scale`: boxes and squares
    /// before lines, the nearest within reach.
    pub(crate) fn hit(&self, at: Vec2, scale: f32) -> Option<Handle> {
        let reach = REACH * scale;
        let boxed = self
            .boxes
            .iter()
            .filter(|(_, c, half)| (at - *c).abs().max_element() <= half + 2.0 * scale)
            .min_by(|x, y| x.1.distance(at).total_cmp(&y.1.distance(at)))
            .map(|(h, _, _)| *h);
        if boxed.is_some() {
            return boxed;
        }
        if let Some((h, _)) = self.quads.iter().find(|(_, q)| in_quad(at, q)) {
            return Some(*h);
        }
        self.lines
            .iter()
            .map(|(h, a, b)| (*h, to_segment(at, *a, *b)))
            .chain(self.tips.iter().map(|(h, tip, _)| (*h, tip.distance(at))))
            .filter(|(_, d)| *d <= reach)
            .min_by(|x, y| x.1.total_cmp(&y.1))
            .map(|(h, _)| h)
    }
}

/// An entity a drag moves: its world transform at the press, and its parent's inverse
/// (world to the parent's space).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Target {
    pub(crate) entity: Entity,
    pub(crate) world: Affine3A,
    pub(crate) parent_inverse: Affine3A,
    /// Its own transform at the press: what a move or a turn keeps exactly.
    pub(crate) local: Transform,
}

/// A drag of a handle.
#[derive(Debug, Clone)]
pub(crate) struct GizmoDrag {
    pub(crate) handle: Handle,
    pub(crate) pivot: Vec3,
    pub(crate) axes: [Vec3; 3],
    /// Where the pointer's ray met the drag's plane at the press, and the press.
    pub(crate) grab: Vec3,
    pub(crate) press: Vec2,
    /// The plane the drag moves in, across this normal through the pivot.
    pub(crate) normal: Vec3,
    pub(crate) targets: Vec<Target>,
    /// The drag's undo step: one per gesture.
    pub(crate) merge: u64,
    /// Escape took it back.
    pub(crate) cancelled: bool,
    /// What it did last: the status bar shows it, and the gizmo follows a move.
    pub(crate) last: Option<Change>,
    /// The selection's world box at the press, around the pivot (its corners less the
    /// pivot): placing it on a surface puts the box's face on it.
    pub(crate) extent: Option<(Vec3, Vec3)>,
}

/// What a drag does to the selection, in world space about the pivot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Change {
    Move(Vec3),
    Turn(Quat),
    /// Factors along the gizmo's axes.
    Scale(Vec3),
}

impl GizmoDrag {
    /// Where the pivot goes to put the selection on a surface at `point` facing `normal`:
    /// the selection's box (its extent at the press) touching the surface from the
    /// normal's side; the pivot itself on it without a box.
    pub(crate) fn on_surface(&self, point: Vec3, normal: Vec3) -> Vec3 {
        let Some((lo, hi)) = self.extent else {
            return point;
        };
        // How far the box reaches behind the pivot along the normal.
        let behind = (0..3)
            .map(|k| (-lo[k] * normal[k]).max(-hi[k] * normal[k]))
            .sum::<f32>();
        point + normal * behind
    }

    /// A drag of `handle` from pixel `press`; none when its plane cannot be found from
    /// there (it runs along the view).
    pub(crate) fn start(
        gizmo: &Gizmo,
        view: &ViewProj,
        handle: Handle,
        press: Vec2,
        targets: Vec<Target>,
        merge: u64,
    ) -> Option<Self> {
        let axes = gizmo.axes;
        let toward = (view.eye - gizmo.pivot).normalize_or_zero();
        let normal = match handle {
            // The plane holding the axis that faces the camera most.
            Handle::Axis(i) | Handle::Stretch(i) => {
                let a = axes[i];
                let n = a.cross(toward.cross(a)).normalize_or_zero();
                if n == Vec3::ZERO { toward } else { n }
            }
            Handle::Plane(i) | Handle::Ring(i) => axes[i],
            Handle::View | Handle::Grow => toward,
        };
        let grab = hit_plane(view.ray(press), gizmo.pivot, normal)?;
        Some(Self {
            handle,
            pivot: gizmo.pivot,
            axes,
            grab,
            press,
            normal,
            targets,
            merge,
            cancelled: false,
            last: None,
            extent: None,
        })
    }

    /// What the drag does with the pointer at `at`, snapped by the rules with Shift
    /// (`invert`) and Ctrl (`round`) as held; none when the ray misses the drag's plane.
    pub(crate) fn change(
        &self,
        view: &ViewProj,
        at: Vec2,
        snap: &Snapping,
        invert: bool,
        round: bool,
    ) -> Option<Change> {
        let snapped = |q: Quantity, start: f32, free: f32| {
            snap.snap(q, f64::from(start), f64::from(free), invert, round) as f32
        };
        // A move along `axis` by `d`: the pivot's coordinate on it snapped.
        let along = |axis: Vec3, d: f32| {
            let start = self.pivot.dot(axis);
            axis * (snapped(Quantity::Position, start, start + d) - start)
        };
        match self.handle {
            Handle::Axis(i) => {
                let p = hit_plane(view.ray(at), self.pivot, self.normal)?;
                Some(Change::Move(along(
                    self.axes[i],
                    (p - self.grab).dot(self.axes[i]),
                )))
            }
            Handle::Plane(_) | Handle::View => {
                let p = hit_plane(view.ray(at), self.pivot, self.normal)?;
                let v = p - self.grab;
                let mut delta = Vec3::ZERO;
                for (j, &axis) in self.axes.iter().enumerate() {
                    if self.handle == Handle::Plane(j) {
                        continue;
                    }
                    delta += along(axis, v.dot(axis));
                }
                Some(Change::Move(delta))
            }
            Handle::Ring(i) => {
                let axis = self.axes[i];
                let p = hit_plane(view.ray(at), self.pivot, self.normal)?;
                let (v0, v1) = (self.grab - self.pivot, p - self.pivot);
                let degrees = axis.dot(v0.cross(v1)).atan2(v0.dot(v1)).to_degrees();
                let degrees = snapped(Quantity::Angle, 0.0, degrees);
                Some(Change::Turn(Quat::from_axis_angle(
                    axis,
                    degrees.to_radians(),
                )))
            }
            Handle::Stretch(i) => {
                let axis = self.axes[i];
                let p = hit_plane(view.ray(at), self.pivot, self.normal)?;
                let from = (self.grab - self.pivot).dot(axis);
                if from.abs() < 1e-6 {
                    return None;
                }
                let factor = snapped(Quantity::Scale, 1.0, (p - self.pivot).dot(axis) / from);
                let mut f = Vec3::ONE;
                f[i] = factor;
                Some(Change::Scale(f))
            }
            Handle::Grow => {
                let center = view.screen(self.pivot)?;
                let from = (self.press - center).length();
                if from < 1.0 {
                    return None;
                }
                let factor = snapped(Quantity::Scale, 1.0, (at - center).length() / from);
                Some(Change::Scale(Vec3::splat(factor)))
            }
        }
    }
}

/// A target's new world transform under `change` about `pivot`, a scale's factors along
/// `axes`: the gizmo's and the keyboard's transforms alike.
pub(crate) fn apply(change: Change, target: &Target, pivot: Vec3, axes: [Vec3; 3]) -> Affine3A {
    let (scale, rotation, translation) = target.world.to_scale_rotation_translation();
    match change {
        Change::Move(d) => {
            Affine3A::from_scale_rotation_translation(scale, rotation, translation + d)
        }
        Change::Turn(q) => Affine3A::from_scale_rotation_translation(
            scale,
            (q * rotation).normalize(),
            pivot + q * (translation - pivot),
        ),
        Change::Scale(f) => {
            // The offset from the pivot scaled along the axes; the entity's own scale along
            // its local axis nearest each of them.
            let offset = translation - pivot;
            let mut moved = Vec3::ZERO;
            for (i, &axis) in axes.iter().enumerate() {
                moved += axis * offset.dot(axis) * f[i];
            }
            Affine3A::from_scale_rotation_translation(
                scale * own_factors(f, rotation, axes),
                rotation,
                pivot + moved,
            )
        }
    }
}

/// A scale's factors `f` along `axes` as factors along an entity's own axes (`rotation`
/// turns them in the world): each factor goes to the entity's axis nearest its axis.
pub(crate) fn own_factors(f: Vec3, rotation: Quat, axes: [Vec3; 3]) -> Vec3 {
    let local = [rotation * Vec3::X, rotation * Vec3::Y, rotation * Vec3::Z];
    let mut own = Vec3::ONE;
    for (i, &axis) in axes.iter().enumerate() {
        let k = (0..3)
            .max_by(|&a, &b| {
                local[a]
                    .dot(axis)
                    .abs()
                    .total_cmp(&local[b].dot(axis).abs())
            })
            .unwrap_or(i);
        own[k] *= f[i];
    }
    own
}

/// The gizmo's colors: x, y, z, the view's and uniform handles, and a handle under the
/// pointer or being dragged.
pub(crate) const COLORS: [Vec4; 5] = [
    Vec4::new(0.93, 0.27, 0.3, 1.0),
    Vec4::new(0.45, 0.85, 0.3, 1.0),
    Vec4::new(0.3, 0.55, 0.98, 1.0),
    Vec4::new(0.92, 0.92, 0.92, 1.0),
    Vec4::new(1.0, 0.82, 0.2, 1.0),
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    #[test]
    fn on_a_surface_the_box_touches_it_from_the_normals_side() {
        let mut drag = GizmoDrag {
            handle: Handle::View,
            pivot: Vec3::new(1.0, 5.0, 0.0),
            axes: [Vec3::X, Vec3::Y, Vec3::Z],
            grab: Vec3::ZERO,
            press: Vec2::ZERO,
            normal: Vec3::Z,
            targets: Vec::new(),
            merge: 1,
            cancelled: false,
            last: None,
            extent: None,
        };
        let point = Vec3::new(3.0, 0.0, 2.0);
        assert_eq!(
            drag.on_surface(point, Vec3::Y),
            point,
            "no box: the pivot on it"
        );
        // A box from 0.25 below the pivot to 1.75 above, 0.5 to each side.
        drag.extent = Some((Vec3::new(-0.5, -0.25, -0.5), Vec3::new(0.5, 1.75, 0.5)));
        assert_eq!(
            drag.on_surface(point, Vec3::Y),
            point + Vec3::Y * 0.25,
            "on a floor"
        );
        assert_eq!(
            drag.on_surface(point, Vec3::NEG_Y),
            point - Vec3::Y * 1.75,
            "under a ceiling"
        );
        assert_eq!(
            drag.on_surface(point, Vec3::X),
            point + Vec3::X * 0.5,
            "on a wall"
        );
        assert!(Handle::View.moves() && Handle::Axis(1).moves() && !Handle::Ring(0).moves());
    }

    fn camera() -> OrbitCamera {
        // Looking at the origin from +Z, level: x to the right, y up on the screen.
        OrbitCamera {
            target: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            distance: 10.0,
            ..OrbitCamera::default()
        }
    }

    fn view() -> ViewProj {
        ViewProj::new(&camera(), [0, 0, 1000, 800])
    }

    #[test]
    fn the_view_projects_and_casts_rays_through_the_same_pixels() {
        let view = view();
        let center = view.screen(Vec3::ZERO).unwrap();
        assert!(
            (center - Vec2::new(500.0, 400.0)).length() < 1e-3,
            "{center}"
        );
        // +x right, +y up on the screen.
        let right = view.screen(Vec3::X).unwrap();
        let up = view.screen(Vec3::Y).unwrap();
        assert!(right.x > center.x && (right.y - center.y).abs() < 1e-3);
        assert!(up.y < center.y);
        // A ray through a point's pixel meets it.
        let p = Vec3::new(1.5, -0.5, 2.0);
        let (o, d) = view.ray(view.screen(p).unwrap());
        let along = (p - o).dot(d);
        assert!((o + d * along).distance(p) < 1e-3);
        assert!(
            view.screen(Vec3::new(0.0, 0.0, 20.0)).is_none(),
            "behind the eye"
        );
    }

    #[test]
    fn handles_are_found_where_they_are_drawn() {
        let view = view();
        let mut gizmo = Gizmo::default();
        assert!(gizmo.layout(
            &view,
            Mode::All,
            Vec3::ZERO,
            [Vec3::X, Vec3::Y, Vec3::Z],
            1.0
        ));
        // About 110 pixels long whatever the distance.
        let tip = view.screen(Vec3::X * gizmo.length).unwrap();
        assert!((tip.x - 500.0 - SIZE).abs() < 1.0, "{tip}");
        assert_eq!(
            gizmo.hit(Vec2::new(580.0, 400.0), 1.0),
            Some(Handle::Axis(0))
        );
        assert_eq!(
            gizmo.hit(Vec2::new(500.0, 320.0), 1.0),
            Some(Handle::Axis(1))
        );
        assert_eq!(gizmo.hit(Vec2::new(500.0, 400.0), 1.0), Some(Handle::View));
        // The square across z sits between x and y.
        assert_eq!(
            gizmo.hit(Vec2::new(533.0, 367.0), 1.0),
            Some(Handle::Plane(2))
        );
        // The ring about z faces the camera: a full circle around the pivot.
        let r = SIZE * 1.08;
        assert_eq!(
            gizmo.hit(
                Vec2::new(
                    500.0 - r * core::f32::consts::FRAC_1_SQRT_2,
                    400.0 + r * core::f32::consts::FRAC_1_SQRT_2
                ),
                1.0
            ),
            Some(Handle::Ring(2))
        );
        assert_eq!(gizmo.hit(Vec2::new(900.0, 100.0), 1.0), None);
        // The scale tool: boxes at the ends, one in the middle.
        gizmo.layout(
            &view,
            Mode::Scale,
            Vec3::ZERO,
            [Vec3::X, Vec3::Y, Vec3::Z],
            1.0,
        );
        assert_eq!(
            gizmo.hit(Vec2::new(500.0 + SIZE, 400.0), 1.0),
            Some(Handle::Stretch(0))
        );
        assert_eq!(gizmo.hit(Vec2::new(500.0, 400.0), 1.0), Some(Handle::Grow));
    }

    fn drag(handle: Handle, press: Vec2) -> (GizmoDrag, ViewProj) {
        let view = view();
        let mut gizmo = Gizmo::default();
        gizmo.layout(
            &view,
            Mode::All,
            Vec3::ZERO,
            [Vec3::X, Vec3::Y, Vec3::Z],
            1.0,
        );
        let target = Target {
            entity: Entity::NULL,
            world: Affine3A::from_translation(Vec3::new(0.0, 0.0, 0.0)),
            parent_inverse: Affine3A::IDENTITY,
            local: Transform::IDENTITY,
        };
        (
            GizmoDrag::start(&gizmo, &view, handle, press, vec![target], 1).unwrap(),
            view,
        )
    }

    #[test]
    fn an_arrow_moves_along_its_axis_with_the_snapping_rules() {
        let (d, view) = drag(Handle::Axis(0), Vec2::new(580.0, 400.0));
        let snap = Snapping::default();
        // 37 pixels to the right and some upward: along x only.
        let at = Vec2::new(617.0, 380.0);
        let Some(Change::Move(free)) = d.change(&view, at, &snap, false, false) else {
            panic!("a move");
        };
        let per_pixel = view.per_pixel(Vec3::ZERO);
        assert!((free.x - 37.0 * per_pixel).abs() < 1e-3, "{free}");
        assert_eq!((free.y, free.z), (0.0, 0.0));
        // Shift: whole steps of 0.1 from the start; Ctrl: the pivot on the grid.
        let Some(Change::Move(stepped)) = d.change(&view, at, &snap, true, false) else {
            panic!("a move");
        };
        assert!((stepped.x / 0.1 - (stepped.x / 0.1).round()).abs() < 1e-4);
        let Some(Change::Move(rounded)) = d.change(&view, at, &snap, false, true) else {
            panic!("a move");
        };
        assert_eq!(rounded.x, stepped.x, "the start is on the grid");
    }

    #[test]
    fn a_ring_turns_about_its_axis_in_45_degree_steps_with_shift() {
        // The ring about z, grabbed on its right: dragged up a quarter turn.
        let r = SIZE * 1.08;
        let (d, view) = drag(Handle::Ring(2), Vec2::new(500.0 + r, 400.0));
        let snap = Snapping::default();
        let Some(Change::Turn(q)) =
            d.change(&view, Vec2::new(500.0, 400.0 - r), &snap, false, false)
        else {
            panic!("a turn");
        };
        assert!((q * Vec3::X - Vec3::Y).length() < 1e-3, "{q}");
        let Some(Change::Turn(q)) = d.change(
            &view,
            Vec2::new(500.0 + r * 0.8, 400.0 - r * 0.6),
            &snap,
            true,
            false,
        ) else {
            panic!("a turn");
        };
        let angle = (q * Vec3::X).y.atan2((q * Vec3::X).x).to_degrees();
        assert!((angle - 45.0).abs() < 1e-3, "{angle}");
        // A turn about the pivot carries an entity around it.
        let target = Target {
            entity: Entity::NULL,
            world: Affine3A::from_translation(Vec3::new(2.0, 0.0, 0.0)),
            parent_inverse: Affine3A::IDENTITY,
            local: Transform::IDENTITY,
        };
        let quarter = Quat::from_rotation_z(core::f32::consts::FRAC_PI_2);
        let moved = apply(Change::Turn(quarter), &target, d.pivot, d.axes);
        assert!((Vec3::from(moved.translation) - Vec3::new(0.0, 2.0, 0.0)).length() < 1e-4);
    }

    #[test]
    fn scaling_along_an_axis_scales_the_entitys_nearest_axis() {
        let (d, _) = drag(Handle::Stretch(0), Vec2::new(566.0, 400.0));
        // An entity turned a quarter about y: its local z lies along world x.
        let target = Target {
            entity: Entity::NULL,
            world: Affine3A::from_rotation_y(core::f32::consts::FRAC_PI_2),
            parent_inverse: Affine3A::IDENTITY,
            local: Transform::IDENTITY,
        };
        let scaled = apply(
            Change::Scale(Vec3::new(2.0, 1.0, 1.0)),
            &target,
            d.pivot,
            d.axes,
        );
        let (scale, _, _) = scaled.to_scale_rotation_translation();
        assert!(
            (scale - Vec3::new(1.0, 1.0, 2.0)).length() < 1e-4,
            "{scale}"
        );
    }
}
