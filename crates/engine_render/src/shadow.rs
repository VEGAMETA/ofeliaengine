//! Cascaded shadow maps for the directional light (ADR 0044, docs/research/shadows.md).
//!
//! The view is cut along its depth into up to [`MAX_CASCADES`] slices (practical splits:
//! logarithmic and uniform mixed by a λ). Each slice gets an orthographic light projection
//! fitted to its bounding sphere, whose radius does not change when the camera turns,
//! and moved only in whole shadow-map texels, so shadow edges stay still while the camera
//! moves. All cascades share one light view (a rotation) and differ only by a scale and an
//! offset; their depth covers the scene's bounds from the light's side, so casters in front
//! of a slice still cast into it. The cascades are tiles of one atlas.
//!
//! Receivers use the finest cascade that holds them ([`Cascade::holds`]): every point of
//! the view within the shadows' reach lies in the sphere of its slice, so one always does.

use engine_core::math::{Mat4, Vec2, Vec3, Vec4, look_at, vulkan_orthographic};
use engine_rhi::ash::vk;

/// Most cascades a light can have.
pub const MAX_CASCADES: usize = 4;

/// Shadow settings of a [`crate::forward::ForwardRenderer`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowSettings {
    /// Width and height of each cascade in texels, a layer of the shadow map each (ADR
    /// 0093). The default, 1024, gives 4 cascades the texels of the 2048² single shadow map
    /// before them (ADR 0044).
    pub resolution: u32,
    /// Depth format.
    pub format: vk::Format,
    /// Cascades, 1 to [`MAX_CASCADES`].
    pub cascades: u32,
    /// How far from the camera shadows reach, in world units. They never reach beyond the
    /// scene's bounds or the camera's far plane.
    pub distance: f32,
    /// Split scheme: 0 uniform slices, 1 logarithmic (finest near the camera).
    pub split_lambda: f32,
    /// Part of each cascade's half size, at its edge, over which it blends into the next
    /// (and the last one fades out).
    pub blend: f32,
    /// How far receivers are pushed along their normal before the lookup, in texels of
    /// their cascade: against acne on surfaces at grazing angles to the light.
    pub normal_bias: f32,
}

impl Default for ShadowSettings {
    fn default() -> Self {
        Self {
            resolution: 1024,
            format: vk::Format::D32_SFLOAT,
            cascades: 4,
            distance: 150.0,
            split_lambda: 0.75,
            blend: 0.1,
            normal_bias: 1.0,
        }
    }
}

impl ShadowSettings {
    /// Cascades actually used (at least 1, at most [`MAX_CASCADES`]).
    pub fn cascade_count(&self) -> usize {
        (self.cascades as usize).clamp(1, MAX_CASCADES)
    }
}

/// One cascade's light projection, in the light's view space (right-handed, looking down
/// −Z, so depth from the light is −z).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cascade {
    /// Center of the square the cascade covers (light view x, y), on whole texels.
    pub center: Vec2,
    /// Half the square's side, in world units.
    pub radius: f32,
    /// Depth range from the light: the scene's near side to the far side of the slice.
    pub near: f32,
    /// See `near`.
    pub far: f32,
    /// Part of the half size (in the cascade's NDC) within which a receiver counts as
    /// held: the rest is the margin the 3 × 3 filter reads into.
    pub limit: f32,
    /// World size of one texel.
    pub texel: f32,
    /// The slice's far end: distance from the camera's near plane along its view axis.
    pub split: f32,
}

impl Cascade {
    /// The margin a 3 × 3 filter with bilinear taps needs at the edge of a cascade of
    /// `resolution` texels: [`Self::limit`].
    pub fn limit_for(resolution: u32) -> f32 {
        1.0 - 3.0 * 2.0 / resolution.max(8) as f32
    }

    /// Whether a light view position lies inside the cascade with room for the filter.
    pub fn holds(&self, light: Vec3) -> bool {
        let depth = -light.z;
        (light.x - self.center.x).abs() < self.radius * self.limit
            && (light.y - self.center.y).abs() < self.radius * self.limit
            && depth >= self.near
            && depth <= self.far
    }

    /// The cascade's projection from the light's view space: a diagonal scale and an
    /// offset (Vulkan clip space, depth 0..1).
    pub fn clip_from_light(&self) -> Mat4 {
        let (c, r) = (self.center, self.radius);
        vulkan_orthographic(c.x - r, c.x + r, c.y - r, c.y + r, self.near, self.far)
    }
}

