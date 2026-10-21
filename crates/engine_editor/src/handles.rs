//! A brush's side handles (ADR 0110 E2b-3c; `docs/research/ux/blockout.md` §3): the active
//! brush shows a handle in the middle of each side of its size that faces the camera. A
//! drag moves that side along its normal and the opposite side stays; a room's sides are
//! its inside's, its walls following.
//!
//! - **The owner's snapping rules** ([`crate::Snapping`]): the size in whole steps of the
//!   tool bar's position step from where it was with snapping on (G), or Shift with it off;
//!   Ctrl rounds the size to the step. Free, it lands on the brushes' millimetre grid.
//! - The size never goes below a millimetre.
//! - One undo step per drag; Escape takes it back.

use crate::gizmo::{ViewProj, hit_plane};
use crate::snap::{Quantity, Snapping};
use engine_core::math::{Affine3A, Vec2, Vec3};
use engine_ecs::{Entity, Transform};

/// How near the pointer takes a handle, logical pixels.
const REACH: f32 = 8.0;

/// A side of a brush's size: its own axis (0 x, 1 its height, 2 z) and which end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Side {
    pub(crate) axis: usize,
    pub(crate) positive: bool,
}

/// A brush's six sides: +x, −x, the top, the base, +z, −z.
pub(crate) const SIDES: [Side; 6] = [
    Side {
        axis: 0,
        positive: true,
    },
    Side {
        axis: 0,
        positive: false,
    },
    Side {
        axis: 1,
        positive: true,
    },
    Side {
        axis: 1,
        positive: false,
    },
    Side {
        axis: 2,
        positive: true,
    },
    Side {
        axis: 2,
        positive: false,
    },
];

impl Side {
    /// The middle of this side of a brush of `size`, in the brush's own space: its origin
    /// is its base's middle, its height along y.
    pub(crate) fn middle(self, size: Vec3) -> Vec3 {
        let mut p = Vec3::new(0.0, size.y * 0.5, 0.0);
        p[self.axis] = match (self.axis, self.positive) {
            (1, true) => size.y,
            (1, false) => 0.0,
            (a, true) => size[a] * 0.5,
            (a, false) => -size[a] * 0.5,
        };
        p
    }

    /// Its outward direction in the brush's own space.
    pub(crate) fn normal(self) -> Vec3 {
        let mut n = Vec3::ZERO;
        n[self.axis] = if self.positive { 1.0 } else { -1.0 };
        n
    }
}

/// The active brush's handles this frame: its entity, and its sides facing the camera with
/// their places on the screen (physical pixels).
#[derive(Debug, Default)]
pub(crate) struct Handles {
    pub(crate) entity: Option<Entity>,
    pub(crate) at: Vec<(Side, Vec2)>,
}

impl Handles {
    /// The handles of brush `entity` of `size` placed at `world` (its global transform), as
    /// `view` shows them: the sides facing the camera, and those it sees edge-on.
    pub(crate) fn layout(&mut self, view: &ViewProj, entity: Entity, size: Vec3, world: Affine3A) {
        self.at.clear();
        self.entity = Some(entity);
        for side in SIDES {
            let p = world.transform_point3(side.middle(size));
            let n = world.transform_vector3(side.normal()).normalize_or_zero();
            if n.dot((view.eye - p).normalize_or_zero()) < -0.05 {
                continue;
            }
            if let Some(s) = view.screen(p) {
                self.at.push((side, s));
            }
        }
    }

    /// No handles: no brush is active.
    pub(crate) fn clear(&mut self) {
        self.at.clear();
        self.entity = None;
    }

    /// The side whose handle is under `at` (physical pixels), the nearest within reach, the
    /// UI at `scale`.
    pub(crate) fn hit(&self, at: Vec2, scale: f32) -> Option<Side> {
        self.at
            .iter()
            .map(|&(side, p)| (side, p.distance(at)))
            .filter(|&(_, d)| d <= REACH * scale)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(side, _)| side)
    }
}

