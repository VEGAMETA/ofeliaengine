//! A selected sound's and reverb zone's handles (ADR 0107 E11-4b;
//! `docs/research/ux/sound.md`'s gizmos): a square on each of an `AudioSource3D`'s two rings,
//! on the side toward the camera's right, and a `ReverbZone`'s box sides with one more on top
//! of its fade.
//!
//! - **A ring's radius** follows the pointer's distance from the source in the plane facing
//!   the camera, by the owner's snapping rules (the tool bar's position step); the inner ring
//!   pushes the outer one along, and the outer one stops at the inner.
//! - **A zone's side** moves along its normal and the opposite side stays (the entity moves
//!   by half the change), as a brush's does; the fade's handle sets how far out the room
//!   fades.
//! - One undo step a drag; Escape takes it back.

use crate::gizmo::{ViewProj, hit_plane};
use crate::handles::{SIDES, Side};
use crate::snap::{Quantity, Snapping};
use engine_core::math::{Affine3A, Vec2, Vec3};
use engine_ecs::{Entity, Transform};

/// How near the pointer takes a handle, logical pixels.
const REACH: f32 = 8.0;
/// The least a radius, a zone's size or a fade goes to, in metres.
const LEAST: f32 = 0.01;

/// A handle of a sound or a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SoundHandle {
    /// An `AudioSource3D`'s full-volume ring (`min_distance`).
    Near,
    /// Its falloff's end (`max_distance`).
    Far,
    /// A `ReverbZone`'s side.
    Side(Side),
    /// The zone's fade, on its top.
    Fade,
}

/// What the handles are on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Shape {
    /// A sound's rings about `at`.
    Rings { at: Vec3, near: f32, far: f32 },
    /// A zone's box placed at `world` (its entity's), `half` its half size in the entity's
    /// units, `fade` metres out.
    Zone {
        world: Affine3A,
        half: Vec3,
        fade: f32,
    },
}

/// The world's direction toward the camera's right at `at`, level, as the rings are drawn.
fn across(view: &ViewProj, at: Vec3) -> Vec3 {
    let facing = (at - view.eye).normalize_or(view.forward);
    facing.cross(Vec3::Y).normalize_or(Vec3::X)
}

/// A zone's side's middle in its entity's space: the box is centered on the entity.
fn side_middle(side: Side, half: Vec3) -> Vec3 {
    let mut p = Vec3::ZERO;
    p[side.axis] = if side.positive {
        half[side.axis]
    } else {
        -half[side.axis]
    };
    p
}

/// A zone's world up and its half size in metres along its world axes.
fn zone_frame(world: Affine3A, half: Vec3) -> (Vec3, Vec3) {
    let m = world.matrix3;
    let lengths = Vec3::new(
        Vec3::from(m.x_axis).length(),
        Vec3::from(m.y_axis).length(),
        Vec3::from(m.z_axis).length(),
    );
    let up = Vec3::from(m.y_axis).normalize_or(Vec3::Y);
    (up, half.abs() * lengths)
}

/// Where the fade's handle is: on top of the fade's box.
fn fade_point(world: Affine3A, half: Vec3, fade: f32) -> Vec3 {
    let (up, half) = zone_frame(world, half);
    Vec3::from(world.translation) + up * (half.y + fade.max(0.0))
}

/// The selected sound's or zone's handles this frame: its entity, and each handle with its
/// place on the screen (physical pixels).
#[derive(Debug, Default)]
pub(crate) struct SoundHandles {
    pub(crate) entity: Option<Entity>,
    pub(crate) at: Vec<(SoundHandle, Vec2)>,
}

impl SoundHandles {
    /// The handles of `entity`'s `shape` as `view` shows them: a ring's on its right; a
    /// zone's sides facing the camera (or seen edge-on), and its fade's on top.
    pub(crate) fn layout(&mut self, view: &ViewProj, entity: Entity, shape: Shape) {
        self.at.clear();
        self.entity = Some(entity);
        match shape {
            Shape::Rings { at, near, far } => {
                let right = across(view, at);
                for (handle, radius) in [(SoundHandle::Near, near), (SoundHandle::Far, far)] {
                    if let Some(s) = view.screen(at + right * radius) {
                        self.at.push((handle, s));
                    }
                }
            }
            Shape::Zone { world, half, fade } => {
                for side in SIDES {
                    let p = world.transform_point3(side_middle(side, half));
                    let n = world.transform_vector3(side.normal()).normalize_or_zero();
                    if n.dot((view.eye - p).normalize_or_zero()) < -0.05 {
                        continue;
                    }
                    if let Some(s) = view.screen(p) {
                        self.at.push((SoundHandle::Side(side), s));
                    }
                }
                if let Some(s) = view.screen(fade_point(world, half, fade)) {
                    self.at.push((SoundHandle::Fade, s));
                }
            }
        }
    }

