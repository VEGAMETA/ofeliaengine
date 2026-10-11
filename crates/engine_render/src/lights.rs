//! Local lights (point and spot) and their culling by z-bins and tile bitmasks (ADR 0045,
//! docs/research/many-lights.md; after Drobot, SIGGRAPH 2017).
//!
//! Every frame the CPU:
//!
//! 1. culls the lights against the view frustum (by their bounding spheres);
//! 2. sorts the rest by their distance along the view axis;
//! 3. fills the **z-bins**: [`Z_BINS`] slices of view depth, each with the lowest and the
//!    highest index (in sorted order) of the lights overlapping it;
//! 4. computes each light's conservative screen rectangle in [`LIGHT_TILE`]-pixel tiles.
//!
//! A compute pass then sets, for every tile, one bit per light whose rectangle covers it,
//! and a pixel shades the lights that are both in its tile's bits and in its z-bin's
//! index range. Memory grows with tiles × lights / 8 bytes, with no per-cell limit.

use crate::cull::Frustum;
use engine_core::math::{Mat4, Vec3, Vec4};

/// Side of a screen tile of the light bitmasks, in pixels.
pub const LIGHT_TILE: u32 = 16;
/// Slices of view depth the z-bins divide the lights' range into.
pub const Z_BINS: usize = 1024;
/// Most lights a view can shade (indices are 16-bit in the z-bins); the rest are dropped.
pub const MAX_LIGHTS: usize = 65_535;

/// A point or spot light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalLight {
    /// World position.
    pub position: Vec3,
    /// Distance at which the light's influence ends: its falloff reaches zero there.
    /// Smaller ranges cull better.
    pub range: f32,
    /// Color × intensity: the light's intensity per steradian (candela-like). Illuminance
    /// at distance `d` is `color / d²`, windowed to zero at `range`.
    pub color: Vec3,
    /// A cone, or `None` for a point light.
    pub spot: Option<Spot>,
    /// Whether it casts shadows (ADR 0071; with
    /// [`crate::forward::ForwardRenderer::set_local_shadows`]): a spot light one view, a
    /// point light six cube faces in the shadow atlas, drawn again only when the light or
    /// a caster in its range changes. Keep the light's index in the list stable: its
    /// cached shadow is known by it.
    pub shadow: bool,
}

/// The cone of a spot light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spot {
    /// Unit direction the cone points in.
    pub direction: Vec3,
    /// Half angle of full intensity, in radians.
    pub inner: f32,
    /// Half angle where the light ends, in radians (at least `inner`).
    pub outer: f32,
}

/// `Light` in `forward.slang` (48 bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[repr(C)]
pub(crate) struct LightData {
    /// xyz position, w range.
    pub position_range: [f32; 4],
    /// rgb color, w: spot scale (0 for point lights).
    pub color_scale: [f32; 4],
    /// xyz direction the light shines in, w: spot offset (1 for point lights).
    pub direction_offset: [f32; 4],
}

const _: () = assert!(core::mem::size_of::<LightData>() == 48);

impl LightData {
    fn new(light: &LocalLight) -> Self {
        // Cone attenuation saturate(cos · scale + offset)², from the cosines of the two
        // half angles (as in Filament).
        let (direction, scale, offset) = match light.spot {
            Some(spot) => {
                let outer = spot.outer.max(spot.inner).clamp(0.0, core::f32::consts::PI);
                let inner = spot.inner.clamp(0.0, outer);
                let (cos_outer, cos_inner) = (outer.cos(), inner.cos());
                let scale = 1.0 / (cos_inner - cos_outer).max(1e-4);
                (
                    spot.direction.normalize_or(Vec3::NEG_Y),
                    scale,
                    -cos_outer * scale,
                )
            }
            None => (Vec3::NEG_Y, 0.0, 1.0),
        };
        Self {
            position_range: light.position.extend(light.range.max(0.0)).to_array(),
            color_scale: light.color.extend(scale).to_array(),
            direction_offset: direction.extend(offset).to_array(),
        }
    }
}

/// A culled light on the way to the GPU.
#[derive(Debug, Clone, Copy)]
struct Culled {
    /// Distance of the sphere's center along the view axis, from the near plane.
    depth: f32,
    /// Index in the caller's list.
    index: u32,
    /// Tile rectangle: min x, min y, max x, max y (inclusive).
    rect: [u16; 4],
}

