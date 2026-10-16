//! Shadows of point and spot lights (ADR 0071, docs/research/local-shadows.md).
//!
//! Lights marked as casting shadows ([`crate::lights::LocalLight::shadow`]) get views in
//! one persistent depth **atlas**: a spot light one perspective view around its cone, a
//! point light six cube faces (90°, widened by a border so the filter never reads a
//! neighbouring tile). Tiles are power-of-two squares from a quadtree (buddy) allocator.
//!
//! - **Resolution** follows the light's size on screen: its sphere's projected diameter in
//!   pixels times [`LocalShadowSettings::texels_per_pixel`] (halved for a cube face),
//!   rounded to a power of two between the minimum and the maximum. When the lights in view
//!   want more than the atlas holds, all of them step down together. A light keeps its
//!   resolution until the wish moves past 1.5× or below ⅓ of it, so tiles do not flip
//!   between sizes as the camera moves.
//! - **Cache:** a light's views are drawn again only when the light changed (position,
//!   range, cone) or an instance whose box (before or after the change) touches the light's
//!   sphere was added, moved, edited or removed: the GPU scene's change list
//!   ([`crate::gpu_scene::SceneFrame`]), or for a draw list a comparison with the previous
//!   frame's list by index. A static scene draws nothing.
//! - **Pieces:** when only casters changed, a view is drawn again only where their boxes
//!   (old and new places) project: their rectangles gathered into at most
//!   [`PIECES`] per view, each cleared and drawn through the view's projection narrowed
//!   to it (its viewport, and the culling of what reaches it). A light keeps at most
//!   [`DIRTY_BOXES`] boxes while it waits (more are merged into them). A view the pieces
//!   would cover for the most part, or a box reaching across its eye plane, draws whole.
//! - **Budget:** lights whose shadow is out of date are drawn oldest and largest first,
//!   at most [`LocalShadowSettings::updates_per_frame`] views per frame (a piece counts as
//!   a view: it can cost as much, a large mesh reaching into it drawn whole); the others
//!   keep their previous views (the light's old position included) one more frame. A light seen for the first time
//!   is always drawn: a missing shadow would leak light through walls.
//! - **Eviction:** lights not in view keep their tiles until the space is needed; then
//!   the least recently seen go first.
//!
//! Only lights reaching the view are looked up; the view's shading uses the `*Shadowed`
//! pipelines only while one of them has a shadow, so other views pay nothing.

use crate::cull::Frustum;
use crate::lights::{LocalLight, sphere_bounds};
use crate::shadow_cache::{MAX_RECTS, clusters};
use engine_core::math::{Mat4, Vec2, Vec3, Vec4, look_at, vulkan_perspective};
use engine_rhi::DepthBias;
use engine_rhi::ash::vk;

/// `NO_SHADOW` in `forward.slang`: a light without views.
pub(crate) const NO_SHADOW: u32 = u32::MAX;

/// Texels kept around a view's square for the filter: a cube face or a spot cone is
/// widened so its receivers stay this far inside the tile.
const BORDER: f32 = 2.0;

/// The widest spot cone (half angle) a projection covers; beyond it the cone is lit.
const MAX_SPOT_HALF_ANGLE: f32 = 1.4;

/// World boxes of changed casters a light's shadow keeps while it waits, to draw only
/// their places; more are merged into them (the pair whose union grows least).
pub(crate) const DIRTY_BOXES: usize = 8;

/// Boxes past [`DIRTY_BOXES`] a light's shadow merges before it is drawn whole instead:
/// that many changes in its range cover its views anyway, and each merge searches the
/// boxes (2,064 casters moving in 7 lights' ranges took 0.3 ms a frame, core audit 3).
const MERGES: u16 = 2 * DIRTY_BOXES as u16;

/// Rectangles a view's changed places are gathered into: its pieces drawn again.
pub(crate) const PIECES: usize = 2;

/// A view whose pieces would cover more than this share of it is drawn whole.
const WHOLE_SHARE: f32 = 0.5;

/// Settings of local lights' shadows ([`crate::forward::ForwardRenderer::set_local_shadows`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalShadowSettings {
    /// Side of the square atlas in texels (a power of two).
    pub atlas_size: u32,
    /// Depth format of the atlas: `D16_UNORM` (half the memory and bandwidth; the near
    /// plane at a fiftieth of the range keeps its steps below a centimetre for ranges up to
    /// about 15 m) or `D32_SFLOAT`.
    pub format: vk::Format,
    /// Smallest tile (texels per side, a power of two).
    pub min_resolution: u32,
    /// Largest tile.
    pub max_resolution: u32,
    /// Shadow texels per pixel the light's sphere covers on screen.
    pub texels_per_pixel: f32,
    /// Views (a spot light 1, a cube face 1) redrawn per frame for lights whose shadow is
    /// out of date. Lights seen for the first time are drawn regardless.
    pub updates_per_frame: u32,
    /// How far receivers move along their normal before the lookup, in texels at their
    /// distance (against acne at grazing angles).
    pub normal_bias: f32,
    /// Rasterizer depth bias of the shadow views.
    pub depth_bias: DepthBias,
    /// Near plane of the views as a part of the light's range.
    pub near: f32,
}

impl Default for LocalShadowSettings {
    fn default() -> Self {
        Self {
            atlas_size: 4096,
            format: vk::Format::D16_UNORM,
            min_resolution: 64,
            max_resolution: 1024,
            texels_per_pixel: 0.5,
            updates_per_frame: 12,
            normal_bias: 1.0,
            depth_bias: DepthBias {
                constant: 2.0,
                slope: 2.0,
                clamp: 0.0,
            },
            near: 0.02,
        }
    }
}

impl LocalShadowSettings {
    /// The settings made consistent: powers of two, the minimum at most the maximum, the
    /// maximum at most half the atlas.
    pub(crate) fn sanitized(&self) -> Self {
        let atlas = self.atlas_size.clamp(256, 16384).next_power_of_two();
        let max = self
            .max_resolution
            .clamp(32, atlas / 2)
            .next_power_of_two()
            .min(atlas / 2);
        let min = self
            .min_resolution
            .clamp(16, max)
            .next_power_of_two()
            .min(max);
        Self {
            atlas_size: atlas,
            min_resolution: min,
            max_resolution: max,
            texels_per_pixel: self.texels_per_pixel.max(0.0),
            near: self.near.clamp(1e-4, 0.5),
            ..*self
        }
    }
}

/// What the last frame did with local lights' shadows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalShadowStats {
    /// Shadowed lights reaching the view.
    pub lights_in_view: u32,
    /// Of those, the ones drawn with a shadow.
    pub shadowed: u32,
    /// Lights whose views were drawn this frame (whole or in pieces).
    pub lights_drawn: u32,
    /// Views drawn whole (a cube face counts as one).
    pub views_drawn: u32,
    /// Pieces of views drawn where casters changed, the rest of their views kept.
    pub pieces_drawn: u32,
    /// Lights in view whose shadow is out of date but waits for the budget.
    pub deferred: u32,
    /// Lights evicted from the atlas to make room.
    pub evicted: u32,
    /// Lights in view left without a shadow (no room even at the smallest tile).
    pub without_room: u32,
    /// Texels of the atlas in use.
    pub texels_used: u64,
}

/// A quadtree (buddy) allocator of power-of-two square tiles in a square atlas.
#[derive(Debug)]
pub(crate) struct Atlas {
    size: u32,
    /// Free tiles per level (level `k`: side `size >> k`), as texel corners.
    free: Vec<Vec<[u32; 2]>>,
    used: u64,
}

impl Atlas {
    /// An empty atlas of `size` (a power of two) whose smallest tiles are `min`.
    pub(crate) fn new(size: u32, min: u32) -> Self {
        let levels = (size / min.max(1)).max(1).trailing_zeros() as usize + 1;
        // Room for every tile a level can hold (up to 4096), so freeing never allocates.
        let mut free: Vec<Vec<[u32; 2]>> = (0..levels)
            .map(|k| Vec::with_capacity(1usize << (2 * k).min(12)))
            .collect();
        free[0].push([0, 0]);
        Self {
            size,
            free,
            used: 0,
        }
    }