    /// No handles: nothing with them is active.
    pub(crate) fn clear(&mut self) {
        self.at.clear();
        self.entity = None;
    }

    /// The handle under `at` (physical pixels), the nearest within reach, the UI at
    /// `scale`.
    pub(crate) fn hit(&self, at: Vec2, scale: f32) -> Option<SoundHandle> {
        self.at
            .iter()
            .map(|&(h, p)| (h, p.distance(at)))
            .filter(|&(_, d)| d <= REACH * scale)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(h, _)| h)
    }
}

/// What a drag gives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SoundChange {
    /// A sound's two distances.
    Ranges { near: f32, far: f32 },
    /// A zone's half size and its entity's own translation.
    Zone { half: Vec3, translation: Vec3 },
    /// A zone's fade.
    Fade(f32),
}

/// A handle being dragged.
#[derive(Debug, Clone)]
pub(crate) struct SoundDrag {
    pub(crate) entity: Entity,
    pub(crate) handle: SoundHandle,
    shape: Shape,
    /// The entity's own transform when the drag began (a zone's side moves it).
    local: Transform,
    /// Where the handle was, and the way it moves (a zone's side's normal, its fade's up).
    grab: Vec3,
    dir: Vec3,
    /// Its undo step; taken back (Escape).
    pub(crate) merge: u64,
    pub(crate) cancelled: bool,
    /// The value now, in metres, for its label.
    pub(crate) value: f32,
}

impl SoundDrag {
    /// A drag of `handle` on `entity`'s `shape`, its own transform `local`, as `view`
    /// shows it, as undo step `merge`.
    pub(crate) fn start(
        view: &ViewProj,
        entity: Entity,
        handle: SoundHandle,
        shape: Shape,
        local: Transform,
        merge: u64,
    ) -> Self {
        let (grab, dir, value) = match (handle, shape) {
            (SoundHandle::Near | SoundHandle::Far, Shape::Rings { at, near, far }) => {
                let r = if handle == SoundHandle::Near {
                    near
                } else {
                    far
                };
                (at + across(view, at) * r, across(view, at), r)
            }
            (SoundHandle::Side(side), Shape::Zone { world, half, .. }) => (
                world.transform_point3(side_middle(side, half)),
                world
                    .transform_vector3(side.normal())
                    .try_normalize()
                    .unwrap_or(Vec3::Y),
                half[side.axis].abs() * 2.0,
            ),
            (SoundHandle::Fade, Shape::Zone { world, half, fade }) => (
                fade_point(world, half, fade),
                zone_frame(world, half).0,
                fade,
            ),
            _ => (Vec3::ZERO, Vec3::Y, 0.0),
        };
        Self {
            entity,
            handle,
            shape,
            local,
            grab,
            dir,
            merge,
            cancelled: false,
            value,
        }
    }

