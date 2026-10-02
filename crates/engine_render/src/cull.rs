//! Visibility tests shared by the CPU and the GPU culling pass (ADR 0034).
//!
//! The same formulas run in `forward.slang`'s `cullMain`; these versions serve CPU-side
//! culling (lights, 2D, tools) and are the reference the GPU tests compare against.

use engine_core::math::{Mat4, Vec3, Vec4};

/// The six planes of a view frustum, each `xyz` an inward unit normal and `w` an offset: a
/// point `p` is inside a plane when `dot(xyz, p) + w >= 0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frustum {
    /// Left, right, bottom, top, near, far (bottom and top swap under a Y flip, which does
    /// not matter for culling).
    pub planes: [Vec4; 6],
}

impl Frustum {
    /// The frustum of a clip-from-world matrix in Vulkan clip space (depth 0..1),
    /// perspective or orthographic (Gribb and Hartmann, "Fast Extraction of Viewing
    /// Frustum Planes from the World-View-Projection Matrix", 2001). Degenerate planes,
    /// such as the far plane of an infinite projection, accept everything.
    pub fn from_clip_from_world(clip_from_world: Mat4) -> Self {
        let m = clip_from_world;
        let (x, y, z, w) = (m.row(0), m.row(1), m.row(2), m.row(3));
        // -w <= x <= w, -w <= y <= w, 0 <= z <= w.
        let planes = [w + x, w - x, w + y, w - y, z, w - z].map(|p| {
            let length = p.truncate().length();
            if length > 1e-20 { p / length } else { Vec4::W }
        });
        Self { planes }
    }

    /// Whether an axis-aligned box (center, half extents) is at least partly inside.
    /// Conservative: boxes near the frustum's edges may pass while outside.
    pub fn intersects_box(&self, center: Vec3, extents: Vec3) -> bool {
        self.planes.iter().all(|p| {
            let normal = p.truncate();
            normal.dot(center) + normal.abs().dot(extents) + p.w >= 0.0
        })
    }

    /// Whether a sphere is at least partly inside (conservative, as
    /// [`Self::intersects_box`]).
    pub fn intersects_sphere(&self, center: Vec3, radius: f32) -> bool {
        self.planes
            .iter()
            .all(|p| p.truncate().dot(center) + p.w >= -radius)
    }
}

/// The world-space axis-aligned box around an object-space box (center, half extents)
/// transformed by the affine `world_from_object` (Arvo, "Transforming Axis-Aligned
/// Bounding Boxes", Graphics Gems, 1990): the center goes through the transform, the
/// extents through the absolute values of its linear part. Exact for the box's corners.
pub fn transform_box(world_from_object: &Mat4, center: Vec3, extents: Vec3) -> (Vec3, Vec3) {
    let m = world_from_object;
    let world_center = m.transform_point3(center);
    let abs = |axis: Vec4| axis.truncate().abs();
    let world_extents =
        abs(m.x_axis) * extents.x + abs(m.y_axis) * extents.y + abs(m.z_axis) * extents.z;
    (world_center, world_extents)
}

/// Pixels per unit at distance 1 over the detail-level threshold, for a view of `height`
/// pixels (the culling pass's `lod_scale`, ADR 0053): the projection's vertical scale (the
/// length of the clip matrix's y row, whatever the camera's rotation) times half the
/// height. 0 (full meshes) for orthographic views and a threshold of 0.
pub fn lod_scale(clip_from_world: &Mat4, height: u32, threshold: f32) -> f32 {
    let (y_row, w_row) = (
        clip_from_world.row(1).truncate(),
        clip_from_world.row(3).truncate(),
    );
    if threshold > 0.0 && w_row.length() > 1e-6 {
        y_row.length() * height as f32 * 0.5 / threshold
    } else {
        0.0
    }
}

