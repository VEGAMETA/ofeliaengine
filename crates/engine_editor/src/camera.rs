//! The editor's viewport camera: it orbits a target point, pans, zooms, looks around and
//! flies (`docs/research/ux/viewport-navigation.md`). Angles are in radians, distances in
//! meters; drags come in physical pixels.

use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};

/// Radians of turn per pixel of drag.
const TURN_PER_PIXEL: f32 = 0.005;
/// Distance factor per wheel notch toward the target.
const ZOOM_PER_NOTCH: f32 = 0.85;
/// Pitch stays inside ±89°, so the view never flips over the pole.
const MAX_PITCH: f32 = 1.553;
/// Room around a framed sphere: it fills the view's narrower side less a tenth.
const FRAME_MARGIN: f32 = 1.1;

/// A camera orbiting `target`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrbitCamera {
    /// The point the camera orbits and looks at.
    pub target: Vec3,
    /// Turn around +Y; 0 puts the camera on +Z of the target, looking toward -Z.
    pub yaw: f32,
    /// Elevation; positive puts the camera above the target, looking down.
    pub pitch: f32,
    /// Distance from the target.
    pub distance: f32,
    /// Vertical field of view.
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            target: Vec3::ZERO,
            yaw: 0.6,
            pitch: 0.45,
            distance: 16.0,
            fov_y: 50f32.to_radians(),
            near: 0.1,
            far: 500.0,
        }
    }
}

impl OrbitCamera {
    /// Unit vector from the target to the eye.
    fn back(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(cp * sy, sp, cp * cy)
    }

    /// The camera position.
    pub fn eye(&self) -> Vec3 {
        self.target + self.back() * self.distance
    }

    /// Orbits around the target (the eye moves, the target stays).
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * TURN_PER_PIXEL;
        self.pitch = (self.pitch + dy * TURN_PER_PIXEL).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Turns the view around the eye (the eye stays, the target moves).
    pub fn look(&mut self, dx: f32, dy: f32) {
        let eye = self.eye();
        self.orbit(dx, dy);
        self.target = eye - self.back() * self.distance;
    }

