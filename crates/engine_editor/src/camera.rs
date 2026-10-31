//! The editor's viewport camera: it orbits a target point, pans, zooms, looks around and
//! flies (`docs/research/ux/viewport-navigation.md`). Angles are in radians, distances in
//! meters; drags come in physical pixels.
//!
//! In a 2D view (ADR 0125's E13-1b, [`OrbitCamera::flat`]) it looks down −Z onto XY,
//! orthographic: dragging pans, the wheel zooms by whole numbers of the art's pixels
//! ([`OrbitCamera::zoom_2d`]), turning does nothing.

use engine_core::math::{Mat4, Vec2, Vec3, look_at, vulkan_orthographic, vulkan_perspective};

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
    /// A 2D view's height in world units (ADR 0125): orthographic, looking down −Z onto
    /// XY; `None` in 3D.
    pub flat: Option<f32>,
}

/// How far in front of and behind a 2D view's eye it draws.
pub const FLAT_DEPTH: f32 = 10_000.0;

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
            flat: None,
        }
    }
}

impl OrbitCamera {
    /// Unit vector from the target to the eye.
    fn back(&self) -> Vec3 {
        if self.flat.is_some() {
            return Vec3::Z;
        }
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
        if self.flat.is_some() {
            return;
        }
        self.yaw -= dx * TURN_PER_PIXEL;
        self.pitch = (self.pitch + dy * TURN_PER_PIXEL).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Turns the view around the eye (the eye stays, the target moves).
    pub fn look(&mut self, dx: f32, dy: f32) {
        if self.flat.is_some() {
            return;
        }
        let eye = self.eye();
        self.orbit(dx, dy);
        self.target = eye - self.back() * self.distance;
    }

    /// Moves eye and target sideways so that the scene follows the cursor at the target's
    /// depth: `viewport_height` is the viewport's height in the same pixels as the drag.
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_height: f32) {
        let meters_per_pixel = match self.flat {
            Some(height) => height / viewport_height.max(1.0),
            None => 2.0 * self.distance * (self.fov_y * 0.5).tan() / viewport_height.max(1.0),
        };
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

    /// A 2D view's zoom (ADR 0125's E13-1b): `pixels_per_unit` art pixels to a metre
    /// shown at a whole number of screen pixels each (100 %, 200 %, 300 % …; below 100 %
    /// halves), stepped by wheel `notches` from the nearest such scale, in a viewport
    /// `viewport_height` pixels high. The scale it ends at.
    pub fn zoom_2d(&mut self, notches: f32, viewport_height: f32, pixels_per_unit: f32) -> f32 {
        let Some(height) = self.flat else {
            return 1.0;
        };
        let scale = |h: f32| viewport_height.max(1.0) / (h * pixels_per_unit);
        let now = scale(height);
        // Scales as steps: 1, 2, 3 … up, 1/2, 1/4 … down (as negative steps from 1).
        let step = if now >= 1.0 {
            now.round()
        } else {
            1.0 - now.recip().log2().round()
        };
        let step = (step + notches.round()).clamp(-6.0, 64.0);
        let next = if step >= 1.0 {
            step
        } else {
            2f32.powf(step - 1.0)
        };
        self.flat = Some(viewport_height.max(1.0) / (next * pixels_per_unit));
        next
    }

    /// Moves toward the target by wheel notches (positive: closer).
    pub fn zoom(&mut self, notches: f32) {
        self.distance = (self.distance * ZOOM_PER_NOTCH.powf(notches)).clamp(0.05, 10_000.0);
    }

    /// The camera looking at a sphere around `center` of `radius` the way it looks now
    /// (the same turn and elevation), from as far as fits the sphere in a viewport of
    /// `aspect` (width / height), with a margin: what framing a selection moves to.
    pub fn framing(&self, center: Vec3, radius: f32, aspect: f32) -> Self {
        if self.flat.is_some() {
            let height = 2.0 * radius.max(0.05) * FRAME_MARGIN / aspect.min(1.0).max(1e-3);
            return Self {
                target: Vec3::new(center.x, center.y, self.target.z),
                flat: Some(height),
                ..*self
            };
        }
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
        let view = look_at(self.eye(), self.target, Vec3::Y);
        if let Some(height) = self.flat {
            let half = Vec2::new(height * aspect, height) * 0.5;
            return vulkan_orthographic(-half.x, half.x, -half.y, half.y, -FLAT_DEPTH, FLAT_DEPTH)
                * view;
        }
        vulkan_perspective(self.fov_y, aspect, self.near, self.far) * view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    /// A 2D view (ADR 0125's E13-1b): zoom by whole numbers of the art's pixels (halves
    /// below 100 %), pans by the drag at every depth, does not turn, frames in XY, and its
    /// projection shows its height.
    #[test]
    fn a_flat_view_zooms_by_whole_pixels_and_pans() {
        let ppu = 16.0;
        let vh = 720.0;
        let mut c = OrbitCamera {
            flat: Some(vh / (3.3 * ppu)),
            ..OrbitCamera::default()
        };
        // From 330 %, a notch in: 400 %, out twice: 200 %; then halves.
        assert_eq!(c.zoom_2d(1.0, vh, ppu), 4.0);
        assert_eq!(c.zoom_2d(-2.0, vh, ppu), 2.0);
        assert_eq!(c.zoom_2d(-1.0, vh, ppu), 1.0);
        assert_eq!(c.zoom_2d(-1.0, vh, ppu), 0.5);
        assert_eq!(c.zoom_2d(-1.0, vh, ppu), 0.25);
        assert_eq!(c.zoom_2d(2.0, vh, ppu), 1.0);
        assert!((c.flat.unwrap() - vh / ppu).abs() < 1e-3, "100 %: 45 m shown");
        // A drag of 72 px pans a tenth of the height, whatever the distance.
        let before = c.target;
        c.distance = 500.0;
        c.pan(-72.0, 0.0, vh);
        assert!(close(c.target - before, Vec3::new(4.5, 0.0, 0.0)), "{:?}", c.target);
        // Turning does nothing; the eye looks down -Z.
        let eye = c.eye();
        c.orbit(100.0, 50.0);
        c.look(100.0, 50.0);
        assert!(close(c.eye(), eye));
        assert!(close((c.target - c.eye()).normalize(), -Vec3::Z));
        // Framing a sprite 2 m across in a wide view: centred, its height with the margin.
        let f = c.framing(Vec3::new(3.0, 4.0, 9.0), 1.0, 16.0 / 9.0);
        assert!(close(f.target, Vec3::new(3.0, 4.0, c.target.z)));
        assert!((f.flat.unwrap() - 2.0 * FRAME_MARGIN).abs() < 1e-4);
        // The projection maps the view's top edge to clip -1 (Vulkan's y down).
        let top = c.target + Vec3::Y * c.flat.unwrap() * 0.5;
        let clip = c.clip_from_world(16.0 / 9.0) * top.extend(1.0);
        assert!((clip.y / clip.w + 1.0).abs() < 1e-4, "{clip:?}");
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