/// The cascades of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CascadeFit {
    /// The light's view: a rotation, shared by every cascade.
    pub light_from_world: Mat4,
    /// The first `count` are in use.
    pub cascades: [Cascade; MAX_CASCADES],
    /// Cascades in use; 0 when the view cannot be split (degenerate camera, nothing in
    /// front of it): everything is lit.
    pub count: usize,
    /// How far from the camera's near plane the cascades reach (the last one's split).
    pub reach: f32,
    /// Whether shadows end at the distance setting before the scene or the view does:
    /// they then fade out over the blend band at the last cascade's edge instead of ending
    /// abruptly.
    pub fade: bool,
}

/// How [`fit_cascades_keeping`] holds cascades still for their cache (ADR 0093).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Keep {
    /// The reach the cache's cascades were split over: kept while it still covers the view
    /// and is at most a quarter more than it needs (the view's depth changes with every
    /// step in a scene smaller than the shadows' distance, and the splits with it).
    pub reach: Option<f32>,
    /// Per cascade, the radius its cache was drawn with: kept while it still holds the
    /// slice and is at most a quarter larger than needed, so turning and moving the camera
    /// seldom changes the texel size.
    pub radius: [Option<f32>; MAX_CASCADES],
    /// The cascades are cached. Their depth range is the scene's, not the slice's: depths
    /// drawn in one frame stay right in the next. A reach chosen anew gets a tenth more
    /// than the view needs, so it is kept for longer.
    pub cached: bool,
}

/// Splits the view of `clip_from_world` (unjittered; the camera at `eye`) into cascades
/// and fits each a stable projection for a light shining along `direction`, over a scene
/// within `scene_min`..`scene_max`.
pub fn fit_cascades(
    clip_from_world: Mat4,
    eye: Vec3,
    direction: Vec3,
    scene_min: Vec3,
    scene_max: Vec3,
    settings: &ShadowSettings,
) -> CascadeFit {
    fit_cascades_keeping(
        clip_from_world,
        eye,
        direction,
        scene_min,
        scene_max,
        settings,
        &Keep::default(),
    )
}