    /// Moves eye and target sideways so that the scene follows the cursor at the target's
    /// depth: `viewport_height` is the viewport's height in the same pixels as the drag.
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_height: f32) {
        let meters_per_pixel =
            2.0 * self.distance * (self.fov_y * 0.5).tan() / viewport_height.max(1.0);
        let forward = -self.back();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward);
        self.target += (-right * dx + up * dy) * meters_per_pixel;
    }

    /// Flies the eye and the target together: `forward` along the view, `right` across it
    /// and level, `up` along the world's +Y, in meters.
    pub fn fly(&mut self, forward: f32, right: f32, up: f32) {
        let ahead = -self.back();
        let side = ahead.cross(Vec3::Y).normalize_or_zero();
        self.target += ahead * forward + side * right + Vec3::Y * up;
    }

    /// Moves toward the target by wheel notches (positive: closer).
    pub fn zoom(&mut self, notches: f32) {
        self.distance = (self.distance * ZOOM_PER_NOTCH.powf(notches)).clamp(0.05, 10_000.0);
    }

    /// The camera looking at a sphere around `center` of `radius` the way it looks now
    /// (the same turn and elevation), from as far as fits the sphere in a viewport of
    /// `aspect` (width / height), with a margin: what framing a selection moves to.
    pub fn framing(&self, center: Vec3, radius: f32, aspect: f32) -> Self {
        let tan_y = (self.fov_y * 0.5).tan();
        // The narrower of the view's two half angles.
        let half = tan_y.min(tan_y * aspect).atan();
        let distance = radius.max(0.05) / half.sin() * FRAME_MARGIN;
        Self {
            target: center,
            distance: distance.clamp(self.near * 2.0, 10_000.0),
            ..*self
        }
    }

    /// The camera `t` of the way (0 to 1) from `self` to `to`: the target along a line, the
    /// turn the short way round, the distance in proportion (so a long way in shrinks as
    /// smoothly as a short one).
    pub fn toward(&self, to: &Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let turn = (to.yaw - self.yaw + core::f32::consts::PI).rem_euclid(core::f32::consts::TAU)
            - core::f32::consts::PI;
        Self {
            target: self.target.lerp(to.target, t),
            yaw: self.yaw + turn * t,
            pitch: self.pitch + (to.pitch - self.pitch) * t,
            distance: (self.distance.ln() + (to.distance.ln() - self.distance.ln()) * t).exp(),
            ..*to
        }
    }

    /// World to clip space for a viewport of the given aspect ratio (width / height).
    pub fn clip_from_world(&self, aspect: f32) -> Mat4 {
        vulkan_perspective(self.fov_y, aspect, self.near, self.far)
            * look_at(self.eye(), self.target, Vec3::Y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    #[test]
    fn orbit_keeps_the_target_and_the_distance() {
        let mut c = OrbitCamera::default();
        let target = c.target;
        c.orbit(120.0, -40.0);
        assert!(close(c.target, target));
        assert!(((c.eye() - c.target).length() - c.distance).abs() < 1e-4);
        c.orbit(0.0, 10_000.0);
        assert!(c.pitch <= MAX_PITCH, "no flip over the pole");
    }

    #[test]
    fn look_keeps_the_eye() {
        let mut c = OrbitCamera::default();
        let eye = c.eye();
        c.look(200.0, 50.0);
        assert!(close(c.eye(), eye));
        assert!(!close(c.target, Vec3::ZERO));
    }

    #[test]
    fn pan_moves_eye_and_target_together_and_zoom_keeps_the_target() {
        let mut c = OrbitCamera::default();
        let offset = c.eye() - c.target;
        c.pan(100.0, 30.0, 600.0);
        assert!(!close(c.target, Vec3::ZERO));
        assert!(close(c.eye() - c.target, offset));
        let target = c.target;
        c.zoom(2.0);
        assert!(close(c.target, target));
        assert!((c.distance - 16.0 * 0.85 * 0.85).abs() < 1e-4);
    }

    #[test]
    fn fly_moves_eye_and_target_along_the_view_and_up_the_world() {
        let mut c = OrbitCamera::default();
        let (eye, offset) = (c.eye(), c.eye() - c.target);
        let ahead = (c.target - eye).normalize();
        c.fly(2.0, 0.0, 0.0);
        assert!(close(c.eye(), eye + ahead * 2.0));
        assert!(close(c.eye() - c.target, offset), "the view stays");
        c.fly(0.0, 0.0, 1.5);
        assert!(
            close(c.eye(), eye + ahead * 2.0 + Vec3::Y * 1.5),
            "up is the world's"
        );
        let before = c.eye();
        c.fly(0.0, 1.0, 0.0);
        let side = c.eye() - before;
        assert!((side.length() - 1.0).abs() < 1e-4, "{side}");
        assert!(
            side.y.abs() < 1e-6 && side.dot(ahead).abs() < 1e-4,
            "level, across"
        );
    }

    #[test]
    fn framing_fits_the_sphere_in_the_narrower_side() {
        let c = OrbitCamera::default();
        for aspect in [16.0 / 9.0, 0.5] {
            let f = c.framing(Vec3::new(3.0, 1.0, -2.0), 2.0, aspect);
            assert_eq!((f.yaw, f.pitch), (c.yaw, c.pitch), "it looks the same way");
            let m = f.clip_from_world(aspect);
            // The sphere's points across the view stay inside it, with the margin.
            let eye = f.eye();
            let forward = (f.target - eye).normalize();
            let right = forward.cross(Vec3::Y).normalize();
            let up = right.cross(forward);
            for (dir, across) in [(right, true), (-right, true), (up, false), (-up, false)] {
                // Where the sphere's edge seen from the eye toward `dir` crosses the
                // target's plane.
                let d = f.distance;
                let edge = f.target + dir * (2.0 * d / (d * d - 4.0).sqrt());
                let p = m * edge.extend(1.0);
                let (x, y) = ((p.x / p.w).abs(), (p.y / p.w).abs());
                assert!(x <= 1.0 && y <= 1.0, "inside at {aspect}: {x} {y}");
                // The narrower side, less the margin.
                if across == (aspect < 1.0) {
                    assert!(x.max(y) > 0.85, "fills the narrower side: {x} {y}");
                }
            }
        }
    }

    #[test]
    fn toward_goes_the_short_way_round() {
        let from = OrbitCamera {
            yaw: 3.0,
            distance: 1.0,
            ..OrbitCamera::default()
        };
        let to = OrbitCamera {
            yaw: -3.0,
            distance: 100.0,
            target: Vec3::new(10.0, 0.0, 0.0),
            ..OrbitCamera::default()
        };
        assert_eq!(from.toward(&to, 0.0).target, from.target);
        let end = from.toward(&to, 1.0);
        assert!(close(end.target, to.target) && (end.distance - 100.0).abs() < 1e-3);
        // 3 to -3 is 0.28 the short way, through π.
        let half = from.toward(&to, 0.5);
        assert!((half.yaw - (3.0 + (core::f32::consts::TAU - 6.0) * 0.5)).abs() < 1e-4);
        assert!(
            (half.distance - 10.0).abs() < 1e-3,
            "in proportion: {}",
            half.distance
        );
    }

    #[test]
    fn the_target_projects_to_the_center() {
        let c = OrbitCamera::default();
        let p = c.clip_from_world(16.0 / 9.0) * c.target.extend(1.0);
        assert!((p.x / p.w).abs() < 1e-5 && (p.y / p.w).abs() < 1e-5);
        assert!(p.z / p.w > 0.0 && p.z / p.w < 1.0);
    }
}