    fn level(&self, tile: u32) -> Option<usize> {
        if !tile.is_power_of_two() || tile > self.size {
            return None;
        }
        let level = (self.size / tile).trailing_zeros() as usize;
        (level < self.free.len()).then_some(level)
    }

    /// A free tile of side `tile`, splitting a larger one if needed.
    pub(crate) fn alloc(&mut self, tile: u32) -> Option<[u32; 2]> {
        let level = self.level(tile)?;
        let at = self.take(level)?;
        self.used += u64::from(tile) * u64::from(tile);
        Some(at)
    }

    fn take(&mut self, level: usize) -> Option<[u32; 2]> {
        if let Some(at) = self.free[level].pop() {
            return Some(at);
        }
        if level == 0 {
            return None;
        }
        let parent = self.take(level - 1)?;
        let half = self.size >> level;
        // Keep the three others; the lowest corner first in the list is taken last, which
        // keeps allocations packed towards the origin.
        self.free[level].push([parent[0] + half, parent[1] + half]);
        self.free[level].push([parent[0], parent[1] + half]);
        self.free[level].push([parent[0] + half, parent[1]]);
        Some(parent)
    }

    /// Returns a tile; merges it with its three siblings when they are free.
    pub(crate) fn free(&mut self, tile: u32, at: [u32; 2]) {
        let Some(level) = self.level(tile) else {
            return;
        };
        self.used = self.used.saturating_sub(u64::from(tile) * u64::from(tile));
        self.give(level, at);
    }

    fn give(&mut self, level: usize, at: [u32; 2]) {
        if level > 0 {
            let parent_side = self.size >> (level - 1);
            let parent = [
                at[0] / parent_side * parent_side,
                at[1] / parent_side * parent_side,
            ];
            let side = self.size >> level;
            let siblings = [
                [parent[0], parent[1]],
                [parent[0] + side, parent[1]],
                [parent[0], parent[1] + side],
                [parent[0] + side, parent[1] + side],
            ];
            let list = &self.free[level];
            if siblings
                .iter()
                .all(|s| *s == at || list.iter().any(|f| f == s))
            {
                self.free[level].retain(|f| !siblings.contains(f));
                self.give(level - 1, parent);
                return;
            }
        }
        self.free[level].push(at);
    }

    /// Texels in use.
    pub(crate) fn used(&self) -> u64 {
        self.used
    }

    /// Side of the atlas.
    pub(crate) fn size(&self) -> u32 {
        self.size
    }
}

/// `ShadowView` in `forward.slang` (112 bytes): one view of a light's shadow.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct ShadowViewData {
    pub clip_from_world: [f32; 16],
    /// NDC → atlas uv: xy scale, zw offset.
    pub atlas: [f32; 4],
    /// xyz the light's position; w the normal offset per unit of distance.
    pub eye_bias: [f32; 4],
    /// Drawn views: x, y, size of the tile in texels; w pixels per unit at distance 1 over
    /// the LOD threshold. Looked-up point light faces: [`point_terms`] (the shader projects
    /// them without a matrix).
    pub tile: [f32; 4],
}

/// What `local_shadow` in `forward.slang` projects a point light's faces with: 1 /
/// tan(half angle), and the depth's A and B (depth = A + B / distance along the face's
/// axis) of `vulkan_perspective` from `near` to `far`.
pub(crate) fn point_terms(tan_half: f32, near: f32, far: f32) -> [f32; 4] {
    let a = far / (far - near);
    [1.0 / tan_half, a, -near * a, 0.0]
}

const _: () = assert!(size_of::<ShadowViewData>() == 112);

/// What a light's shadow was drawn for: the light's position, range and cone.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Key {
    position: Vec3,
    range: f32,
    /// Direction, inner and outer half angles of a spot light.
    spot: Option<(Vec3, f32)>,
}

impl Key {
    fn of(light: &LocalLight) -> Self {
        Self {
            position: light.position,
            range: light.range,
            spot: light
                .spot
                .map(|s| (s.direction.normalize_or(Vec3::NEG_Y), s.outer.max(s.inner))),
        }
    }

    fn faces(&self) -> usize {
        if self.spot.is_some() { 1 } else { 6 }
    }

    /// The light's sphere touches a box.
    fn touches(&self, b: &[Vec3; 2]) -> bool {
        let nearest = self.position.clamp(b[0], b[1]);
        nearest.distance_squared(self.position) <= self.range * self.range
    }

    /// The views of a light drawn into tiles of side `tile` at `corners`.
    fn views(
        &self,
        tile: u32,
        corners: &[[u32; 2]; 6],
        atlas: u32,
        settings: &LocalShadowSettings,
        lod_threshold: f32,
        out: &mut Vec<ShadowViewData>,
    ) {
        let near = (self.range * settings.near).max(1e-3);
        let far = self.range.max(near * 2.0);
        // Widened so receivers stay BORDER texels inside the tile.
        let widen = tile as f32 / (tile as f32 - 2.0 * BORDER).max(1.0);
        let view_of = |forward: Vec3, tan_half: f32, corner: [u32; 2]| {
            let up = if forward.y.abs() > 0.99 {
                Vec3::Z
            } else {
                Vec3::Y
            };
            let fov = 2.0 * tan_half.atan();
            let clip_from_world = vulkan_perspective(fov, 1.0, near, far)
                * look_at(self.position, self.position + forward, up);
            let a = atlas as f32;
            let half = 0.5 * tile as f32;
            // Pixels per unit at distance 1: half the tile over tan(half angle).
            let lod_scale = if lod_threshold > 0.0 {
                half / tan_half / lod_threshold
            } else {
                0.0
            };
            ShadowViewData {
                clip_from_world: clip_from_world.to_cols_array(),
                atlas: [
                    half / a,
                    half / a,
                    (corner[0] as f32 + half) / a,
                    (corner[1] as f32 + half) / a,
                ],
                eye_bias: self
                    .position
                    .extend(settings.normal_bias * 2.0 * tan_half / tile as f32)
                    .to_array(),
                tile: [corner[0] as f32, corner[1] as f32, tile as f32, lod_scale],
            }
        };
        match self.spot {
            Some((direction, outer)) => {
                let tan_half = outer.clamp(0.01, MAX_SPOT_HALF_ANGLE).tan() * widen;
                out.push(view_of(direction, tan_half, corners[0]));
            }
            None => {
                for (k, forward) in CUBE_FACES.iter().enumerate() {
                    out.push(view_of(*forward, widen, corners[k]));
                }
            }
        }
    }

    /// The point light faces' projection terms ([`point_terms`]) for tiles of side `tile`.
    fn point_terms(&self, tile: u32, settings: &LocalShadowSettings) -> [f32; 4] {
        let near = (self.range * settings.near).max(1e-3);
        let far = self.range.max(near * 2.0);
        let widen = tile as f32 / (tile as f32 - 2.0 * BORDER).max(1.0);
        point_terms(widen, near, far)
    }
}