    /// What the handle gives for the pointer at `at` (physical pixels), snapped by the
    /// rules with Shift (`invert`) and Ctrl (`round`); None when its ray misses the plane.
    pub(crate) fn change(
        &mut self,
        view: &ViewProj,
        at: Vec2,
        snap: &Snapping,
        invert: bool,
        round: bool,
    ) -> Option<SoundChange> {
        let snapped = |start: f32, free: f32| {
            let v = snap.snap(
                Quantity::Position,
                f64::from(start),
                f64::from(free),
                invert,
                round,
            ) as f32;
            ((v * 1000.0).round() / 1000.0).max(LEAST)
        };
        match (self.handle, self.shape) {
            (
                SoundHandle::Near | SoundHandle::Far,
                Shape::Rings {
                    at: center,
                    near,
                    far,
                },
            ) => {
                // The plane through the source facing the camera.
                let normal = (view.eye - center).normalize_or(Vec3::Z);
                let hit = hit_plane(view.ray(at), center, normal)?;
                let free = hit.distance(center);
                let change = if self.handle == SoundHandle::Near {
                    let near = snapped(near, free);
                    SoundChange::Ranges {
                        near,
                        far: far.max(near),
                    }
                } else {
                    let far = snapped(far, free).max(near);
                    SoundChange::Ranges { near, far }
                };
                if let SoundChange::Ranges { near, far } = change {
                    self.value = if self.handle == SoundHandle::Near {
                        near
                    } else {
                        far
                    };
                }
                Some(change)
            }
            (SoundHandle::Side(side), Shape::Zone { world, half, .. }) => {
                let toward = (view.eye - self.grab).normalize_or_zero();
                let plane = self.dir.cross(toward.cross(self.dir)).try_normalize()?;
                let free = (hit_plane(view.ray(at), self.grab, plane)? - self.grab).dot(self.dir);
                // World units per unit of the zone's own along the axis.
                let unit = world.transform_vector3(side.normal()).length().max(1e-6);
                let a = side.axis;
                let size = half[a].abs() * 2.0;
                let length = snapped(size, size + free / unit);
                let delta = length - size;
                let mut half = half.abs();
                half[a] = length * 0.5;
                // The opposite side stays: the center moves half the change.
                let shift = side.normal() * (delta * 0.5);
                let translation =
                    self.local.translation + self.local.rotation * (self.local.scale * shift);
                self.value = length;
                Some(SoundChange::Zone { half, translation })
            }
            (SoundHandle::Fade, Shape::Zone { fade, .. }) => {
                let toward = (view.eye - self.grab).normalize_or_zero();
                let plane = self.dir.cross(toward.cross(self.dir)).try_normalize()?;
                let free = (hit_plane(view.ray(at), self.grab, plane)? - self.grab).dot(self.dir);
                // A fade of nothing is allowed: the room ends at the box.
                let v = snap.snap(
                    Quantity::Position,
                    f64::from(fade),
                    f64::from(fade + free),
                    invert,
                    round,
                ) as f32;
                let fade = ((v * 1000.0).round() / 1000.0).max(0.0);
                self.value = fade;
                Some(SoundChange::Fade(fade))
            }
            _ => None,
        }
    }
}