/// The detail level the culling pass draws an instance at (`choose_lod` in
/// `forward.slang`, ADR 0053): the coarsest whose error (object units, `errors[0]` the full
/// mesh's), times the instance's largest scale and `lod_scale`, stays within the distance
/// from `eye` to the instance's world box (`center`, `extents`) at its nearest.
pub fn choose_lod(
    errors: &[f32],
    world_from_object: &Mat4,
    center: Vec3,
    extents: Vec3,
    eye: Vec3,
    lod_scale: f32,
) -> u32 {
    if errors.len() <= 1 || lod_scale <= 0.0 {
        return 0;
    }
    let m = world_from_object;
    let scale = m
        .x_axis
        .truncate()
        .length_squared()
        .max(m.y_axis.truncate().length_squared())
        .max(m.z_axis.truncate().length_squared())
        .sqrt();
    let distance = ((center - eye).length() - extents.length()).max(1e-3);
    let allowed = distance / (scale * lod_scale);
    let mut lod = 0;
    for (k, &error) in errors.iter().enumerate().skip(1) {
        if error <= allowed {
            lod = k as u32;
        }
    }
    lod
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{Quat, look_at, vulkan_orthographic, vulkan_perspective};

    #[test]
    fn detail_levels_follow_the_projected_error() {
        // 90° vertical field of view, 1000 pixels high: 500 pixels per unit at distance 1.
        let clip = vulkan_perspective(90f32.to_radians(), 1.0, 0.1, 1000.0)
            * look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y);
        let scale = lod_scale(&clip, 1000, 1.0);
        assert!((scale - 500.0).abs() < 0.01, "{scale}");
        assert_eq!(lod_scale(&clip, 1000, 0.0), 0.0);
        let ortho = vulkan_orthographic(-1.0, 1.0, -1.0, 1.0, 0.0, 10.0);
        assert_eq!(lod_scale(&ortho, 1000, 1.0), 0.0);
        // Errors of 0.01, 0.1, 1 units: a pixel at 5, 50 and 500 units away.
        let errors = [0.0, 0.01, 0.1, 1.0];
        let at = |distance: f32, scale_up: f32| {
            let world = Mat4::from_scale_rotation_translation(
                Vec3::splat(scale_up),
                Quat::IDENTITY,
                Vec3::new(0.0, 0.0, -distance),
            );
            // A point-sized box, so the nearest distance is the center's.
            choose_lod(
                &errors,
                &world,
                Vec3::new(0.0, 0.0, -distance),
                Vec3::ZERO,
                Vec3::ZERO,
                scale,
            )
        };
        assert_eq!(at(2.0, 1.0), 0);
        assert_eq!(at(6.0, 1.0), 1);
        assert_eq!(at(60.0, 1.0), 2);
        assert_eq!(at(600.0, 1.0), 3);
        // Twice as large: twice as far for the same level.
        assert_eq!(at(60.0, 2.0), 1);
        assert_eq!(at(120.0, 2.0), 2);
        // A box reaching towards the camera counts from its nearest point.
        let world = Mat4::from_translation(Vec3::new(0.0, 0.0, -6.0));
        let near = choose_lod(
            &errors,
            &world,
            Vec3::new(0.0, 0.0, -6.0),
            Vec3::new(0.0, 0.0, 2.0),
            Vec3::ZERO,
            scale,
        );
        assert_eq!(near, 0);
    }

    fn camera() -> Mat4 {
        vulkan_perspective(60f32.to_radians(), 16.0 / 9.0, 0.5, 100.0)
            * look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y)
    }

    #[test]
    fn points_inside_and_outside_a_perspective_frustum() {
        let f = Frustum::from_clip_from_world(camera());
        let inside = |p: Vec3| f.intersects_sphere(p, 0.0);
        assert!(inside(Vec3::new(0.0, 0.0, -10.0)));
        assert!(inside(Vec3::new(0.0, 0.0, -0.6)));
        assert!(inside(Vec3::new(0.0, 0.0, -99.0)));
        assert!(!inside(Vec3::new(0.0, 0.0, -0.4)), "before the near plane");
        assert!(!inside(Vec3::new(0.0, 0.0, -101.0)), "beyond the far plane");
        assert!(!inside(Vec3::new(0.0, 0.0, 5.0)), "behind the camera");
        // Vertical half angle 30°: at distance 10 the edge is at y = 5.77.
        assert!(inside(Vec3::new(0.0, 5.6, -10.0)));
        assert!(!inside(Vec3::new(0.0, 6.0, -10.0)));
        assert!(!inside(Vec3::new(0.0, -6.0, -10.0)));
        // Horizontal: 16/9 wider.
        assert!(inside(Vec3::new(10.0, 0.0, -10.0)));
        assert!(!inside(Vec3::new(10.5, 0.0, -10.0)));
        assert!(!inside(Vec3::new(-10.5, 0.0, -10.0)));
    }

    #[test]
    fn planes_match_clip_space_for_random_points() {
        let m = camera();
        let f = Frustum::from_clip_from_world(m);
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };
        for _ in 0..10_000 {
            let p = Vec3::new(
                next() * 200.0 - 100.0,
                next() * 120.0 - 60.0,
                next() * -120.0,
            );
            let clip = m * p.extend(1.0);
            let in_clip = clip.x.abs() <= clip.w && clip.y.abs() <= clip.w && clip.z >= 0.0;
            let in_clip = in_clip && clip.z <= clip.w;
            // Skip points within rounding distance of a plane.
            let margin = f.planes.iter().map(|q| (q.truncate().dot(p) + q.w).abs());
            if margin.fold(f32::MAX, f32::min) < 1e-3 {
                continue;
            }
            assert_eq!(f.intersects_sphere(p, 0.0), in_clip, "{p}");
        }
    }

    #[test]
    fn orthographic_frustum_is_the_box() {
        let m = vulkan_orthographic(-2.0, 2.0, -1.0, 1.0, 0.0, 10.0);
        let f = Frustum::from_clip_from_world(m);
        assert!(f.intersects_box(Vec3::new(0.0, 0.0, -5.0), Vec3::splat(0.1)));
        assert!(f.intersects_box(Vec3::new(2.05, 0.0, -5.0), Vec3::splat(0.1)));
        assert!(!f.intersects_box(Vec3::new(2.2, 0.0, -5.0), Vec3::splat(0.1)));
        assert!(!f.intersects_box(Vec3::new(0.0, 1.2, -5.0), Vec3::splat(0.1)));
        assert!(!f.intersects_box(Vec3::new(0.0, 0.0, 1.0), Vec3::splat(0.1)));
        assert!(!f.intersects_box(Vec3::new(0.0, 0.0, -10.5), Vec3::splat(0.1)));
    }

    #[test]
    fn infinite_far_plane_accepts_everything_far() {
        // Right-handed, depth 0 at `near` and 1 at infinity: the far plane row is
        // (0, 0, 0, near), which has no normal.
        let (s, near) = (1.0 / 0.5f32.tan(), 0.1);
        let projection = Mat4::from_cols(
            Vec4::new(s, 0.0, 0.0, 0.0),
            Vec4::new(0.0, -s, 0.0, 0.0),
            Vec4::new(0.0, 0.0, -1.0, -1.0),
            Vec4::new(0.0, 0.0, -near, 0.0),
        );
        let m = projection * look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y);
        let f = Frustum::from_clip_from_world(m);
        assert_eq!(f.planes[5], Vec4::W);
        assert!(f.intersects_sphere(Vec3::new(0.0, 0.0, -0.2), 0.0));
        assert!(!f.intersects_sphere(Vec3::new(0.0, 0.0, -0.05), 0.0));
        assert!(f.intersects_sphere(Vec3::new(0.0, 0.0, -1.0e6), 0.0));
        assert!(!f.intersects_sphere(Vec3::new(0.0, 0.0, 1.0), 0.0));
    }

    #[test]
    fn transformed_box_contains_every_transformed_corner_tightly() {
        let m = Mat4::from_scale_rotation_translation(
            Vec3::new(2.0, 0.5, 3.0),
            Quat::from_euler(engine_core::math::EulerRot::XYZ, 0.3, -1.1, 2.0),
            Vec3::new(5.0, -2.0, 1.0),
        );
        let (center, extents) = (Vec3::new(0.5, 1.0, -0.25), Vec3::new(1.0, 0.25, 2.0));
        let (wc, we) = transform_box(&m, center, extents);
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for i in 0..8 {
            let s = Vec3::new(
                if i & 1 == 0 { -1.0 } else { 1.0 },
                if i & 2 == 0 { -1.0 } else { 1.0 },
                if i & 4 == 0 { -1.0 } else { 1.0 },
            );
            let p = m.transform_point3(center + s * extents);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        assert!((wc - (lo + hi) * 0.5).abs().max_element() < 1e-4);
        assert!((we - (hi - lo) * 0.5).abs().max_element() < 1e-4);
    }
}