/// The rectangles of `view`'s tile (texels of the atlas: x0, y0, x1, y1, the last
/// exclusive) where `boxes` project, a texel wider on every side, gathered into at most
/// [`PIECES`] (none where no box reaches the view's frustum). `None`: the view is drawn
/// whole, a box in its frustum reaching across its eye plane or the rectangles covering
/// more than [`WHOLE_SHARE`] of it.
fn pieces_of(view: &ShadowViewData, boxes: &[[Vec3; 2]]) -> Option<([[i32; 4]; MAX_RECTS], usize)> {
    let m = Mat4::from_cols_array(&view.clip_from_world);
    let [tx, ty, size, _] = view.tile;
    // The view's frustum (Gribb and Hartmann, as the GPU's culling).
    let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
    let planes = [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2];
    let mut rects = [[0i32; 4]; DIRTY_BOXES];
    let mut n = 0;
    for b in boxes {
        let (center, extents) = ((b[0] + b[1]) * 0.5, (b[1] - b[0]) * 0.5);
        if planes
            .iter()
            .any(|p| p.truncate().dot(center) + p.truncate().abs().dot(extents) + p.w < 0.0)
        {
            continue;
        }
        let (mut lo, mut hi) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
        let mut behind = 0;
        for corner in 0..8 {
            let p = Vec3::new(
                if corner & 1 == 0 { b[0].x } else { b[1].x },
                if corner & 2 == 0 { b[0].y } else { b[1].y },
                if corner & 4 == 0 { b[0].z } else { b[1].z },
            );
            let c = m * p.extend(1.0);
            if c.w <= 1e-4 {
                behind += 1;
                continue;
            }
            let ndc = Vec2::new(c.x, c.y) / c.w;
            lo = lo.min(ndc);
            hi = hi.max(ndc);
        }
        // Behind the eye plane: not in this view. Across it: no rectangle bounds it.
        if behind == 8 {
            continue;
        }
        if behind > 0 {
            return None;
        }
        if hi.x < -1.0 || lo.x > 1.0 || hi.y < -1.0 || lo.y > 1.0 {
            continue;
        }
        // NDC to texels of the tile (Vulkan's y down, as the viewport maps it).
        let texel = |v: f32, origin: f32| origin + (v.clamp(-1.0, 1.0) + 1.0) * 0.5 * size;
        let x0 = (texel(lo.x, tx).floor() as i32 - 1).max(tx as i32);
        let y0 = (texel(lo.y, ty).floor() as i32 - 1).max(ty as i32);
        let x1 = (texel(hi.x, tx).ceil() as i32 + 1).min((tx + size) as i32);
        let y1 = (texel(hi.y, ty).ceil() as i32 + 1).min((ty + size) as i32);
        if x1 > x0 && y1 > y0 && n < DIRTY_BOXES {
            rects[n] = [x0, y0, x1, y1];
            n += 1;
        }
    }
    let (rects, count) = clusters(&rects[..n], PIECES);
    let area: f32 = rects[..count]
        .iter()
        .map(|r| (r[2] - r[0]) as f32 * (r[3] - r[1]) as f32)
        .sum();
    (area <= WHOLE_SHARE * size * size).then_some((rects, count))
}

/// `view`'s projection narrowed to `rect` (texels of the atlas in its tile, as
/// [`pieces_of`] gives them). Drawn with the rectangle as its viewport it puts every
/// texel where the view does; its frustum is the rectangle's, so culling keeps what
/// reaches it. The level of detail stays the view's.
fn narrowed(view: &ShadowViewData, rect: [i32; 4]) -> ShadowViewData {
    let [tx, ty, size, _] = view.tile;
    let ndc = |t: i32, origin: f32| 2.0 * (t as f32 - origin) / size - 1.0;
    let (x0, x1) = (ndc(rect[0], tx), ndc(rect[2], tx));
    let (y0, y1) = (ndc(rect[1], ty), ndc(rect[3], ty));
    let (cx, hx) = (0.5 * (x0 + x1), 0.5 * (x1 - x0));
    let (cy, hy) = (0.5 * (y0 + y1), 0.5 * (y1 - y0));
    let zoom = Mat4::from_cols(
        Vec4::new(1.0 / hx, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 1.0 / hy, 0.0, 0.0),
        Vec4::new(0.0, 0.0, 1.0, 0.0),
        Vec4::new(-cx / hx, -cy / hy, 0.0, 1.0),
    );
    ShadowViewData {
        clip_from_world: (zoom * Mat4::from_cols_array(&view.clip_from_world)).to_cols_array(),
        ..*view
    }
}

/// The cube faces in the order `forward.slang` picks them: +x, −x, +y, −y, +z, −z.
const CUBE_FACES: [Vec3; 6] = [
    Vec3::X,
    Vec3::NEG_X,
    Vec3::Y,
    Vec3::NEG_Y,
    Vec3::Z,
    Vec3::NEG_Z,
];

/// What of a light's shadow is out of date.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stale {
    /// Nothing.
    No,
    /// Where casters changed within its range: their world boxes, old and new places.
    Boxes {
        count: u8,
        /// Boxes merged into them past [`DIRTY_BOXES`] (past [`MERGES`]: whole).
        merged: u16,
        boxes: [[Vec3; 2]; DIRTY_BOXES],
    },
    /// Everything: its views are drawn whole.
    Whole,
}

impl Stale {
    fn any(&self) -> bool {
        *self != Self::No
    }

    /// Adds a changed box; past [`DIRTY_BOXES`] it is merged into the box whose union with
    /// it is the smallest, and past [`MERGES`] of those the shadow is stale whole.
    fn add(&mut self, b: [Vec3; 2]) {
        match self {
            Self::No => {
                let mut boxes = [[Vec3::ZERO; 2]; DIRTY_BOXES];
                boxes[0] = b;
                *self = Self::Boxes {
                    count: 1,
                    merged: 0,
                    boxes,
                };
            }
            Self::Boxes { merged, .. } if *merged >= MERGES => *self = Self::Whole,
            Self::Boxes {
                count,
                merged,
                boxes,
            } => {
                let n = usize::from(*count);
                if n < DIRTY_BOXES {
                    boxes[n] = b;
                    *count += 1;
                } else {
                    *merged += 1;
                    let union = |a: [Vec3; 2]| [a[0].min(b[0]), a[1].max(b[1])];
                    let volume = |a: [Vec3; 2]| (a[1] - a[0]).max(Vec3::ZERO).element_product();
                    let k = (0..n)
                        .min_by(|&i, &j| {
                            let grow = |k: usize| volume(union(boxes[k])) - volume(boxes[k]);
                            grow(i).total_cmp(&grow(j))
                        })
                        .unwrap_or(0);
                    boxes[k] = union(boxes[k]);
                }
            }
            Self::Whole => {}
        }
    }

    fn boxes(&self) -> &[[Vec3; 2]] {
        match self {
            Self::Boxes { count, boxes, .. } => &boxes[..usize::from(*count)],
            _ => &[],
        }
    }
}

/// A light's shadow in the atlas.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// What the tiles hold.
    key: Key,
    tile: u32,
    corners: [[u32; 2]; 6],
    /// What changed within its range since it was drawn.
    stale: Stale,
    /// Plan serial it was last in view, and last drawn.
    seen: u64,
    drawn: u64,
}

/// A light in view this frame.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    index: u32,
    /// Wished texels per side before the atlas budget.
    wish: f32,
    /// Tile size chosen this frame.
    tile: u32,
    /// Whether it is drawn this frame.
    draw: bool,
    /// The light changed (its views must be drawn again, or it keeps the old ones).
    changed: bool,
    /// Drawn in pieces: its range in [`ShadowCache::pieces`] (first, count).
    pieces: Option<(u32, u32)>,
}

/// Where a view of a light drawn in pieces is drawn this frame.
#[derive(Debug, Clone, Copy)]
struct Piece {
    /// The light's view (its cube face).
    face: u8,
    /// A rectangle of its tile (texels of the atlas: x0, y0, x1, y1); `None`: the whole
    /// view.
    rect: Option<[i32; 4]>,
}

/// A view drawn this frame: its data and its tile.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DrawnView {
    pub data: ShadowViewData,
    pub rect: vk::Rect2D,
}

/// One frame's plan: the views to look up, by light, and the views to draw.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// Views of every light in view with a shadow.
    pub views: Vec<ShadowViewData>,
    /// By light index (the caller's list): its first view in `views`, or [`NO_SHADOW`].
    pub first_view: Vec<u32>,
    /// Views drawn this frame.
    pub drawn: Vec<DrawnView>,
}

/// A draw of the previous frame's list (draw-list path): what it was, to find changes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreviousDraw {
    pub rows: [[f32; 4]; 3],
    pub mesh: crate::mesh::MeshId,
    /// The mesh's shape ([`crate::mesh::MeshInfo::revision`]).
    pub revision: u32,
    pub material: crate::material::MaterialId,
    pub bounds: [Vec3; 2],
}

/// A draw list compared with the previous one, draw by draw (by index): the world boxes
/// of what changed, old and new, for the caches that redraw only that (local lights'
/// shadows, ADR 0071; the sun's cached cascades, ADR 0093).
#[derive(Debug, Default)]
pub(crate) struct DrawDiff {
    previous: Vec<PreviousDraw>,
    /// The last comparison's changed boxes.
    pub(crate) changes: Vec<[Vec3; 2]>,
}