/// [`fit_cascades`], holding the cascades as `keep` says.
pub(crate) fn fit_cascades_keeping(
    clip_from_world: Mat4,
    eye: Vec3,
    direction: Vec3,
    scene_min: Vec3,
    scene_max: Vec3,
    settings: &ShadowSettings,
    keep: &Keep,
) -> CascadeFit {
    let direction = direction.normalize_or(Vec3::NEG_Y);
    let up = if direction.y.abs() > 0.99 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let light_from_world = look_at(Vec3::ZERO, direction, up);
    let empty = Cascade {
        center: Vec2::ZERO,
        radius: 0.0,
        near: 0.0,
        far: 1.0,
        limit: 1.0,
        texel: 0.0,
        split: 0.0,
    };
    let mut fit = CascadeFit {
        light_from_world,
        cascades: [empty; MAX_CASCADES],
        count: 0,
        reach: 0.0,
        fade: false,
    };

    // The view as four corner rays from the near plane: a point at distance s along the
    // view axis is a + step · s (perspective and orthographic alike).
    let inverse = clip_from_world.inverse();
    let unproject = |x: f32, y: f32, z: f32| {
        let p = inverse * Vec4::new(x, y, z, 1.0);
        p.truncate() / p.w
    };
    let near_center = unproject(0.0, 0.0, 0.0);
    let forward = (unproject(0.0, 0.0, 0.5) - near_center).normalize_or_zero();
    let corners = [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)];
    let mut rays = [(Vec3::ZERO, Vec3::ZERO); 4];
    for (ray, (x, y)) in rays.iter_mut().zip(corners) {
        let a = unproject(x, y, 0.0);
        let along = unproject(x, y, 0.5) - a;
        let d = along.dot(forward);
        if d.is_nan() || d <= 1e-12 || !a.is_finite() {
            return fit;
        }
        *ray = (a, along / d);
    }
    if forward == Vec3::ZERO || !near_center.is_finite() {
        return fit;
    }
    let at = |ray: (Vec3, Vec3), s: f32| ray.0 + ray.1 * s;

    // How far shadows reach: the setting, the scene's far side, the camera's far plane.
    let scene_corners: [Vec3; 8] = core::array::from_fn(|k| {
        Vec3::new(
            if k & 1 == 0 { scene_min.x } else { scene_max.x },
            if k & 2 == 0 { scene_min.y } else { scene_max.y },
            if k & 4 == 0 { scene_min.z } else { scene_max.z },
        )
    });
    let mut view_reach = scene_corners
        .iter()
        .map(|c| (*c - near_center).dot(forward))
        .fold(f32::MIN, f32::max);
    let far_plane = (unproject(0.0, 0.0, 1.0) - near_center).dot(forward);
    if far_plane.is_finite() && far_plane > 0.0 {
        view_reach = view_reach.min(far_plane);
    }
    let needed = settings.distance.min(view_reach);
    if needed.is_nan() || needed <= 0.0 {
        return fit;
    }
    let reach = match keep.reach.filter(|&r| r >= needed && r <= needed * 1.25) {
        Some(kept) => kept,
        None if keep.cached => (needed * 1.1).min(settings.distance),
        None => needed,
    };
    fit.reach = reach;
    fit.fade = settings.distance < view_reach;

    // Practical splits (Zhang et al. 2006), in distance from the near plane.
    let count = settings.cascade_count();
    let near = (near_center - eye).dot(forward).max(1e-3);
    let lambda = settings.split_lambda.clamp(0.0, 1.0);
    let split = |i: usize| {
        let t = i as f32 / count as f32;
        let logarithmic = near * ((near + reach) / near).powf(t) - near;
        let uniform = reach * t;
        lambda * logarithmic + (1.0 - lambda) * uniform
    };

    // The scene's depth range from the light, shared by every cascade.
    let (mut light_near, mut light_far) = (f32::MAX, f32::MIN);
    for c in scene_corners {
        let depth = -light_from_world.transform_point3(c).z;
        light_near = light_near.min(depth);
        light_far = light_far.max(depth);
    }
    let resolution = settings.resolution.max(1) as f32;
    let limit = Cascade::limit_for(settings.resolution);
    let mut start = 0.0;
    fit.count = count;
    for (i, cascade) in fit.cascades.iter_mut().take(count).enumerate() {
        let end = if i + 1 == count { reach } else { split(i + 1) };
        // The slice's bounding sphere, its center on the view axis where it is equally
        // far from the near and the far corners: it turns with the camera and keeps its
        // radius.
        let ring = |s: f32| {
            let axis = near_center + forward * s;
            rays.iter()
                .map(|&ray| (at(ray, s) - axis).length())
                .fold(0.0f32, f32::max)
        };
        let (r0, r1) = (ring(start), ring(end));
        let t = ((end * end - start * start + r1 * r1 - r0 * r0) / (2.0 * (end - start)))
            .clamp(start, end);
        let center = near_center + forward * t;
        let mut radius = rays
            .iter()
            .flat_map(|&ray| [at(ray, start), at(ray, end)])
            .map(|p| (p - center).length())
            .fold(0.0f32, f32::max);
        // Grown by the filter's margin and the half texel snapping may move the center,
        // so the whole sphere is held; then rounded up to 1/64 of its power of two, so
        // float noise from turning the camera never changes the texel size.
        radius = radius * (1.0 + 1.0 / resolution) / limit;
        let step = 2f32.powi(radius.max(1e-6).log2().floor() as i32 - 6);
        radius = (radius / step).ceil() * step;
        if let Some(kept) = keep.radius[i].filter(|&r| r >= radius && r <= radius * 1.25) {
            radius = kept;
        }
        let texel = 2.0 * radius / resolution;
        // Centered on whole texels: moving the camera moves the map by whole texels.
        let light = light_from_world.transform_point3(center);
        let snapped = Vec2::new(
            (light.x / texel).round() * texel,
            (light.y / texel).round() * texel,
        );
        let depth = -light.z;
        let margin = texel;
        let near = light_near - margin;
        let far = if keep.cached {
            light_far + margin
        } else {
            (depth + radius).min(light_far) + margin
        };
        *cascade = Cascade {
            center: snapped,
            radius,
            near,
            far: far.max(near + texel),
            limit,
            texel,
            split: end,
        };
        start = end;
    }
    fit
}