/// Reused buffers of the CPU part.
#[derive(Debug, Default)]
pub(crate) struct LightScratch {
    culled: Vec<Culled>,
}

impl LightScratch {
    /// The caller's index of each light kept by the last [`cull_lights`], in depth order.
    pub(crate) fn kept(&self) -> impl Iterator<Item = u32> + '_ {
        self.culled.iter().map(|c| c.index)
    }
}

/// The view: where its near plane is and which way it looks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ViewAxis {
    /// The near plane's center.
    pub near_center: Vec3,
    /// Unit view direction.
    pub forward: Vec3,
}

impl ViewAxis {
    /// The axis of `clip_from_world` (perspective or orthographic), or `None` when it is
    /// degenerate.
    pub(crate) fn new(clip_from_world: Mat4) -> Option<Self> {
        let inverse = clip_from_world.inverse();
        let unproject = |z: f32| {
            let p = inverse * Vec4::new(0.0, 0.0, z, 1.0);
            p.truncate() / p.w
        };
        let near_center = unproject(0.0);
        let forward = (unproject(0.5) - near_center).normalize_or_zero();
        (near_center.is_finite() && forward != Vec3::ZERO).then_some(Self {
            near_center,
            forward,
        })
    }

    /// Distance of `p` along the view axis from the near plane.
    pub(crate) fn depth(&self, p: Vec3) -> f32 {
        (p - self.near_center).dot(self.forward)
    }
}

/// What the frame's shaders need to find the lights.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LightLayout {
    /// Lights kept, in depth order.
    pub count: u32,
    /// 32-bit words of a tile's bitmask.
    pub words: u32,
    /// Tiles per row and column.
    pub tiles: [u32; 2],
    /// Z-bins per unit of depth along the view axis.
    pub bin_scale: f32,
}

impl LightLayout {
    /// No lights.
    pub(crate) const EMPTY: Self = Self {
        count: 0,
        words: 0,
        tiles: [1, 1],
        bin_scale: 0.0,
    };

    /// Words of all the tiles' bitmasks.
    pub(crate) fn mask_words(&self) -> u64 {
        u64::from(self.tiles[0]) * u64::from(self.tiles[1]) * u64::from(self.words)
    }
}

/// Culls and sorts `lights` for a view of `size` pixels. Returns the layout; the lights
/// are then written by [`write_lights`], [`write_rects`] and [`write_bins`].
pub(crate) fn cull_lights(
    lights: &[LocalLight],
    clip_from_world: Mat4,
    axis: ViewAxis,
    size: [u32; 2],
    scratch: &mut LightScratch,
) -> LightLayout {
    let tiles = [
        size[0].div_ceil(LIGHT_TILE).max(1),
        size[1].div_ceil(LIGHT_TILE).max(1),
    ];
    let frustum = Frustum::from_clip_from_world(clip_from_world);
    let culled = &mut scratch.culled;
    culled.clear();
    let mut far = 0.0f32;
    for (index, light) in lights.iter().enumerate() {
        if culled.len() == MAX_LIGHTS {
            break;
        }
        let (center, radius) = (light.position, light.range);
        let finite = center.is_finite() && radius.is_finite();
        if !finite
            || radius <= 0.0
            || light.color.max_element() <= 0.0
            || frustum
                .planes
                .iter()
                .any(|p| p.truncate().dot(center) + p.w < -radius)
        {
            continue;
        }
        let depth = axis.depth(center);
        far = far.max(depth + radius);
        culled.push(Culled {
            depth,
            index: index as u32,
            rect: tile_rect(clip_from_world, center, radius, size, tiles),
        });
    }
    culled.sort_unstable_by(|a, b| a.depth.total_cmp(&b.depth));
    LightLayout {
        count: culled.len() as u32,
        words: (culled.len() as u32).div_ceil(32),
        tiles,
        bin_scale: Z_BINS as f32 / far.max(1e-6),
    }
}

/// Writes the lights kept by [`cull_lights`], in depth order.
pub(crate) fn write_lights(
    lights: &[LocalLight],
    scratch: &LightScratch,
    out: &mut [core::mem::MaybeUninit<LightData>],
) {
    for (out, c) in out.iter_mut().zip(&scratch.culled) {
        out.write(LightData::new(&lights[c.index as usize]));
    }
}