/// A side being dragged.
#[derive(Debug, Clone)]
pub(crate) struct SideDrag {
    pub(crate) entity: Entity,
    pub(crate) side: Side,
    /// The brush's size and own transform when the drag began, and where it was placed.
    size: Vec3,
    local: Transform,
    world: Affine3A,
    /// The side's middle and outward direction in the world then.
    grab: Vec3,
    dir: Vec3,
    /// Its undo step; taken back (Escape).
    pub(crate) merge: u64,
    pub(crate) cancelled: bool,
    /// The size along the side's axis now, for its label.
    pub(crate) length: f32,
}

impl SideDrag {
    /// A drag of `side` of brush `entity`, of `size` with own transform `local` and placed
    /// at `world`, as undo step `merge`.
    pub(crate) fn start(
        entity: Entity,
        side: Side,
        size: Vec3,
        local: Transform,
        world: Affine3A,
        merge: u64,
    ) -> Self {
        Self {
            entity,
            side,
            size,
            local,
            world,
            grab: world.transform_point3(side.middle(size)),
            dir: world
                .transform_vector3(side.normal())
                .try_normalize()
                .unwrap_or(Vec3::Y),
            merge,
            cancelled: false,
            length: size[side.axis],
        }
    }

    /// The brush's size and own translation for the pointer at `at` (physical pixels): the
    /// side follows the pointer along its normal, on the plane holding that line that faces
    /// the camera most, snapped by the rules with Shift (`invert`) and Ctrl (`round`); the
    /// opposite side stays. None when the pointer's ray misses that plane.
    pub(crate) fn change(
        &mut self,
        view: &ViewProj,
        at: Vec2,
        snap: &Snapping,
        invert: bool,
        round: bool,
    ) -> Option<(Vec3, Vec3)> {
        let toward = (view.eye - self.grab).normalize_or_zero();
        let across = self.dir.cross(toward.cross(self.dir)).try_normalize()?;
        let free = (hit_plane(view.ray(at), self.grab, across)? - self.grab).dot(self.dir);
        // World units per unit of the brush's own along the axis: its scale and its parents'.
        let unit = self
            .world
            .transform_vector3(self.side.normal())
            .length()
            .max(1e-6);
        let a = self.side.axis;
        let start = f64::from(self.size[a]);
        let value = snap.snap(
            Quantity::Position,
            start,
            start + f64::from(free / unit),
            invert,
            round,
        );
        let length = ((value as f32 * 1000.0).round() / 1000.0).max(0.001);
        let delta = length - self.size[a];
        let mut size = self.size;
        size[a] = length;
        // The opposite side stays: the origin, the base's middle, moves half the change
        // along x or z, all of it when the base moves, none when the top does.
        let shift = match (a, self.side.positive) {
            (1, true) => Vec3::ZERO,
            (1, false) => self.side.normal() * delta,
            _ => self.side.normal() * (delta * 0.5),
        };
        let translation = self.local.translation + self.local.rotation * (self.local.scale * shift);
        self.length = length;
        Some((size, translation))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::camera::OrbitCamera;
    use engine_core::math::Quat;

    fn view() -> ViewProj {
        let camera = OrbitCamera {
            target: Vec3::new(0.0, 1.0, 0.0),
            yaw: 0.5,
            pitch: 0.5,
            distance: 12.0,
            ..OrbitCamera::default()
        };
        ViewProj::new(&camera, [0, 0, 1200, 900])
    }

    fn near(a: Vec3, b: Vec3) -> bool {
        a.distance(b) < 2e-3
    }

    #[test]
    fn sides_have_their_middles_and_normals_about_the_base() {
        let size = Vec3::new(4.0, 3.0, 2.0);
        assert_eq!(SIDES[0].middle(size), Vec3::new(2.0, 1.5, 0.0));
        assert_eq!(SIDES[3].middle(size), Vec3::ZERO, "the base");
        assert_eq!(SIDES[2].middle(size), Vec3::new(0.0, 3.0, 0.0), "the top");
        assert_eq!(SIDES[5].normal(), Vec3::NEG_Z);
    }

    #[test]
    fn the_handles_shown_face_the_camera_and_the_nearest_is_hit() {
        let view = view();
        let mut h = Handles::default();
        h.layout(&view, Entity::NULL, Vec3::splat(2.0), Affine3A::IDENTITY);
        // From above and in front (+x, +z): the top, +x and +z, not the base or −x, −z.
        let sides: Vec<Side> = h.at.iter().map(|&(s, _)| s).collect();
        assert_eq!(sides, [SIDES[0], SIDES[2], SIDES[4]]);
        let (side, at) = h.at[1];
        assert_eq!(h.hit(at + Vec2::new(3.0, 2.0), 1.0), Some(side));
        assert_eq!(h.hit(at + Vec2::new(30.0, 0.0), 1.0), None);
    }

    #[test]
    fn a_side_moves_along_its_normal_and_the_opposite_one_stays() {
        let view = view();
        let snap = Snapping::default();
        // A brush 2 m wide at (1, 0, 0), turned a quarter about y: its +x side faces −z.
        let rotation = Quat::from_rotation_y(core::f32::consts::FRAC_PI_2);
        let mut local = Transform::from_translation(Vec3::new(1.0, 0.0, 0.0));
        local.rotation = rotation;
        let world = local.to_affine();
        let size = Vec3::new(2.0, 1.0, 1.0);
        let mut d = SideDrag::start(Entity::NULL, SIDES[0], size, local, world, 1);
        // The pointer 0.5 m further out along the side's normal.
        let out = world.transform_point3(Vec3::new(1.5, 0.5, 0.0));
        let (size, at) = d
            .change(&view, view.screen(out).unwrap(), &snap, false, false)
            .unwrap();
        assert!((size.x - 2.5).abs() < 2e-3, "{size}");
        assert_eq!((size.y, size.z), (1.0, 1.0));
        // Its middle moved a quarter metre along the side's normal (−z); −x stays.
        assert!(near(at, Vec3::new(1.0, 0.0, -0.25)), "{at}");
        assert_eq!(d.length, size.x);
        // Snapping on: whole steps from where it was.
        let snap = Snapping {
            on: true,
            position: 0.5,
            ..Snapping::default()
        };
        let out = world.transform_point3(Vec3::new(1.6, 0.5, 0.0));
        let (size, _) = d
            .change(&view, view.screen(out).unwrap(), &snap, false, false)
            .unwrap();
        assert_eq!(size.x, 2.5);
    }

    #[test]
    fn the_top_keeps_the_origin_the_base_moves_it_and_a_size_stays_above_a_millimetre() {
        let view = view();
        let snap = Snapping::default();
        let local = Transform::IDENTITY;
        let size = Vec3::splat(2.0);
        let mut top = SideDrag::start(Entity::NULL, SIDES[2], size, local, Affine3A::IDENTITY, 1);
        let (s, at) = top
            .change(
                &view,
                view.screen(Vec3::new(0.0, 3.0, 0.0)).unwrap(),
                &snap,
                false,
                false,
            )
            .unwrap();
        assert!((s.y - 3.0).abs() < 2e-3 && at == Vec3::ZERO, "{s} {at}");
        let mut base = SideDrag::start(Entity::NULL, SIDES[3], size, local, Affine3A::IDENTITY, 1);
        let (s, at) = base
            .change(
                &view,
                view.screen(Vec3::new(0.0, -1.0, 0.0)).unwrap(),
                &snap,
                false,
                false,
            )
            .unwrap();
        assert!(
            (s.y - 3.0).abs() < 2e-3 && near(at, Vec3::new(0.0, -1.0, 0.0)),
            "{s} {at}"
        );
        // Dragged past the opposite side: a millimetre is left.
        let (s, _) = base
            .change(
                &view,
                view.screen(Vec3::new(0.0, 5.0, 0.0)).unwrap(),
                &snap,
                false,
                false,
            )
            .unwrap();
        assert_eq!(s.y, 0.001);
    }
}