impl DrawDiff {
    /// Compares `draws` with the previous list and keeps them as the next one's: the
    /// changed boxes go to [`Self::changes`]. `mesh` gives a mesh's object-space box
    /// (center, half extents) and its shape's revision (a skinned mesh posed differently
    /// changes shape).
    pub(crate) fn diff(
        &mut self,
        draws: &[crate::forward::DrawItem],
        mesh: impl Fn(crate::mesh::MeshId) -> (Vec3, Vec3, u32),
    ) {
        let changes = &mut self.changes;
        changes.clear();
        // Two boxes per draw at most.
        changes.reserve(2 * draws.len().max(self.previous.len()));
        for (i, draw) in draws.iter().enumerate() {
            let t = draw.world_from_object.transpose();
            let rows = [
                t.x_axis.to_array(),
                t.y_axis.to_array(),
                t.z_axis.to_array(),
            ];
            let (center, extents, revision) = mesh(draw.mesh);
            let same = self.previous.get(i).is_some_and(|p| {
                p.mesh == draw.mesh
                    && p.revision == revision
                    && p.material == draw.material
                    && p.rows
                        .iter()
                        .flatten()
                        .zip(rows.iter().flatten())
                        .all(|(a, b)| a.to_bits() == b.to_bits())
            });
            if same {
                continue;
            }
            let (center, extents) =
                crate::cull::transform_box(&draw.world_from_object, center, extents);
            let bounds = [center - extents, center + extents];
            let entry = PreviousDraw {
                rows,
                mesh: draw.mesh,
                revision,
                material: draw.material,
                bounds,
            };
            match self.previous.get_mut(i) {
                Some(p) => {
                    changes.push(p.bounds);
                    *p = entry;
                }
                None => self.previous.push(entry),
            }
            changes.push(bounds);
        }
        for p in self.previous.iter().skip(draws.len()) {
            changes.push(p.bounds);
        }
        self.previous.truncate(draws.len());
    }
}

/// How a frame's changes relate to the last frame's ([`ShadowCache::begin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Continuity {
    /// The same changes again: nothing new.
    Same,
    /// The next frame: its changes apply.
    Next,
    /// Unrelated: every shadow is stale.
    Broken,
}

/// Where the last frame's changes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeSource {
    /// A draw list, compared with the previous one.
    Draws,
    /// A GPU scene upload with this serial.
    Scene(u64),
}

/// The cache of local lights' shadows (see the module documentation).
#[derive(Debug)]
pub(crate) struct ShadowCache {
    settings: LocalShadowSettings,
    atlas: Atlas,
    /// By light index.
    entries: Vec<Option<Entry>>,
    serial: u64,
    /// What the last frame's changes came from, and the material table's revision.
    source: Option<ChangeSource>,
    material_revision: Option<u64>,
    /// The draw-list path's previous list, and this frame's changed boxes.
    draws: DrawDiff,
    /// Unions of runs of changed boxes ([`Self::invalidate`]).
    chunk_unions: Vec<[Vec3; 2]>,
    candidates: Vec<Candidate>,
    order: Vec<u32>,
    /// The pieces of the lights drawn in pieces this frame ([`Candidate::pieces`]).
    pieces: Vec<Piece>,
    /// A light's views (scratch).
    face_views: Vec<ShadowViewData>,
    stats: LocalShadowStats,
}

impl ShadowCache {
    pub(crate) fn new(settings: LocalShadowSettings) -> Self {
        let settings = settings.sanitized();
        Self {
            settings,
            atlas: Atlas::new(settings.atlas_size, settings.min_resolution),
            entries: Vec::new(),
            serial: 0,
            source: None,
            material_revision: None,
            draws: DrawDiff::default(),
            // Up to 65,536 changes without growing.
            chunk_unions: Vec::with_capacity(1024),
            candidates: Vec::new(),
            order: Vec::new(),
            pieces: Vec::new(),
            face_views: Vec::with_capacity(6),
            stats: LocalShadowStats::default(),
        }
    }

    pub(crate) fn settings(&self) -> &LocalShadowSettings {
        &self.settings
    }

    pub(crate) fn stats(&self) -> LocalShadowStats {
        self.stats
    }

    /// Drops every shadow (they are drawn again when next in view).
    pub(crate) fn clear(&mut self) {
        for entry in self.entries.iter_mut() {
            if let Some(e) = entry.take() {
                for corner in &e.corners[..e.key.faces()] {
                    self.atlas.free(e.tile, *corner);
                }
            }
        }
    }

    /// How this frame's changes relate to the last frame's: the same scene upload again
    /// (another view of the frame: nothing new), the next one or the next draw list (its
    /// changes apply), or anything else (every shadow is stale: a skipped upload, another
    /// source, edited materials).
    pub(crate) fn begin(&mut self, source: ChangeSource, material_revision: u64) -> Continuity {
        let same_materials = self.material_revision == Some(material_revision);
        let continuity = match (self.source, source) {
            _ if !same_materials => Continuity::Broken,
            (Some(ChangeSource::Draws), ChangeSource::Draws) => Continuity::Next,
            (Some(ChangeSource::Scene(a)), ChangeSource::Scene(b)) if b == a => Continuity::Same,
            (Some(ChangeSource::Scene(a)), ChangeSource::Scene(b)) if b == a + 1 => {
                Continuity::Next
            }
            _ => Continuity::Broken,
        };
        self.source = Some(source);
        self.material_revision = Some(material_revision);
        continuity
    }

    /// Compares a draw list with the previous one (by index) and marks the shadows its
    /// changes touch stale; `mesh` gives a mesh's object-space box (center, half extents)
    /// and its shape's revision (a skinned mesh posed differently changes shape). `all`:
    /// every shadow is stale anyway.
    pub(crate) fn diff_draws(
        &mut self,
        draws: &[crate::forward::DrawItem],
        mesh: impl Fn(crate::mesh::MeshId) -> (Vec3, Vec3, u32),
        all: bool,
    ) {
        self.draws.diff(draws, mesh);
        let changes = core::mem::take(&mut self.draws.changes);
        self.invalidate(&changes, all);
        self.draws.changes = changes;
    }