/// Writes the kept lights' tile rectangles: min x | min y << 16, max x | max y << 16.
pub(crate) fn write_rects(scratch: &LightScratch, out: &mut [core::mem::MaybeUninit<[u32; 2]>]) {
    for (out, c) in out.iter_mut().zip(&scratch.culled) {
        out.write([
            u32::from(c.rect[0]) | u32::from(c.rect[1]) << 16,
            u32::from(c.rect[2]) | u32::from(c.rect[3]) << 16,
        ]);
    }
}

/// Writes the [`Z_BINS`] z-bins: lowest index | highest index << 16 of the kept lights
/// overlapping each (empty bins have lowest > highest).
pub(crate) fn write_bins(
    lights: &[LocalLight],
    layout: &LightLayout,
    scratch: &LightScratch,
    out: &mut [core::mem::MaybeUninit<u32>],
) {
    let mut bins = [(u16::MAX, 0u16); Z_BINS];
    let bin = |depth: f32| ((depth * layout.bin_scale).max(0.0) as usize).min(Z_BINS - 1);
    for (i, c) in scratch.culled.iter().enumerate() {
        let range = lights[c.index as usize].range;
        let i = i as u16;
        for b in &mut bins[bin(c.depth - range)..=bin(c.depth + range)] {
            b.0 = b.0.min(i);
            b.1 = b.1.max(i);
        }
    }
    for (out, (low, high)) in out.iter_mut().zip(bins) {
        out.write(u32::from(low) | u32::from(high) << 16);
    }
}

/// The screen bounds (NDC x and y, low and high) of a sphere under a projection: the
/// extents of its projected quadric (Sigg, Weyrich, Botsch, Gross, "GPU-Based Ray-Casting
/// of Quadratic Surfaces", 2006), exact for perspective and orthographic projections.
/// `None` when the sphere reaches the plane of the eye (it may cover the whole screen).
pub(crate) fn sphere_bounds(
    clip_from_world: Mat4,
    center: Vec3,
    radius: f32,
) -> Option<[[f32; 2]; 2]> {
    // Clip space from the unit sphere; its rows x, y and w.
    let t = clip_from_world
        * Mat4::from_cols(
            Vec4::X * radius,
            Vec4::Y * radius,
            Vec4::Z * radius,
            center.extend(1.0),
        );
    let (rx, ry, rw) = (t.row(0), t.row(1), t.row(3));
    // The quadric's form diag(1, 1, 1, −1).
    let dot = |a: Vec4, b: Vec4| a.x * b.x + a.y * b.y + a.z * b.z - a.w * b.w;
    let ww = dot(rw, rw);
    // In front of the eye, and wholly: |w of the center| > radius · |∇w|.
    if ww >= 0.0 || (clip_from_world * center.extend(1.0)).w <= 0.0 {
        return None;
    }
    let extent = |r: Vec4| {
        let (rw_r, rr) = (dot(r, rw), dot(r, r));
        let root = (rw_r * rw_r - ww * rr).max(0.0).sqrt();
        let (a, b) = ((rw_r + root) / ww, (rw_r - root) / ww);
        [a.min(b), a.max(b)]
    };
    Some([extent(rx), extent(ry)])
}

