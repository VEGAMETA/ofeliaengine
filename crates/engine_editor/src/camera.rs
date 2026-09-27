//! The editor's viewport camera: it orbits a target point, pans, zooms and looks around
//! (`docs/research/ux/viewport-navigation.md`). Angles are in radians, distances in
//! meters; drags come in physical pixels.

use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};

/// Radians of turn per pixel of drag.
const TURN_PER_PIXEL: f32 = 0.005;
/// Distance factor per wheel notch toward the target.
const ZOOM_PER_NOTCH: f32 = 0.85;
/// Pitch stays inside ±89°, so the view never flips over the pole.
const MAX_PITCH: f32 = 1.553;

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

    /// Moves toward the target by wheel notches (positive: closer).
    pub fn zoom(&mut self, notches: f32) {
        self.distance = (self.distance * ZOOM_PER_NOTCH.powf(notches)).clamp(0.05, 10_000.0);
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
    fn the_target_projects_to_the_center() {
        let c = OrbitCamera::default();
        let p = c.clip_from_world(16.0 / 9.0) * c.target.extend(1.0);
        assert!((p.x / p.w).abs() < 1e-5 && (p.y / p.w).abs() < 1e-5);
        assert!(p.z / p.w > 0.0 && p.z / p.w < 1.0);
    }
}