    /// Marks the shadows touched by `changes` stale (or all of them).
    pub(crate) fn invalidate(&mut self, changes: &[[Vec3; 2]], all: bool) {
        if !all && changes.is_empty() {
            return;
        }
        // Unions of runs of CHUNK changes, then of everything: a light tests a run's
        // boxes only when it touches the run's union (many moving instances, many lights).
        const CHUNK: usize = 64;
        let union_of = |boxes: &[[Vec3; 2]]| {
            let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for b in boxes {
                lo = lo.min(b[0]);
                hi = hi.max(b[1]);
            }
            [lo, hi]
        };
        let mut chunks = core::mem::take(&mut self.chunk_unions);
        chunks.clear();
        chunks.extend(changes.chunks(CHUNK).map(union_of));
        let union = union_of(&chunks);
        for entry in self.entries.iter_mut().flatten() {
            if entry.stale == Stale::Whole {
                continue;
            }
            if all {
                entry.stale = Stale::Whole;
                continue;
            }
            if !entry.key.touches(&union) {
                continue;
            }
            // The boxes it keeps, to draw only their places, until it is stale whole.
            'chunks: for (k, u) in chunks.iter().enumerate() {
                if !entry.key.touches(u) {
                    continue;
                }
                for b in &changes[k * CHUNK..((k + 1) * CHUNK).min(changes.len())] {
                    if entry.key.touches(b) {
                        entry.stale.add(*b);
                        if entry.stale == Stale::Whole {
                            break 'chunks;
                        }
                    }
                }
            }
        }
        self.chunk_unions = chunks;
    }

    /// Plans this frame: which shadowed `lights` reach the view of `clip_from_world`
    /// (`size` pixels), their tiles, which are drawn, and their views (into `plan`).
    pub(crate) fn plan(
        &mut self,
        lights: &[LocalLight],
        clip_from_world: Mat4,
        size: [u32; 2],
        lod_threshold: f32,
        plan: &mut Plan,
    ) {
        self.serial += 1;
        let serial = self.serial;
        let settings = self.settings;
        plan.views.clear();
        plan.drawn.clear();
        plan.first_view.clear();
        plan.first_view.resize(lights.len(), NO_SHADOW);
        // Room for every light's views (and pieces), so later frames do not grow them.
        let most = lights.len() * 6;
        plan.views.reserve(most);
        plan.drawn.reserve(most * PIECES);
        self.candidates.reserve(lights.len());
        self.order.reserve(lights.len());
        self.pieces.clear();
        self.pieces.reserve(most * PIECES);
        let mut stats = LocalShadowStats::default();

        // Entries of lights that are gone or no longer cast shadows.
        if self.entries.len() < lights.len() {
            self.entries.resize(lights.len(), None);
        }
        for (i, slot) in self.entries.iter_mut().enumerate() {
            let keep = lights.get(i).is_some_and(|l| l.shadow);
            if !keep && let Some(e) = slot.take() {
                for corner in &e.corners[..e.key.faces()] {
                    self.atlas.free(e.tile, *corner);
                }
            }
        }

        // The shadowed lights in view and the resolution they wish for.
        let frustum = Frustum::from_clip_from_world(clip_from_world);
        let pixels = size[0].max(size[1]) as f32;
        self.candidates.clear();
        for (index, light) in lights.iter().enumerate() {
            let valid = light.shadow
                && light.position.is_finite()
                && light.range.is_finite()
                && light.range > 0.0
                && light.color.max_element() > 0.0;
            if !valid || !frustum.intersects_sphere(light.position, light.range) {
                continue;
            }
            // The sphere's diameter on screen; the whole screen when it reaches the eye.
            let diameter = match sphere_bounds(clip_from_world, light.position, light.range) {
                Some(b) => {
                    let w = (b[0][1].min(1.0) - b[0][0].max(-1.0)).max(0.0) * 0.5;
                    let h = (b[1][1].min(1.0) - b[1][0].max(-1.0)).max(0.0) * 0.5;
                    (w * size[0] as f32).max(h * size[1] as f32)
                }
                None => pixels,
            };
            let per_face = if light.spot.is_some() { 1.0 } else { 0.5 };
            self.candidates.push(Candidate {
                index: index as u32,
                wish: (diameter * settings.texels_per_pixel * per_face).max(1.0),
                tile: 0,
                draw: false,
                changed: false,
                pieces: None,
            });
        }
        stats.lights_in_view = self.candidates.len() as u32;

        // The atlas budget: everyone steps down together until the wishes fit in three
        // quarters of it (room for the allocator's fragmentation).
        let fit = |wish: f32, shift: u32| -> u32 {
            let scaled = wish / (1u32 << shift) as f32;
            (scaled.ceil() as u32)
                .next_power_of_two()
                .clamp(settings.min_resolution, settings.max_resolution)
        };
        let capacity = u64::from(settings.atlas_size).pow(2) * 3 / 4;
        let atlas_size = self.atlas.size();
        let mut shift = 0;
        while shift < 16 {
            let total: u64 = self
                .candidates
                .iter()
                .map(|c| {
                    let t = u64::from(fit(c.wish, shift));
                    t * t * self.faces_of(lights, c.index) as u64
                })
                .sum();
            if total <= capacity {
                break;
            }
            shift += 1;
        }
        for c in self.candidates.iter_mut() {
            let scaled = c.wish / (1u32 << shift) as f32;
            let target = fit(c.wish, shift);
            let light = &lights[c.index as usize];
            c.tile = target;
            // In view: not evicted for another light's room this frame.
            if let Some(e) = self.entries[c.index as usize].as_mut() {
                e.seen = serial;
            }
            match &self.entries[c.index as usize] {
                Some(e) => {
                    // Hysteresis: keep the tile while the wish stays within (⅓, 1.5×].
                    let current = e.tile as f32;
                    if e.tile >= settings.min_resolution
                        && e.tile <= settings.max_resolution
                        && scaled <= current * 1.5
                        && scaled > current / 3.0
                    {
                        c.tile = e.tile;
                    }
                    c.changed = e.key != Key::of(light);
                }
                None => c.draw = true,
            }
        }

        // Out-of-date shadows within the budget: oldest and largest first. New ones are
        // drawn regardless and use the budget first.
        let mut budget = i64::from(settings.updates_per_frame);
        for c in self.candidates.iter().filter(|c| c.draw) {
            budget -= self.faces_of(lights, c.index) as i64;
        }
        self.order.clear();
        for (k, c) in self.candidates.iter().enumerate() {
            if c.draw {
                continue;
            }
            let Some(e) = &self.entries[c.index as usize] else {
                continue;
            };
            if c.changed || e.stale.any() || c.tile != e.tile {
                self.order.push(k as u32);
            }
        }
        let priority = |c: &Candidate, e: &Entry| (serial - e.drawn) as f32 * c.wish;
        {
            let (candidates, entries) = (&self.candidates, &self.entries);
            self.order.sort_unstable_by(|&a, &b| {
                let (ca, cb) = (&candidates[a as usize], &candidates[b as usize]);
                let pa = entries[ca.index as usize].map_or(0.0, |e| priority(ca, &e));
                let pb = entries[cb.index as usize].map_or(0.0, |e| priority(cb, &e));
                pb.total_cmp(&pa).then(a.cmp(&b))
            });
        }
        for k in 0..self.order.len() {
            let c = self.order[k] as usize;
            let candidate = self.candidates[c];
            let faces = self.faces_of(lights, candidate.index) as i64;
            // Only casters changed: the pieces of its views they touch.
            let pieces = self.entries[candidate.index as usize]
                .filter(|e| !candidate.changed && candidate.tile == e.tile)
                .and_then(|e| {
                    let first = self.pieces.len();
                    let added = self.add_pieces(&e, atlas_size, lod_threshold);
                    added.then_some((first, self.pieces.len()))
                });
            // A piece costs as a view: a large mesh reaching into it is drawn whole (its
            // vertices), only its texels are fewer.
            let cost = match pieces {
                Some((first, end)) => (end - first) as i64,
                None => faces,
            };
            if cost <= budget {
                budget -= cost;
                self.candidates[c].draw = true;
                self.candidates[c].pieces =
                    pieces.map(|(first, end)| (first as u32, (end - first) as u32));
            } else {
                if let Some((first, _)) = pieces {
                    self.pieces.truncate(first);
                }
                stats.deferred += 1;
            }
        }

        // Tiles for the drawn lights, largest first. A light keeping its size keeps its
        // tiles; a new size frees the old ones first.
        self.order.clear();
        self.order.extend(
            self.candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| c.draw)
                .map(|(k, _)| k as u32),
        );
        {
            let candidates = &self.candidates;
            self.order.sort_unstable_by(|&a, &b| {
                candidates[b as usize]
                    .tile
                    .cmp(&candidates[a as usize].tile)
                    .then(a.cmp(&b))
            });
        }
        for k in 0..self.order.len() {
            let c = self.order[k] as usize;
            let candidate = self.candidates[c];
            let index = candidate.index as usize;
            let light = &lights[index];
            let key = Key::of(light);
            let faces = key.faces();
            let kept = self.entries[index].filter(|e| e.tile == candidate.tile);
            let corners = match kept {
                Some(e) if e.key.faces() == faces => Some((e.tile, e.corners)),
                _ => {
                    if let Some(e) = self.entries[index].take() {
                        for corner in &e.corners[..e.key.faces()] {
                            self.atlas.free(e.tile, *corner);
                        }
                    }
                    self.allocate(candidate.tile, faces, serial, &mut stats)
                }
            };
            match corners {
                Some((tile, corners)) => {
                    self.candidates[c].tile = tile;
                    self.entries[index] = Some(Entry {
                        key,
                        tile,
                        corners,
                        stale: Stale::No,
                        seen: serial,
                        drawn: serial,
                    });
                }
                None => {
                    self.candidates[c].draw = false;
                    stats.without_room += 1;
                }
            }
        }

        // The views: every light in view with a shadow; the drawn ones also to draw.
        let atlas = self.atlas.size();
        for c in self.candidates.iter() {
            let Some(entry) = self.entries[c.index as usize].as_mut() else {
                continue;
            };
            entry.seen = serial;
            let first = plan.views.len();
            plan.first_view[c.index as usize] = first as u32;
            entry.key.views(
                entry.tile,
                &entry.corners,
                atlas,
                &settings,
                lod_threshold,
                &mut plan.views,
            );
            stats.shadowed += 1;
            if c.draw {
                stats.lights_drawn += 1;
                let whole = |view: &ShadowViewData| DrawnView {
                    data: *view,
                    rect: vk::Rect2D {
                        offset: vk::Offset2D {
                            x: view.tile[0] as i32,
                            y: view.tile[1] as i32,
                        },
                        extent: vk::Extent2D {
                            width: view.tile[2] as u32,
                            height: view.tile[2] as u32,
                        },
                    },
                };
                match c.pieces {
                    None => {
                        plan.drawn.extend(plan.views[first..].iter().map(whole));
                        stats.views_drawn += (plan.views.len() - first) as u32;
                    }
                    Some((at, count)) => {
                        for piece in &self.pieces[at as usize..(at + count) as usize] {
                            let view = &plan.views[first + usize::from(piece.face)];
                            match piece.rect {
                                None => {
                                    plan.drawn.push(whole(view));
                                    stats.views_drawn += 1;
                                }
                                Some(r) => {
                                    plan.drawn.push(DrawnView {
                                        data: narrowed(view, r),
                                        rect: vk::Rect2D {
                                            offset: vk::Offset2D { x: r[0], y: r[1] },
                                            extent: vk::Extent2D {
                                                width: (r[2] - r[0]) as u32,
                                                height: (r[3] - r[1]) as u32,
                                            },
                                        },
                                    });
                                    stats.pieces_drawn += 1;
                                }
                            }
                        }
                    }
                }
            }
            // Point light faces are looked up without their matrices.
            if entry.key.spot.is_none() {
                let terms = entry.key.point_terms(entry.tile, &settings);
                for view in &mut plan.views[first..] {
                    view.tile = terms;
                }
            }
        }
        stats.texels_used = self.atlas.used();
        self.stats = stats;
    }

    /// Adds the pieces of the views of `entry`'s light its changed boxes touch to
    /// [`Self::pieces`]; false when it is drawn whole (stale whole or more than
    /// [`DIRTY_BOXES`] changes, nothing added).
    fn add_pieces(&mut self, entry: &Entry, atlas: u32, lod_threshold: f32) -> bool {
        let Stale::Boxes { .. } = entry.stale else {
            return false;
        };
        self.face_views.clear();
        entry.key.views(
            entry.tile,
            &entry.corners,
            atlas,
            &self.settings,
            lod_threshold,
            &mut self.face_views,
        );
        for (face, view) in self.face_views.iter().enumerate() {
            match pieces_of(view, entry.stale.boxes()) {
                None => self.pieces.push(Piece {
                    face: face as u8,
                    rect: None,
                }),
                Some((rects, count)) => {
                    self.pieces.extend(rects[..count].iter().map(|r| Piece {
                        face: face as u8,
                        rect: Some(*r),
                    }));
                }
            }
        }
        true
    }

    fn faces_of(&self, lights: &[LocalLight], index: u32) -> usize {
        if lights[index as usize].spot.is_some() {
            1
        } else {
            6
        }
    }

    /// `faces` tiles of side `tile` (or smaller, down to the minimum), evicting lights not
    /// seen this frame, least recently seen first, while there is no room.
    fn allocate(
        &mut self,
        tile: u32,
        faces: usize,
        serial: u64,
        stats: &mut LocalShadowStats,
    ) -> Option<(u32, [[u32; 2]; 6])> {
        let mut size = tile;
        loop {
            loop {
                if let Some(corners) = self.try_allocate(size, faces) {
                    return Some((size, corners));
                }
                // The least recently seen light not in view this frame.
                let victim = self
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| e.as_ref().map(|e| (i, e.seen)))
                    .filter(|&(_, seen)| seen < serial)
                    .min_by_key(|&(i, seen)| (seen, i));
                let Some((victim, _)) = victim else {
                    break;
                };
                if let Some(e) = self.entries[victim].take() {
                    for corner in &e.corners[..e.key.faces()] {
                        self.atlas.free(e.tile, *corner);
                    }
                    stats.evicted += 1;
                }
            }
            if size <= self.settings.min_resolution {
                return None;
            }
            size /= 2;
        }
    }

    /// All `faces` tiles or none.
    fn try_allocate(&mut self, tile: u32, faces: usize) -> Option<[[u32; 2]; 6]> {
        let mut corners = [[0u32; 2]; 6];
        for k in 0..faces {
            match self.atlas.alloc(tile) {
                Some(at) => corners[k] = at,
                None => {
                    for corner in &corners[..k] {
                        self.atlas.free(tile, *corner);
                    }
                    return None;
                }
            }
        }
        Some(corners)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lights::Spot;

    #[test]
    fn the_atlas_splits_and_merges_tiles() {
        let mut atlas = Atlas::new(1024, 64);
        let a = atlas.alloc(512).unwrap();
        let b = atlas.alloc(256).unwrap();
        let c = atlas.alloc(64).unwrap();
        // Distinct, inside, aligned to their size.
        for (at, side) in [(a, 512), (b, 256), (c, 64)] {
            assert_eq!(at[0] % side, 0);
            assert_eq!(at[1] % side, 0);
            assert!(at[0] + side <= 1024 && at[1] + side <= 1024);
        }
        assert_eq!(atlas.used(), 512 * 512 + 256 * 256 + 64 * 64);
        // Room for exactly what is left of the 512 quadrants: two more 512s do not fit.
        assert!(atlas.alloc(512).is_some());
        assert!(atlas.alloc(512).is_some());
        assert!(atlas.alloc(512).is_none());
        atlas.free(64, c);
        atlas.free(256, b);
        // The quadrant b and c came from merged back: a 512 fits again.
        assert!(atlas.alloc(512).is_some());
        assert!(atlas.alloc(64).is_none());
        // Everything freed: the whole atlas is one tile again.
        let mut full = Atlas::new(256, 16);
        let tiles: Vec<_> = (0..256).map(|_| full.alloc(16).unwrap()).collect();
        assert!(full.alloc(16).is_none());
        for t in tiles {
            full.free(16, t);
        }
        assert_eq!(full.used(), 0);
        assert_eq!(full.alloc(256), Some([0, 0]));
    }

    #[test]
    fn tiles_never_overlap() {
        let mut atlas = Atlas::new(512, 16);
        let mut live: Vec<(u32, [u32; 2])> = Vec::new();
        let mut seed = 12345u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..2000 {
            if next() % 3 == 0 && !live.is_empty() {
                let k = next() as usize % live.len();
                let (side, at) = live.swap_remove(k);
                atlas.free(side, at);
            } else {
                let side = 16 << (next() % 4);
                if let Some(at) = atlas.alloc(side) {
                    for &(s, b) in &live {
                        let apart = at[0] + side <= b[0]
                            || b[0] + s <= at[0]
                            || at[1] + side <= b[1]
                            || b[1] + s <= at[1];
                        assert!(apart, "{at:?}/{side} overlaps {b:?}/{s}");
                    }
                    live.push((side, at));
                }
            }
            let used: u64 = live.iter().map(|&(s, _)| u64::from(s * s)).sum();
            assert_eq!(atlas.used(), used);
        }
    }

    fn camera() -> Mat4 {
        vulkan_perspective(1.0, 16.0 / 9.0, 0.1, 500.0)
            * look_at(Vec3::new(0.0, 2.0, 5.0), Vec3::new(0.0, 1.0, 0.0), Vec3::Y)
    }

    fn light(x: f32, spot: bool) -> LocalLight {
        LocalLight {
            position: Vec3::new(x, 2.0, 0.0),
            range: 4.0,
            color: Vec3::ONE,
            spot: spot.then_some(Spot {
                direction: Vec3::NEG_Y,
                inner: 0.3,
                outer: 0.6,
            }),
            shadow: true,
        }
    }

    const SIZE: [u32; 2] = [1920, 1080];

    fn frame(cache: &mut ShadowCache, lights: &[LocalLight], changes: &[[Vec3; 2]]) -> Plan {
        cache.invalidate(changes, false);
        let mut plan = Plan::default();
        cache.plan(lights, camera(), SIZE, 1.0, &mut plan);
        plan
    }

    #[test]
    fn a_static_scene_draws_each_shadow_once() {
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let lights = [light(-2.0, false), light(2.0, true)];
        let plan = frame(&mut cache, &lights, &[]);
        assert_eq!(plan.drawn.len(), 7);
        assert_eq!(plan.views.len(), 7);
        assert_eq!(plan.first_view, [0, 6]);
        for _ in 0..10 {
            let plan = frame(&mut cache, &lights, &[]);
            assert_eq!(plan.drawn.len(), 0, "{:?}", cache.stats());
            assert_eq!(plan.views.len(), 7);
        }
    }

    #[test]
    fn a_moving_light_or_caster_redraws_only_what_it_touches() {
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let mut lights = [light(-5.0, false), light(5.0, true)];
        frame(&mut cache, &lights, &[]);
        // A box moving near the spot light only.
        let near_spot = [Vec3::new(4.5, 0.0, -0.5), Vec3::new(5.5, 1.0, 0.5)];
        let plan = frame(&mut cache, &lights, &[near_spot]);
        assert_eq!(plan.drawn.len(), 1);
        assert_eq!(cache.stats().lights_drawn, 1);
        // Far from both: nothing.
        let far = [Vec3::new(40.0, 0.0, 0.0), Vec3::new(41.0, 1.0, 1.0)];
        assert_eq!(frame(&mut cache, &lights, &[far]).drawn.len(), 0);
        // The point light moves: its six faces.
        lights[0].position.x += 0.1;
        let plan = frame(&mut cache, &lights, &[]);
        assert_eq!(plan.drawn.len(), 6);
        assert_eq!(frame(&mut cache, &lights, &[]).drawn.len(), 0);
        // A light that stops casting shadows frees its tiles.
        let used = cache.stats().texels_used;
        lights[0].shadow = false;
        let plan = frame(&mut cache, &lights, &[]);
        assert_eq!(plan.first_view[0], NO_SHADOW);
        assert!(cache.stats().texels_used < used);
    }

    #[test]
    fn many_changes_find_the_one_near_a_light() {
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let lights = [light(-5.0, false), light(5.0, true)];
        frame(&mut cache, &lights, &[]);
        // 1,000 changes far away, one near the spot light deep in the list.
        let mut changes: Vec<[Vec3; 2]> = (0..1000)
            .map(|k| {
                let c = Vec3::new(100.0 + k as f32, 0.0, 50.0);
                [c - Vec3::ONE, c + Vec3::ONE]
            })
            .collect();
        assert_eq!(frame(&mut cache, &lights, &changes).drawn.len(), 0);
        changes[700] = [Vec3::new(4.5, 0.0, -0.5), Vec3::new(5.5, 1.0, 0.5)];
        assert_eq!(frame(&mut cache, &lights, &changes).drawn.len(), 1);
    }

    #[test]
    fn a_moved_caster_redraws_a_piece_of_the_view_it_reaches() {
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let lights = [light(0.0, true)];
        frame(&mut cache, &lights, &[]);
        // A small box under the spot light (it looks down from 2 m).
        let b = [Vec3::new(0.3, 0.0, 0.2), Vec3::new(0.5, 0.2, 0.4)];
        let plan = frame(&mut cache, &lights, &[b]);
        let s = cache.stats();
        assert_eq!((s.lights_drawn, s.views_drawn, s.pieces_drawn), (1, 0, 1));
        let piece = plan.drawn[0];
        let tile = plan.views[0].tile;
        let r = piece.rect;
        let (x0, y0) = (r.offset.x as f32, r.offset.y as f32);
        let (w, h) = (r.extent.width as f32, r.extent.height as f32);
        assert!(x0 >= tile[0] && y0 >= tile[1]);
        assert!(x0 + w <= tile[0] + tile[2] && y0 + h <= tile[1] + tile[2]);
        assert!(
            w * h * 4.0 < tile[2] * tile[2],
            "a piece, not the view: {r:?}"
        );
        // Its projection through its rectangle puts the box where the view puts it.
        let full = Mat4::from_cols_array(&plan.views[0].clip_from_world);
        let narrow = Mat4::from_cols_array(&piece.data.clip_from_world);
        let texel = |m: Mat4, p: Vec3, origin: Vec2, size: Vec2| {
            let c = m * p.extend(1.0);
            origin + (Vec2::new(c.x, c.y) / c.w + 1.0) * 0.5 * size
        };
        for p in [b[0], b[1], (b[0] + b[1]) * 0.5] {
            let a = texel(full, p, Vec2::new(tile[0], tile[1]), Vec2::splat(tile[2]));
            let n = texel(narrow, p, Vec2::new(x0, y0), Vec2::new(w, h));
            assert!((a - n).length() < 1e-2, "{a} against {n}");
            assert!(n.x > x0 && n.x < x0 + w && n.y > y0 && n.y < y0 + h);
        }
        assert!(frame(&mut cache, &lights, &[]).drawn.is_empty());
    }

    #[test]
    fn a_point_light_draws_pieces_only_of_the_faces_a_box_shows_in() {
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let lights = [light(0.0, false)];
        frame(&mut cache, &lights, &[]);
        // Beside and below the light: in its bottom face only.
        let beside = [Vec3::new(1.0, 0.5, -0.1), Vec3::new(1.2, 0.7, 0.1)];
        frame(&mut cache, &lights, &[beside]);
        let s = cache.stats();
        assert_eq!((s.views_drawn, s.pieces_drawn), (0, 1));
        // Around the light: across every face's eye plane, every face whole.
        let around = [Vec3::new(-0.2, 1.8, -0.2), Vec3::new(0.2, 2.2, 0.2)];
        frame(&mut cache, &lights, &[around]);
        let s = cache.stats();
        assert_eq!((s.views_drawn, s.pieces_drawn), (6, 0));
    }

    #[test]
    fn changes_past_the_limit_are_merged() {
        let mut stale = Stale::No;
        let boxes: Vec<[Vec3; 2]> = (0..DIRTY_BOXES + 3)
            .map(|k| {
                let x = k as f32;
                [Vec3::new(x, 0.0, 0.0), Vec3::new(x + 0.5, 0.5, 0.5)]
            })
            .collect();
        for b in &boxes {
            stale.add(*b);
        }
        let kept = stale.boxes();
        assert_eq!(kept.len(), DIRTY_BOXES);
        for b in &boxes {
            assert!(
                kept.iter()
                    .any(|k| k[0].cmple(b[0]).all() && k[1].cmpge(b[1]).all()),
                "{b:?} kept"
            );
        }
    }

    #[test]
    fn too_many_changes_in_a_light_s_range_redraw_it_whole() {
        let mut stale = Stale::No;
        let small = |k: usize| {
            let x = (k % 16) as f32 * 0.2;
            [Vec3::new(x, 0.0, 0.0), Vec3::new(x + 0.1, 0.1, 0.1)]
        };
        for k in 0..DIRTY_BOXES + usize::from(MERGES) {
            stale.add(small(k));
        }
        assert!(matches!(stale, Stale::Boxes { .. }));
        stale.add(small(0));
        assert_eq!(stale, Stale::Whole);
        // A crowd moving under a spot light: its view drawn whole, at the cost of a few
        // boxes rather than every one.
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let lights = [light(0.0, true)];
        frame(&mut cache, &lights, &[]);
        let crowd: Vec<[Vec3; 2]> = (0..2000).map(small).collect();
        let plan = frame(&mut cache, &lights, &crowd);
        let s = cache.stats();
        assert_eq!((s.views_drawn, s.pieces_drawn), (1, 0));
        assert_eq!(plan.drawn.len(), 1);
    }

    #[test]
    fn pieces_count_as_views_in_the_budget() {
        let settings = LocalShadowSettings {
            updates_per_frame: 1,
            ..LocalShadowSettings::default()
        };
        let mut cache = ShadowCache::new(settings);
        let lights = [light(-1.5, true), light(1.5, true)];
        frame(&mut cache, &lights, &[]);
        let under = |x: f32| [Vec3::new(x - 0.1, 0.0, -0.1), Vec3::new(x + 0.1, 0.2, 0.1)];
        let plan = frame(&mut cache, &lights, &[under(-1.5), under(1.5)]);
        assert_eq!(plan.drawn.len(), 1);
        assert_eq!(cache.stats().deferred, 1);
        assert_eq!(
            frame(&mut cache, &lights, &[]).drawn.len(),
            1,
            "the other's turn"
        );
        assert!(frame(&mut cache, &lights, &[]).drawn.is_empty());
    }

    #[test]
    fn updates_wait_for_the_budget_and_new_lights_do_not() {
        let settings = LocalShadowSettings {
            updates_per_frame: 6,
            ..LocalShadowSettings::default()
        };
        let mut cache = ShadowCache::new(settings);
        let mut lights: Vec<LocalLight> =
            (0..4).map(|k| light(k as f32 * 3.0 - 4.5, false)).collect();
        // Four new point lights: 24 views, beyond the budget, all drawn.
        assert_eq!(frame(&mut cache, &lights, &[]).drawn.len(), 24);
        // All four move: one per frame (6 views), every one in four frames.
        for l in lights.iter_mut() {
            l.position.y += 0.5;
        }
        let mut drawn = Vec::new();
        for _ in 0..4 {
            let plan = frame(&mut cache, &lights, &[]);
            assert_eq!(plan.drawn.len(), 6);
            assert_eq!(cache.stats().deferred, 3 - drawn.len() as u32);
            drawn.push(plan.drawn[0].data.eye_bias);
            // The others still have views (their old ones).
            assert!(plan.first_view.iter().all(|&f| f != NO_SHADOW));
        }
        drawn.sort_by(|a, b| a[0].total_cmp(&b[0]));
        drawn.dedup();
        assert_eq!(drawn.len(), 4, "every light got its turn");
        assert_eq!(frame(&mut cache, &lights, &[]).drawn.len(), 0);
    }

    #[test]
    fn lights_out_of_view_are_evicted_when_room_is_needed() {
        let settings = LocalShadowSettings {
            atlas_size: 1024,
            min_resolution: 256,
            max_resolution: 512,
            ..LocalShadowSettings::default()
        };
        let mut cache = ShadowCache::new(settings);
        // Two spot lights at 512 fill half of the atlas each... but the budget keeps
        // three quarters: they step down to 256.
        let mut lights = vec![light(-1.0, true), light(1.0, true)];
        frame(&mut cache, &lights, &[]);
        assert_eq!(cache.stats().shadowed, 2);
        // Move them out of view and bring a point light (6 × 256²: 6/16 of the atlas) and
        // more: the old ones go when room runs out.
        for l in lights.iter_mut() {
            l.position.z = 1000.0;
        }
        lights.extend((0..2).map(|k| light(k as f32 - 0.5, false)));
        let plan = frame(&mut cache, &lights, &[]);
        assert_eq!(cache.stats().lights_in_view, 2);
        assert_eq!(cache.stats().shadowed, 2, "{:?}", cache.stats());
        assert!(cache.stats().evicted >= 1, "{:?}", cache.stats());
        assert_eq!(plan.first_view[0], NO_SHADOW);
        assert_ne!(plan.first_view[2], NO_SHADOW);
    }

    #[test]
    fn lights_in_view_are_never_evicted_for_others() {
        // Room for two spot lights' tiles: a third new one gets no room rather than
        // taking a tile from a light in view.
        let settings = LocalShadowSettings {
            atlas_size: 512,
            min_resolution: 256,
            max_resolution: 256,
            ..LocalShadowSettings::default()
        };
        let mut cache = ShadowCache::new(settings);
        let mut lights = vec![light(-1.0, true), light(1.0, true)];
        frame(&mut cache, &lights, &[]);
        assert_eq!(cache.stats().shadowed, 2);
        lights.push(light(0.0, true));
        lights.push(light(0.5, true));
        lights.push(light(-0.5, true));
        let plan = frame(&mut cache, &lights, &[]);
        assert_eq!(cache.stats().evicted, 0, "{:?}", cache.stats());
        assert_ne!(plan.first_view[0], NO_SHADOW);
        assert_ne!(plan.first_view[1], NO_SHADOW);
        assert!(cache.stats().without_room >= 1, "{:?}", cache.stats());
    }

    #[test]
    fn resolution_follows_screen_size_with_hysteresis() {
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let tile_at = |cache: &mut ShadowCache, z: f32| {
            let mut l = light(0.0, true);
            l.position.z = z;
            frame(cache, &[l], &[]);
            let mut plan = Plan::default();
            cache.plan(&[l], camera(), SIZE, 1.0, &mut plan);
            plan.views[0].tile[2] as u32
        };
        let near = tile_at(&mut cache, 2.0);
        let far = tile_at(&mut cache, -60.0);
        assert!(near > far, "{near} {far}");
        assert!(near <= 1024 && far >= 64);
        // A small step back keeps the size.
        let mut cache = ShadowCache::new(LocalShadowSettings::default());
        let a = tile_at(&mut cache, -10.0);
        let b = tile_at(&mut cache, -10.8);
        assert_eq!(a, b);
    }

    /// `local_shadow`'s projection of a point light's faces, as in `forward.slang`.
    fn shader_point_ndc(d: Vec3, terms: [f32; 4]) -> (usize, Vec3) {
        let a = d.abs();
        let x_major = a.x >= a.y && a.x >= a.z;
        let y_major = !x_major && a.y >= a.z;
        let along = if x_major {
            d.x
        } else if y_major {
            d.y
        } else {
            d.z
        };
        let flip = if along < 0.0 { -1.0 } else { 1.0 };
        let u = if x_major {
            flip * d.z
        } else if y_major {
            flip * d.x
        } else {
            -flip * d.x
        };
        let v = if y_major { -d.z } else { -d.y };
        let face = if x_major {
            0
        } else if y_major {
            2
        } else {
            4
        } + usize::from(along < 0.0);
        let inverse = 1.0 / along.abs();
        (
            face,
            Vec3::new(
                u * terms[0] * inverse,
                v * terms[0] * inverse,
                terms[1] + terms[2] * inverse,
            ),
        )
    }

    #[test]
    fn point_faces_project_without_matrices_as_with_them() {
        let key = Key::of(&light(0.0, false));
        let settings = LocalShadowSettings::default();
        let mut views = Vec::new();
        key.views(256, &[[0, 0]; 6], 1024, &settings, 1.0, &mut views);
        let terms = key.point_terms(256, &settings);
        let mut seed = 77u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0
        };
        for _ in 0..1000 {
            let d = Vec3::new(next(), next(), next()) * 3.5;
            if d.length() < 0.2 {
                continue;
            }
            let (face, ndc) = shader_point_ndc(d, terms);
            let m = Mat4::from_cols_array(&views[face].clip_from_world);
            let c = m * (key.position + d).extend(1.0);
            let expected = c.truncate() / c.w;
            assert!(
                (expected - ndc).abs().max_element() < 1e-4,
                "face {face}: {expected} vs {ndc}"
            );
        }
    }

    #[test]
    fn cube_faces_cover_their_major_axis_with_a_border() {
        let key = Key::of(&light(0.0, false));
        let mut views = Vec::new();
        key.views(
            128,
            &[[0, 0]; 6],
            1024,
            &LocalShadowSettings::default(),
            1.0,
            &mut views,
        );
        assert_eq!(views.len(), 6);
        for (k, face) in CUBE_FACES.iter().enumerate() {
            let m = Mat4::from_cols_array(&views[k].clip_from_world);
            // Directions whose major axis is this face's land inside, BORDER texels in.
            for (u, v) in [(0.99, 0.99), (-0.99, 0.3), (0.0, -0.99), (0.5, 0.5)] {
                let (a, b) = if face.x != 0.0 {
                    (Vec3::Y, Vec3::Z)
                } else if face.y != 0.0 {
                    (Vec3::X, Vec3::Z)
                } else {
                    (Vec3::X, Vec3::Y)
                };
                let p = key.position + (*face + a * u + b * v) * 2.0;
                let c = m * p.extend(1.0);
                let limit = 1.0 - 2.0 * BORDER / 128.0 + 1e-3;
                assert!(c.w > 0.0);
                assert!((c.x / c.w).abs() <= limit && (c.y / c.w).abs() <= limit);
                assert!((0.0..=1.0).contains(&(c.z / c.w)));
            }
        }
    }
}