/// The tiles a sphere may cover: its exact screen bounds, or the whole screen when it
/// reaches the plane of the eye.
pub(crate) fn tile_rect(
    clip_from_world: Mat4,
    center: Vec3,
    radius: f32,
    size: [u32; 2],
    tiles: [u32; 2],
) -> [u16; 4] {
    let Some(bounds) = sphere_bounds(clip_from_world, center, radius) else {
        return [0, 0, (tiles[0] - 1) as u16, (tiles[1] - 1) as u16];
    };
    let tile = |ndc: f32, axis: usize| {
        let pixel = (ndc.clamp(-1.0, 1.0) * 0.5 + 0.5) * size[axis] as f32;
        ((pixel / LIGHT_TILE as f32).floor().max(0.0) as u32).min(tiles[axis] - 1) as u16
    };
    [
        tile(bounds[0][0], 0),
        tile(bounds[1][0], 1),
        tile(bounds[0][1], 0),
        tile(bounds[1][1], 1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::MaybeUninit;
    use engine_core::math::{look_at, vulkan_perspective};

    const SIZE: [u32; 2] = [1920, 1080];

    fn camera() -> Mat4 {
        vulkan_perspective(1.0, 16.0 / 9.0, 0.1, 500.0)
            * look_at(Vec3::new(0.0, 2.0, 0.0), Vec3::new(0.0, 2.0, -1.0), Vec3::Y)
    }

    fn point(x: f32, y: f32, z: f32, range: f32) -> LocalLight {
        LocalLight {
            position: Vec3::new(x, y, z),
            range,
            color: Vec3::ONE,
            spot: None,
            shadow: false,
        }
    }

    fn run(lights: &[LocalLight]) -> (LightLayout, Vec<LightData>, Vec<[u32; 2]>, Vec<u32>) {
        let clip = camera();
        let mut scratch = LightScratch::default();
        let layout = cull_lights(
            lights,
            clip,
            ViewAxis::new(clip).unwrap(),
            SIZE,
            &mut scratch,
        );
        let n = layout.count as usize;
        let mut data = vec![MaybeUninit::uninit(); n];
        let mut rects = vec![MaybeUninit::uninit(); n];
        let mut bins = vec![MaybeUninit::uninit(); Z_BINS];
        write_lights(lights, &scratch, &mut data);
        write_rects(&scratch, &mut rects);
        write_bins(lights, &layout, &scratch, &mut bins);
        // SAFETY: every element was written.
        unsafe {
            (
                layout,
                data.into_iter().map(|d| d.assume_init()).collect(),
                rects.into_iter().map(|d| d.assume_init()).collect(),
                bins.into_iter().map(|d| d.assume_init()).collect(),
            )
        }
    }

    #[test]
    fn lights_outside_the_view_are_culled_and_the_rest_sorted() {
        let lights = [
            point(0.0, 2.0, -30.0, 2.0),
            point(0.0, 2.0, 10.0, 2.0), // behind the camera
            point(0.0, 2.0, -5.0, 2.0),
            point(500.0, 2.0, -5.0, 2.0), // far to the right
            point(1.0, 2.0, -12.0, 2.0),
        ];
        let (layout, data, _, _) = run(&lights);
        assert_eq!(layout.count, 3);
        assert_eq!(layout.words, 1);
        let z: Vec<f32> = data.iter().map(|d| d.position_range[2]).collect();
        assert_eq!(z, [-5.0, -12.0, -30.0]);
    }

    #[test]
    fn a_bin_holds_every_light_overlapping_it() {
        let lights: Vec<LocalLight> = (0..200)
            .map(|k| {
                let k = k as f32;
                point(
                    (k * 0.37).sin() * 8.0,
                    2.0,
                    -3.0 - k * 0.9,
                    1.0 + (k * 0.11).cos().abs() * 4.0,
                )
            })
            .collect();
        let (layout, data, _, bins) = run(&lights);
        let axis = ViewAxis::new(camera()).unwrap();
        for (i, d) in data.iter().enumerate() {
            let center = Vec3::from_slice(&d.position_range[..3]);
            let (depth, range) = (axis.depth(center), d.position_range[3]);
            let lo = ((depth - range) * layout.bin_scale).max(0.0) as usize;
            let hi = (((depth + range) * layout.bin_scale).max(0.0) as usize).min(Z_BINS - 1);
            for (b, bin) in bins.iter().enumerate().take(hi + 1).skip(lo) {
                let (low, high) = (bin & 0xFFFF, bin >> 16);
                assert!(low as usize <= i && i <= high as usize, "light {i} bin {b}");
            }
        }
    }

    #[test]
    fn rectangles_cover_the_projected_sphere() {
        let lights = [point(3.0, 1.0, -8.0, 1.5), point(0.0, 2.0, -0.5, 1.0)];
        let (layout, data, rects, _) = run(&lights);
        let clip = camera();
        for (d, r) in data.iter().zip(&rects) {
            let center = Vec3::from_slice(&d.position_range[..3]);
            let range = d.position_range[3];
            // Points on the sphere project inside the rectangle.
            for k in 0..64 {
                let (a, b) = (k as f32 * 0.7, k as f32 * 1.3);
                let p = center + Vec3::new(a.cos() * b.sin(), a.sin() * b.sin(), b.cos()) * range;
                let c = clip * p.extend(1.0);
                if c.w <= 0.0 {
                    continue;
                }
                let px = ((c.x / c.w) * 0.5 + 0.5) * SIZE[0] as f32;
                let py = ((c.y / c.w) * 0.5 + 0.5) * SIZE[1] as f32;
                if !(0.0..SIZE[0] as f32).contains(&px) || !(0.0..SIZE[1] as f32).contains(&py) {
                    continue;
                }
                let (tx, ty) = ((px / 16.0) as u32, (py / 16.0) as u32);
                assert!(tx >= (r[0] & 0xFFFF) && tx <= (r[1] & 0xFFFF), "{tx} {r:?}");
                assert!(ty >= (r[0] >> 16) && ty <= (r[1] >> 16), "{ty} {r:?}");
            }
        }
        // The light around the near plane covers the whole screen.
        assert_eq!(
            rects[0],
            [0, (layout.tiles[0] - 1) | (layout.tiles[1] - 1) << 16]
        );
    }

    #[test]
    fn sphere_bounds_are_tight() {
        // The bounds touch the projected sphere: sampled points reach within 1% of them.
        let clip = camera();
        for (c, r) in [
            (Vec3::new(3.0, 1.0, -8.0), 1.5),
            (Vec3::new(-6.0, 4.0, -20.0), 3.0),
            (Vec3::new(0.0, 2.0, -3.0), 0.5),
        ] {
            let b = sphere_bounds(clip, c, r).unwrap();
            let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
            for i in 0..200 {
                for j in 0..100 {
                    let (a, e) = (i as f32 * 0.0314, j as f32 * 0.0314);
                    let p = c + Vec3::new(a.cos() * e.sin(), a.sin() * e.sin(), e.cos()) * r;
                    let q = clip * p.extend(1.0);
                    for axis in 0..2 {
                        let v = q[axis] / q.w;
                        lo[axis] = lo[axis].min(v);
                        hi[axis] = hi[axis].max(v);
                    }
                }
            }
            for axis in 0..2 {
                let span = b[axis][1] - b[axis][0];
                assert!(lo[axis] >= b[axis][0] - 1e-4 && hi[axis] <= b[axis][1] + 1e-4);
                assert!(
                    (lo[axis] - b[axis][0]).abs() < span * 0.01,
                    "{axis}: {lo:?} {b:?}"
                );
                assert!(
                    (hi[axis] - b[axis][1]).abs() < span * 0.01,
                    "{axis}: {hi:?} {b:?}"
                );
            }
        }
        // Orthographic: the center ± the radius (scaled).
        let ortho = engine_core::math::vulkan_orthographic(-10.0, 10.0, -10.0, 10.0, 0.1, 100.0)
            * look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y);
        let b = sphere_bounds(ortho, Vec3::new(2.0, 0.0, -5.0), 1.0).unwrap();
        assert!(
            (b[0][0] - 0.1).abs() < 1e-5 && (b[0][1] - 0.3).abs() < 1e-5,
            "{b:?}"
        );
        // Around the eye: no bounds.
        assert!(sphere_bounds(clip, Vec3::new(0.0, 2.0, 0.5), 1.0).is_none());
    }

    #[test]
    fn spot_cones_attenuate_between_their_angles() {
        let data = LightData::new(&LocalLight {
            spot: Some(Spot {
                direction: Vec3::NEG_Y,
                inner: 0.3,
                outer: 0.5,
            }),
            ..point(0.0, 0.0, 0.0, 5.0)
        });
        let att = |angle: f32| {
            (angle.cos() * data.color_scale[3] + data.direction_offset[3]).clamp(0.0, 1.0)
        };
        assert_eq!(att(0.0), 1.0);
        assert!((att(0.3) - 1.0).abs() < 1e-4);
        assert!(att(0.5).abs() < 1e-4);
        assert!(att(0.4) > 0.0 && att(0.4) < 1.0);
        let p = LightData::new(&point(0.0, 0.0, 0.0, 5.0));
        assert_eq!((p.color_scale[3], p.direction_offset[3]), (0.0, 1.0));
    }
}
