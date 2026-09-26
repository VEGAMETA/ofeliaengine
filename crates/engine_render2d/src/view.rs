//! Cameras for 2D: a world camera (y up) and pixel space (y down, for UI and overlays).

use engine_core::math::{Mat4, Vec2, Vec3, vulkan_orthographic};

/// How the world maps onto a render target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View2d {
    /// World → clip space.
    pub clip_from_world: Mat4,
    /// World units per target pixel (the larger of the two axes). SDF edges are this wide.
    pub units_per_pixel: f32,
    /// World +y points down on the target (pixel space).
    pub y_down: bool,
}

impl View2d {
    /// A view from any world → clip matrix (for example a viewport's) and the target size.
    pub fn from_matrix(clip_from_world: Mat4, width: u32, height: u32) -> Self {
        let half = Vec2::new(width as f32, height as f32) * 0.5;
        // Pixels per world unit along world x and y.
        let along_x = (clip_from_world.x_axis.truncate().truncate() * half).length();
        let along_y = (clip_from_world.y_axis.truncate().truncate() * half).length();
        let pixels_per_unit = along_x.min(along_y);
        Self {
            clip_from_world,
            units_per_pixel: if pixels_per_unit > 0.0 {
                pixels_per_unit.recip()
            } else {
                0.0
            },
            y_down: clip_from_world.y_axis.y > 0.0,
        }
    }

    /// Pixel space of a `width` × `height` target: origin at the top-left corner, y down,
    /// one unit per pixel. A quad from (0, 0) to (w, h) covers pixels exactly.
    pub fn pixels(width: u32, height: u32) -> Self {
        let clip_from_world = vulkan_orthographic(0.0, width as f32, height as f32, 0.0, -1.0, 1.0);
        Self::from_matrix(clip_from_world, width, height)
    }
}

/// A 2D camera over a y-up world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera2d {
    /// World point at the center of the target.
    pub center: Vec2,
    /// Zoom: target pixels per world unit.
    pub pixels_per_unit: f32,
    /// Counter-clockwise rotation of the camera in radians (the world appears rotated
    /// clockwise).
    pub rotation: f32,
}

impl Default for Camera2d {
    fn default() -> Self {
        Self {
            center: Vec2::ZERO,
            pixels_per_unit: 1.0,
            rotation: 0.0,
        }
    }
}

impl Camera2d {
    /// The view for a `width` × `height` target.
    pub fn view(&self, width: u32, height: u32) -> View2d {
        let half = Vec2::new(width as f32, height as f32) * (0.5 / self.pixels_per_unit.max(1e-30));
        let projection = vulkan_orthographic(-half.x, half.x, -half.y, half.y, -1.0, 1.0);
        let view_from_world = Mat4::from_rotation_z(-self.rotation)
            * Mat4::from_translation(Vec3::new(-self.center.x, -self.center.y, 0.0));
        View2d::from_matrix(projection * view_from_world, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::Vec4;

    fn to_pixels(view: &View2d, world: Vec2, width: u32, height: u32) -> Vec2 {
        let clip = view.clip_from_world * Vec4::new(world.x, world.y, 0.0, 1.0);
        Vec2::new(
            (clip.x / clip.w * 0.5 + 0.5) * width as f32,
            (clip.y / clip.w * 0.5 + 0.5) * height as f32,
        )
    }

    #[test]
    fn pixel_space_maps_units_to_pixels() {
        let view = View2d::pixels(200, 100);
        assert!(view.y_down);
        assert!((view.units_per_pixel - 1.0).abs() < 1e-6);
        let p = to_pixels(&view, Vec2::new(10.0, 20.0), 200, 100);
        assert!((p - Vec2::new(10.0, 20.0)).length() < 1e-4, "{p}");
    }

    #[test]
    fn camera_centers_zooms_and_keeps_y_up() {
        let camera = Camera2d {
            center: Vec2::new(5.0, 5.0),
            pixels_per_unit: 10.0,
            rotation: 0.0,
        };
        let view = camera.view(200, 100);
        assert!(!view.y_down);
        assert!((view.units_per_pixel - 0.1).abs() < 1e-6);
        let center = to_pixels(&view, Vec2::new(5.0, 5.0), 200, 100);
        assert!((center - Vec2::new(100.0, 50.0)).length() < 1e-4);
        // One unit up in the world is ten pixels up on the target (smaller y).
        let up = to_pixels(&view, Vec2::new(5.0, 6.0), 200, 100);
        assert!((up - Vec2::new(100.0, 40.0)).length() < 1e-4, "{up}");

        let rotated = Camera2d {
            rotation: core::f32::consts::FRAC_PI_2,
            ..camera
        };
        // Camera turned 90° counter-clockwise: its up is world −x, so world +x points
        // down on the target.
        let right = to_pixels(&rotated.view(200, 100), Vec2::new(6.0, 5.0), 200, 100);
        assert!((right - Vec2::new(100.0, 60.0)).length() < 1e-3, "{right}");
    }
}