/// A ring's label while it is pointed at or dragged: "6 m · full", "40 m · −16 dB, held"
/// (`Inverse`, which holds its level beyond the end) or "40 m · silent" (`Linear`).
pub(crate) fn ring_label(out: &mut String, handle: SoundHandle, near: f32, far: f32, holds: bool) {
    use std::fmt::Write as _;
    out.clear();
    match handle {
        SoundHandle::Near => {
            let _ = write!(out, "{} m \u{b7} full", crate::transform::short(near));
        }
        SoundHandle::Far if holds => {
            // Half as loud each time the distance doubles past the near ring.
            let db = 20.0 * (near.max(LEAST) / far.max(LEAST)).log10();
            let _ = write!(
                out,
                "{} m \u{b7} \u{2212}{:.0} dB, held",
                crate::transform::short(far),
                -db
            );
        }
        SoundHandle::Far => {
            let _ = write!(out, "{} m \u{b7} silent", crate::transform::short(far));
        }
        SoundHandle::Side(_) | SoundHandle::Fade => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::camera::OrbitCamera;

    fn view() -> ViewProj {
        let camera = OrbitCamera {
            target: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            distance: 20.0,
            ..OrbitCamera::default()
        };
        ViewProj::new(&camera, [0, 0, 1200, 900])
    }

    fn entity() -> Entity {
        let mut world = engine_ecs::World::new();
        world.spawn(())
    }

    #[test]
    fn a_rings_handles_sit_on_its_right_and_set_its_radius() {
        let view = view();
        let rings = Shape::Rings {
            at: Vec3::ZERO,
            near: 2.0,
            far: 6.0,
        };
        let mut handles = SoundHandles::default();
        handles.layout(&view, entity(), rings);
        assert_eq!(handles.at.len(), 2);
        let near = handles.at[0].1;
        let far = handles.at[1].1;
        assert!(far.distance(near) > 10.0, "apart on the screen");
        assert_eq!(handles.hit(far, 1.0), Some(SoundHandle::Far));
        // The far handle dragged to where 9 m would be: 9 m (snapping off).
        let snap = Snapping {
            on: false,
            ..Snapping::default()
        };
        let right = across(&view, Vec3::ZERO);
        let to = view.screen(right * 9.0).unwrap();
        let mut drag = SoundDrag::start(
            &view,
            entity(),
            SoundHandle::Far,
            rings,
            Transform::IDENTITY,
            1,
        );
        let Some(SoundChange::Ranges { near, far }) = drag.change(&view, to, &snap, false, false)
        else {
            panic!("a change");
        };
        assert_eq!(near, 2.0);
        assert!((far - 9.0).abs() < 0.01, "{far}");
        // The outer ring stops at the inner one.
        let inside = view.screen(right * 1.0).unwrap();
        let Some(SoundChange::Ranges { far, .. }) = drag.change(&view, inside, &snap, false, false)
        else {
            panic!("a change");
        };
        assert_eq!(far, 2.0);
        // The inner one pushes the outer along.
        let mut drag = SoundDrag::start(
            &view,
            entity(),
            SoundHandle::Near,
            rings,
            Transform::IDENTITY,
            1,
        );
        let beyond = view.screen(right * 8.0).unwrap();
        let Some(SoundChange::Ranges { near, far }) =
            drag.change(&view, beyond, &snap, false, false)
        else {
            panic!("a change");
        };
        assert!((near - 8.0).abs() < 0.01 && far == near, "{near} {far}");
        // Snapping on: whole steps of the position step from where it was (2 m).
        let snap = Snapping::default();
        let Some(SoundChange::Ranges { near, .. }) =
            drag.change(&view, beyond, &snap, false, false)
        else {
            panic!("a change");
        };
        let steps = (f64::from(near) - 2.0) / snap.position;
        assert!((steps - steps.round()).abs() < 1e-3, "{near}");
    }

    #[test]
    fn a_zones_side_moves_and_the_opposite_side_stays() {
        let view = view();
        let world = Affine3A::from_translation(Vec3::new(1.0, 0.0, 0.0));
        let zone = Shape::Zone {
            world,
            half: Vec3::new(2.0, 1.0, 3.0),
            fade: 1.0,
        };
        let mut handles = SoundHandles::default();
        handles.layout(&view, entity(), zone);
        assert!(handles.at.iter().any(|(h, _)| *h == SoundHandle::Fade));
        let plus_x = Side {
            axis: 0,
            positive: true,
        };
        let local = Transform::from_xyz(1.0, 0.0, 0.0);
        let mut drag = SoundDrag::start(&view, entity(), SoundHandle::Side(plus_x), zone, local, 1);
        // +x side from x = 3 to x = 5: the box 6 m wide, its middle at 2 (−x stays at −1).
        let to = view.screen(Vec3::new(5.0, 0.0, 0.0)).unwrap();
        let snap = Snapping {
            on: false,
            ..Snapping::default()
        };
        let Some(SoundChange::Zone { half, translation }) =
            drag.change(&view, to, &snap, false, false)
        else {
            panic!("a change");
        };
        assert!((half.x - 3.0).abs() < 0.02, "{half}");
        assert!((translation.x - 2.0).abs() < 0.02, "{translation}");
        assert_eq!(half.y, 1.0);
        // The fade's handle, dragged 2 m up: 3 m of fade.
        let mut fade = SoundDrag::start(&view, entity(), SoundHandle::Fade, zone, local, 2);
        let up = view
            .screen(fade_point(world, Vec3::new(2.0, 1.0, 3.0), 1.0) + Vec3::Y * 2.0)
            .unwrap();
        let Some(SoundChange::Fade(f)) = fade.change(&view, up, &snap, false, false) else {
            panic!("a change");
        };
        assert!((f - 3.0).abs() < 0.02, "{f}");
    }

    #[test]
    fn rings_say_how_loud_they_are() {
        let mut s = String::new();
        ring_label(&mut s, SoundHandle::Near, 6.0, 40.0, true);
        assert_eq!(s, "6 m \u{b7} full");
        ring_label(&mut s, SoundHandle::Far, 6.0, 40.0, true);
        assert_eq!(s, "40 m \u{b7} \u{2212}16 dB, held");
        ring_label(&mut s, SoundHandle::Far, 6.0, 40.0, false);
        assert_eq!(s, "40 m \u{b7} silent");
    }
}