/// One cascade holding the sphere at `center` of `radius` (a reflection probe's
/// surroundings, ADR 0073: its six faces share it), for a light shining along `direction`
/// over a scene within `scene_min`..`scene_max`, snapped to whole texels as
/// [`fit_cascades`]' cascades are.
pub fn fit_sphere(
    center: Vec3,
    radius: f32,
    direction: Vec3,
    scene_min: Vec3,
    scene_max: Vec3,
    settings: &ShadowSettings,
) -> CascadeFit {
    let direction = direction.normalize_or(Vec3::NEG_Y);
    let up = if direction.y.abs() > 0.99 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let light_from_world = look_at(Vec3::ZERO, direction, up);
    let empty = Cascade {
        center: Vec2::ZERO,
        radius: 0.0,
        near: 0.0,
        far: 1.0,
        limit: 1.0,
        texel: 0.0,
        split: 0.0,
    };
    let mut fit = CascadeFit {
        light_from_world,
        cascades: [empty; MAX_CASCADES],
        count: 0,
        reach: 0.0,
        fade: false,
    };
    let radius = radius.min(settings.distance);
    if radius.is_nan() || radius <= 0.0 {
        return fit;
    }
    let (mut light_near, mut light_far) = (f32::MAX, f32::MIN);
    for k in 0..8 {
        let c = Vec3::new(
            if k & 1 == 0 { scene_min.x } else { scene_max.x },
            if k & 2 == 0 { scene_min.y } else { scene_max.y },
            if k & 4 == 0 { scene_min.z } else { scene_max.z },
        );
        let depth = -light_from_world.transform_point3(c).z;
        light_near = light_near.min(depth);
        light_far = light_far.max(depth);
    }
    let resolution = settings.resolution.max(1) as f32;
    let limit = Cascade::limit_for(settings.resolution);
    let mut radius = radius * (1.0 + 1.0 / resolution) / limit;
    let step = 2f32.powi(radius.max(1e-6).log2().floor() as i32 - 6);
    radius = (radius / step).ceil() * step;
    let texel = 2.0 * radius / resolution;
    let light = light_from_world.transform_point3(center);
    let snapped = Vec2::new(
        (light.x / texel).round() * texel,
        (light.y / texel).round() * texel,
    );
    let near = light_near - texel;
    let far = (-light.z + radius).min(light_far) + texel;
    fit.cascades[0] = Cascade {
        center: snapped,
        radius,
        near,
        far: far.max(near + texel),
        limit,
        texel,
        split: radius,
    };
    fit.count = 1;
    fit.reach = radius;
    fit.fade = settings.distance < radius;
    fit
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{Quat, vulkan_perspective};

    fn camera(eye: Vec3, yaw: f32) -> Mat4 {
        let target = eye + Quat::from_rotation_y(yaw) * Vec3::new(0.0, -0.3, -1.0);
        vulkan_perspective(1.0, 16.0 / 9.0, 0.1, 1000.0) * look_at(eye, target, Vec3::Y)
    }

    const LIGHT: Vec3 = Vec3::new(-0.4, -1.0, -0.3);
    const SCENE: (Vec3, Vec3) = (
        Vec3::new(-500.0, -1.0, -500.0),
        Vec3::new(500.0, 30.0, 500.0),
    );

    fn fit(eye: Vec3, yaw: f32) -> CascadeFit {
        fit_cascades(
            camera(eye, yaw),
            eye,
            LIGHT,
            SCENE.0,
            SCENE.1,
            &ShadowSettings::default(),
        )
    }

    #[test]
    fn splits_grow_and_end_at_the_distance() {
        let f = fit(Vec3::new(0.0, 2.0, 0.0), 0.0);
        assert_eq!(f.count, 4);
        let splits: Vec<f32> = f.cascades.iter().map(|c| c.split).collect();
        assert!(splits.windows(2).all(|w| w[0] < w[1]), "{splits:?}");
        assert!((splits[3] - 150.0).abs() < 1e-3);
        // λ = 0.75 at 0.1 near: the first cascade ends near 10 m, like Bevy's defaults.
        assert!((5.0..15.0).contains(&splits[0]), "{splits:?}");
        let texels: Vec<f32> = f.cascades.iter().map(|c| c.texel).collect();
        assert!(texels.windows(2).all(|w| w[0] < w[1]), "{texels:?}");
    }

    #[test]
    fn every_slice_lies_inside_its_cascade() {
        for yaw in [0.0, 0.7, 2.0, -2.9] {
            let eye = Vec3::new(3.3, 2.0, -7.1);
            let inverse = camera(eye, yaw).inverse();
            let unproject = |x: f32, y: f32, z: f32| {
                let p = inverse * Vec4::new(x, y, z, 1.0);
                p.truncate() / p.w
            };
            let forward = (unproject(0.0, 0.0, 0.5) - unproject(0.0, 0.0, 0.0)).normalize();
            let f = fit(eye, yaw);
            let mut start = 0.0;
            for c in &f.cascades[..f.count] {
                let m = c.clip_from_light() * f.light_from_world;
                // Points on the slice's near and far faces.
                for s in [start, c.split] {
                    for x in [-1.0f32, -0.3, 0.5, 1.0] {
                        for y in [-1.0f32, 0.2, 1.0] {
                            let (a, b) = (unproject(x, y, 0.0), unproject(x, y, 0.5));
                            let p = a + (b - a) / (b - a).dot(forward) * s;
                            // Held with room for the filter, after snapping.
                            let q = m * p.extend(1.0);
                            assert!(c.holds(f.light_from_world.transform_point3(p)), "{q:?}");
                            assert!((0.0..=1.0).contains(&q.z), "{q:?}");
                        }
                    }
                }
                start = c.split;
            }
        }
    }

    #[test]
    fn turning_the_camera_keeps_the_texel_size() {
        let eye = Vec3::new(1.0, 2.0, 3.0);
        let reference = fit(eye, 0.0);
        for k in 1..50 {
            let f = fit(eye, k as f32 * 0.13);
            for (a, b) in reference.cascades.iter().zip(&f.cascades) {
                assert_eq!(a.radius, b.radius, "yaw step {k}");
                assert_eq!(a.texel, b.texel);
            }
        }
    }

    #[test]
    fn moving_the_camera_moves_the_maps_by_whole_texels() {
        for k in 0..40 {
            let eye = Vec3::new(k as f32 * 0.0137, 2.0, k as f32 * -0.021);
            let f = fit(eye, 0.4);
            for c in &f.cascades[..f.count] {
                for v in [c.center.x, c.center.y] {
                    let texels = v / c.texel;
                    assert!((texels - texels.round()).abs() < 1e-3, "{texels}");
                }
            }
        }
    }

    #[test]
    fn projections_are_a_scale_and_an_offset() {
        let f = fit(Vec3::new(0.0, 2.0, 0.0), 0.3);
        for c in &f.cascades[..f.count] {
            let m = c.clip_from_light();
            let off = [
                m.x_axis.y, m.x_axis.z, m.x_axis.w, m.y_axis.x, m.y_axis.z, m.y_axis.w, m.z_axis.x,
                m.z_axis.y, m.z_axis.w,
            ];
            assert!(off.iter().all(|v| *v == 0.0), "{m:?}");
            assert_eq!(m.w_axis.w, 1.0);
        }
    }

    #[test]
    fn every_visible_point_within_reach_is_held() {
        // A room 12 m across, seen from its middle.
        let eye = Vec3::new(0.0, 1.7, 0.0);
        let f = fit_cascades(
            camera(eye, 0.0),
            eye,
            LIGHT,
            Vec3::new(-6.0, 0.0, -6.0),
            Vec3::new(6.0, 4.0, 6.0),
            &ShadowSettings::default(),
        );
        assert_eq!(f.count, 4);
        // The room ends before the distance setting: nothing to fade.
        assert!(!f.fade);
        // Every visible point up to the reach is held by some cascade.
        let inverse = camera(eye, 0.0).inverse();
        for x in [-1.0f32, 0.0, 1.0] {
            for y in [-1.0f32, 0.0, 1.0] {
                for z in [0.0f32, 0.3, 0.6, 0.8, 0.9, 0.95] {
                    let p = inverse * Vec4::new(x, y, z, 1.0);
                    let p = p.truncate() / p.w;
                    let light = f.light_from_world.transform_point3(p);
                    let inside_room =
                        p.abs().x <= 6.0 && p.z.abs() <= 6.0 && (0.0..=4.0).contains(&p.y);
                    if inside_room {
                        assert!(
                            f.cascades[..f.count].iter().any(|c| c.holds(light)),
                            "{p:?} is not held"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_small_scene_shortens_the_reach() {
        let eye = Vec3::new(0.0, 2.0, 0.0);
        let f = fit_cascades(
            camera(eye, 0.0),
            eye,
            LIGHT,
            Vec3::splat(-10.0),
            Vec3::splat(10.0),
            &ShadowSettings::default(),
        );
        assert!(f.cascades[f.count - 1].split < 20.0);
        assert!(!f.fade);
    }

    /// Walking towards a small scene's far side shortens what the view needs at every
    /// step: cached cascades keep their reach (and so their splits and texels) until it is
    /// a quarter more than needed, then take a tenth more than the view needs.
    #[test]
    fn cached_cascades_keep_their_reach_while_the_view_shortens() {
        let (lo, hi) = (Vec3::new(-20.0, -1.0, -60.0), Vec3::new(20.0, 10.0, 20.0));
        let settings = ShadowSettings::default();
        let at = |z: f32, keep: &Keep| {
            let eye = Vec3::new(0.0, 2.0, z);
            fit_cascades_keeping(camera(eye, 0.0), eye, LIGHT, lo, hi, &settings, keep)
        };
        let first = at(
            0.0,
            &Keep {
                cached: true,
                ..Keep::default()
            },
        );
        assert!(first.reach < settings.distance);
        let mut keep = Keep {
            reach: Some(first.reach),
            radius: first.cascades.map(|c| Some(c.radius)),
            cached: true,
        };
        let (mut kept, mut changed) = (0, 0);
        for step in 1..40 {
            let f = at(-(step as f32), &keep);
            if f.reach == keep.reach.unwrap() {
                kept += 1;
                // The same splits: the same texels.
                for (c, r) in f.cascades[..f.count].iter().zip(keep.radius) {
                    assert_eq!(Some(c.radius), r, "step {step}");
                }
            } else {
                changed += 1;
                assert!(f.reach < keep.reach.unwrap(), "the view only shortens");
            }
            keep.reach = Some(f.reach);
            keep.radius = f.cascades.map(|c| Some(c.radius));
        }
        // 39 steps of a metre over a view of some 60: a handful of new reaches.
        assert!(
            kept > 30 && changed > 0 && changed < 8,
            "kept {kept}, changed {changed}"
        );
        // Without the cache the reach follows the view at every step.
        let plain = at(-10.0, &Keep::default());
        let needed = plain.reach;
        assert!(needed < first.reach);
    }

    #[test]
    fn kept_cascades_keep_their_texels_and_the_scene_depth() {
        let eye = Vec3::new(1.0, 2.0, 3.0);
        let first = fit(eye, 0.0);
        let mut keep = Keep {
            cached: true,
            ..Keep::default()
        };
        for (k, c) in first.cascades[..first.count].iter().enumerate() {
            keep.radius[k] = Some(c.radius * 1.2);
        }
        let kept = fit_cascades_keeping(
            camera(eye + Vec3::new(0.7, 0.0, -0.3), 0.4),
            eye + Vec3::new(0.7, 0.0, -0.3),
            LIGHT,
            SCENE.0,
            SCENE.1,
            &ShadowSettings::default(),
            &keep,
        );
        for (k, c) in kept.cascades[..kept.count].iter().enumerate() {
            assert_eq!(Some(c.radius), keep.radius[k]);
            assert_eq!(c.texel, 2.0 * c.radius / 1024.0);
            // Every cascade reaches the scene's far side (and a texel), whatever its slice.
            let scene_far = kept.cascades[0].far - kept.cascades[0].texel;
            assert!((c.far - c.texel - scene_far).abs() < 1e-3, "{c:?}");
        }
        // A radius too large (or too small) is not kept.
        keep.radius[0] = Some(first.cascades[0].radius * 2.0);
        let refit = fit_cascades_keeping(
            camera(eye, 0.0),
            eye,
            LIGHT,
            SCENE.0,
            SCENE.1,
            &ShadowSettings::default(),
            &keep,
        );
        assert_eq!(refit.cascades[0].radius, first.cascades[0].radius);
    }

    #[test]
    fn a_degenerate_camera_gives_no_cascades() {
        let f = fit_cascades(
            Mat4::ZERO,
            Vec3::ZERO,
            LIGHT,
            SCENE.0,
            SCENE.1,
            &ShadowSettings::default(),
        );
        assert_eq!(f.count, 0);
    }
}
