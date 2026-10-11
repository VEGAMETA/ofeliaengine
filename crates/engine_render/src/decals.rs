//! Decals through a decal buffer (ADR 0082, docs/research/decals.md): posters, blood,
//! scorch marks, dirt and paint projected onto whatever lies in their boxes.
//!
//! After the depth prepass a compute pass evaluates each pixel's decals into two
//! render-size targets ([`DecalTargets`]) that the forward shading's decal variants blend
//! in before lighting: albedo, and the normal's change with roughness and metalness, each
//! premultiplied by its coverage. Decals are found much as the local lights are (ADR
//! 0045): culled by the frustum, then a bitmask per 16-pixel tile of the decals whose
//! boxes' screen rectangles reach it and a bitmask per z-bin of those whose boxes' depth
//! ranges reach it (bitmasks rather than lights' index ranges: decals are composited in
//! [`Decal::order`], then list order, not by depth). A wave walks the union of its pixels'
//! decals, so a decal's data is read once per wave. Only the tiles some decal reaches are
//! evaluated, written and read.
//!
//! The surface's position and normal come from the depth, so a decal crossing a corner
//! decals each face by that face's own angle to it (faded by [`Decal::angle_fade`]), its
//! normal map laid in the face's plane. Transparent surfaces get no decals: they are not
//! in the depth.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::cull::Frustum;
use crate::graph::{Access, BufferDesc, FrameGraph, ShaderStages, Texture, TextureDesc};
use crate::lights::{LIGHT_TILE, ViewAxis};
use crate::upload::UploadRing;
use core::cell::RefCell;
use engine_core::math::{Mat4, Vec2, Vec3, Vec4};
use engine_rhi::ash::vk;
use engine_rhi::{Bindless, Device, RhiError, SamplerKind};

const TILES: &[u8] = include_bytes!("../shaders/decals.decalTiles.spv");
const APPLY: &[u8] = include_bytes!("../shaders/decals.decalApply.spv");

/// Format of the decal albedo target: the square root of rgb premultiplied by coverage
/// (dark colors keep their steps), a the coverage.
pub const DECAL_ALBEDO_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// Format of the decal surface target: x the change of the normal (world space,
/// premultiplied by coverage) as three signed 10-bit components (bits 0, 10, 20; ±511
/// for ±1); y roughness, metalness (premultiplied by coverage) and the coverage, 10 bits
/// each (1023 for 1).
pub const DECAL_SURFACE_FORMAT: vk::Format = vk::Format::R32G32_UINT;
/// Most decals a view draws; the rest are dropped.
pub const MAX_DECALS: usize = 65_535;
/// Z-bins over the depth the view's decals reach (`DECAL_BINS` in `decals.slang`).
const DECAL_BINS: usize = 256;
const NO_TEXTURE: u32 = u32::MAX;

/// A decal: the box `[-0.5, 0.5]³` of its own space, projected along its −z axis onto
/// the surfaces inside the box that face its +z.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decal {
    /// The box's place, rotation and size in the world.
    pub world_from_decal: Mat4,
    /// A bindless sampled texture (sRGB color, alpha the coverage), or `None` for the tint
    /// alone. Its u runs along the decal's +x, its v along −y.
    pub albedo: Option<u32>,
    /// A bindless tangent-space normal map (xy; x along the decal's +x, y along +y), or
    /// `None` to keep the surface's normal. Its bumps are added to the surface's own (a
    /// normal-mapped wall keeps its bricks under a decal), laid in the plane of whatever
    /// face the decal falls on.
    pub normal: Option<u32>,
    /// Multiplies the albedo (linear rgb); a is the opacity.
    pub tint: [f32; 4],
    /// The decal's roughness, replacing the surface's by [`Self::surface`] × coverage.
    pub roughness: f32,
    /// The decal's metalness, as roughness.
    pub metalness: f32,
    /// How much of the surface's roughness and metalness the decal replaces (0 keeps them).
    pub surface: f32,
    /// Scales the normal map's xy, as glTF's `normalTexture.scale` (0 ignores the map).
    pub normal_scale: f32,
    /// Cosines of the angle between the surface's normal and the decal's +z: the decal
    /// is whole above the first and gone below the second.
    pub angle_fade: [f32; 2],
    /// Drawn over decals of a lower order; equal orders in list order.
    pub order: i32,
}

impl Default for Decal {
    fn default() -> Self {
        Self {
            world_from_decal: Mat4::IDENTITY,
            albedo: None,
            normal: None,
            tint: [1.0; 4],
            roughness: 0.5,
            metalness: 0.0,
            surface: 0.0,
            normal_scale: 1.0,
            angle_fade: [0.25, 0.05],
            order: 0,
        }
    }
}

/// `Decal` in `decals.slang` (144 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct DecalData {
    decal_from_world: [[f32; 4]; 3],
    tint: [f32; 4],
    axis_x: [f32; 4],
    axis_y: [f32; 4],
    axis_z: [f32; 4],
    albedo: u32,
    normal: u32,
    roughness: f32,
    metalness: f32,
    surface: f32,
    normal_scale: f32,
    fade_full: f32,
    fade_none: f32,
}

const _: () = assert!(size_of::<DecalData>() == 144);

impl DecalData {
    fn new(d: &Decal) -> Self {
        let m = d.world_from_decal.inverse();
        let row = |i: usize| m.row(i).to_array();
        let axis = |v: Vec4| v.truncate().normalize_or_zero().extend(0.0).to_array();
        Self {
            decal_from_world: [row(0), row(1), row(2)],
            tint: d.tint,
            axis_x: axis(d.world_from_decal.x_axis),
            axis_y: axis(d.world_from_decal.y_axis),
            axis_z: axis(d.world_from_decal.z_axis),
            albedo: d.albedo.unwrap_or(NO_TEXTURE),
            normal: d.normal.unwrap_or(NO_TEXTURE),
            roughness: d.roughness,
            metalness: d.metalness,
            surface: d.surface,
            normal_scale: d.normal_scale,
            fade_full: d.angle_fade[0],
            fade_none: d.angle_fade[1],
        }
    }
}

/// `DecalFrame` in `decals.slang` (128 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct DecalFrame {
    world_from_clip: [f32; 16],
    bin_row: [f32; 4],
    ndc_per_pixel: [f32; 2],
    /// The occupied tiles, x | y << 16: one group each.
    tile_list: u64,
    size: [u32; 2],
    tiles_x: u32,
    words: u32,
    sampler: u32,
    count: u32,
    /// Per tile a bit: some decal's rectangle reaches it.
    occupied: u64,
}

const _: () = assert!(size_of::<DecalFrame>() == 128);

/// `Push` in `decals.slang` (56 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    frame: u64,
    decals: u64,
    bins: u64,
    tiles: u64,
    rects: u64,
    depth: u32,
    albedo: u32,
    surface: u32,
    _pad: u32,
}

/// A decal kept by [`cull`].
#[derive(Debug, Clone, Copy)]
struct Culled {
    order: i32,
    index: u32,
    /// Along the view axis, from the near plane: the nearest and farthest of the box.
    near: f32,
    far: f32,
    /// Tiles: min x, min y, max x, max y (inclusive).
    rect: [u16; 4],
}

/// A view's decals, culled, binned and put in the ring by [`DecalPasses::prepare`].
#[derive(Debug, Clone, Copy)]
pub struct PreparedDecals {
    frame: u64,
    decals: u64,
    bins: u64,
    rects: u64,
    occupied: u64,
    count: u32,
    words: u32,
    tiles: [u32; 2],
    size: [u32; 2],
    /// Occupied tiles.
    tile_count: u32,
}

impl PreparedDecals {
    /// The tiles some decal reaches, in the ring: per 16-pixel tile ([`LIGHT_TILE`],
    /// rows of `ceil(width / 16)` tiles) a bit. Only those tiles' pixels of
    /// [`DecalTargets`] are written.
    pub fn occupied(&self) -> u64 {
        self.occupied
    }
}

/// The decal buffer of a view (see the module documentation), its size the depth's. Only
/// the pixels of the tiles [`PreparedDecals::occupied`] marks are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecalTargets {
    /// [`DECAL_ALBEDO_FORMAT`].
    pub albedo: Texture,
    /// [`DECAL_SURFACE_FORMAT`].
    pub surface: Texture,
}

/// The decal passes (see the module documentation).
pub struct DecalPasses {
    tiles: ComputeShader<Push>,
    apply: ComputeShader<Push>,
    culled: RefCell<Vec<Culled>>,
    occupied: RefCell<Vec<u32>>,
    tile_list: RefCell<Vec<u32>>,
    bins: RefCell<Vec<u32>>,
}

impl core::fmt::Debug for DecalPasses {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DecalPasses").finish_non_exhaustive()
    }
}

impl DecalPasses {
    /// Creates the pipelines.
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        let [tiles, apply] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(TILES, c"decalTiles", &[])?,
                ComputeSource::new(APPLY, c"decalApply", &[])?,
            ],
        )?;
        Ok(Self {
            tiles,
            apply,
            culled: RefCell::new(Vec::new()),
            occupied: RefCell::new(Vec::new()),
            tile_list: RefCell::new(Vec::new()),
            bins: RefCell::new(Vec::new()),
        })
    }

    /// Declares the decal buffer of `decals` over `depth` (sampled; drawn with
    /// `clip_from_world`), or returns `None` when no decal reaches the view: [`Self::prepare`]
    /// then [`Self::add_prepared`]. The frame must execute with `bindless`.
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        depth: Texture,
        clip_from_world: Mat4,
        decals: &[Decal],
    ) -> Option<DecalTargets> {
        let desc = fg.texture_desc(depth);
        let size = [desc.width, desc.height];
        let prepared = self.prepare(ring, decals, clip_from_world, size)?;
        Some(self.add_prepared(fg, bindless, depth, &prepared))
    }

    /// Culls and bins `decals` for a view of `size` pixels drawn with `clip_from_world`
    /// and puts them in `ring`, or returns `None` when none reaches the view (or the ring
    /// is full). Allocates nothing once its lists have grown to the view's decal count.
    pub fn prepare(
        &self,
        ring: &mut UploadRing,
        decals: &[Decal],
        clip_from_world: Mat4,
        size: [u32; 2],
    ) -> Option<PreparedDecals> {
        engine_profiler::zone!("render::DecalPasses::prepare");
        let size = [size[0].max(1), size[1].max(1)];
        let axis = ViewAxis::new(clip_from_world)?;
        let tiles = [
            size[0].div_ceil(LIGHT_TILE).max(1),
            size[1].div_ceil(LIGHT_TILE).max(1),
        ];
        let mut culled = self.culled.borrow_mut();
        let far = cull(decals, clip_from_world, axis, size, tiles, &mut culled);
        if culled.is_empty() {
            return None;
        }
        let n = culled.len();
        let words = (n as u32).div_ceil(32);
        let bin_scale = DECAL_BINS as f32 / far.max(1e-6);
        let (out, staged_decals) = ring.alloc::<DecalData>(n)?;
        for (out, c) in out.iter_mut().zip(culled.iter()) {
            out.write(DecalData::new(&decals[c.index as usize]));
        }
        let (out, staged_rects) = ring.alloc::<[u32; 2]>(n)?;
        for (out, c) in out.iter_mut().zip(culled.iter()) {
            out.write([
                u32::from(c.rect[0]) | u32::from(c.rect[1]) << 16,
                u32::from(c.rect[2]) | u32::from(c.rect[3]) << 16,
            ]);
        }
        let mut bins = self.bins.borrow_mut();
        write_bins(&culled, bin_scale, words as usize, &mut bins);
        let staged_bins = ring.push(&bins)?;
        let mut occupied = self.occupied.borrow_mut();
        mark_tiles(&culled, tiles, &mut occupied);
        let mut tile_list = self.tile_list.borrow_mut();
        list_tiles(&occupied, tiles[0], &mut tile_list);
        let occupied = ring.push(&occupied)?;
        let tile_count = tile_list.len() as u32;
        let staged_tiles = ring.push(&tile_list)?;
        // A clip point v's z-bin: its depth along the view axis, dot(p - near_center,
        // forward), × bin_scale, where p = (M v).xyz / (M v).w: a ratio of two rows.
        let world_from_clip = clip_from_world.inverse();
        let forward = axis.forward;
        let bin_row = world_from_clip.transpose()
            * forward.extend(-forward.dot(axis.near_center))
            * bin_scale;
        let frame = ring.push(&[DecalFrame {
            world_from_clip: world_from_clip.to_cols_array(),
            bin_row: bin_row.to_array(),
            ndc_per_pixel: [2.0 / size[0] as f32, 2.0 / size[1] as f32],
            tile_list: staged_tiles,
            size,
            tiles_x: tiles[0],
            words,
            sampler: Bindless::sampler(SamplerKind::LinearClamp),
            count: n as u32,
            occupied,
        }])?;
        Some(PreparedDecals {
            frame,
            decals: staged_decals.address,
            bins: staged_bins,
            rects: staged_rects.address,
            occupied,
            count: n as u32,
            words,
            tiles,
            size,
            tile_count,
        })
    }

    /// Declares the decal buffer of `prepared` over `depth` (sampled, the size `prepared`
    /// was made for): the tiles' bitmasks, then the decals' evaluation.
    pub fn add_prepared<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        depth: Texture,
        prepared: &PreparedDecals,
    ) -> DecalTargets {
        engine_profiler::zone!("render::DecalPasses::add_prepared");
        let size = prepared.size;
        let albedo = fg.create_texture(
            "decal albedo",
            TextureDesc::new_2d(DECAL_ALBEDO_FORMAT, size[0], size[1]),
        );
        let surface = fg.create_texture(
            "decal surface",
            TextureDesc::new_2d(DECAL_SURFACE_FORMAT, size[0], size[1]),
        );
        let words =
            u64::from(prepared.tiles[0]) * u64::from(prepared.tiles[1]) * u64::from(prepared.words);
        let tiles = fg.create_buffer("decal tiles", BufferDesc { size: words * 4 });
        fg.add_pass("decal tiles clear")
            .write(tiles, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope.
                unsafe {
                    ctx.device
                        .cmd_fill_buffer(ctx.cmd, ctx.buffer(tiles), 0, vk::WHOLE_SIZE, 0);
                }
            });
        let base = Push {
            frame: prepared.frame,
            decals: prepared.decals,
            bins: prepared.bins,
            rects: prepared.rects,
            ..Push::default()
        };
        // One workgroup per decal: it sets the decal's bit over its rectangle.
        fg.add_compute("decal tiles", bindless)
            .read(tiles, Access::StorageReadWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.tiles,
                Dispatch::Groups([prepared.count, 1, 1]),
                move |ctx| {
                    Some(Push {
                        tiles: ctx.address(tiles),
                        ..base
                    })
                },
            );
        fg.add_compute("decals", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(tiles, Access::StorageRead(ShaderStages::COMPUTE))
            .write(albedo, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(surface, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.apply,
                Dispatch::Groups([prepared.tile_count, 1, 1]),
                move |ctx| {
                    Some(Push {
                        tiles: ctx.address(tiles),
                        depth: ctx.sampled_index(depth)?,
                        albedo: ctx.storage_index(albedo)?,
                        surface: ctx.storage_index(surface)?,
                        ..base
                    })
                },
            );
        DecalTargets { albedo, surface }
    }
}

/// Keeps the decals whose boxes reach the view, in their drawing order (order, then
/// index), with the tiles and depths their boxes cover; returns the farthest depth they
/// reach along the view axis.
fn cull(
    decals: &[Decal],
    clip_from_world: Mat4,
    axis: ViewAxis,
    size: [u32; 2],
    tiles: [u32; 2],
    culled: &mut Vec<Culled>,
) -> f32 {
    let frustum = Frustum::from_clip_from_world(clip_from_world);
    culled.clear();
    let mut far = 0.0f32;
    for (index, d) in decals.iter().enumerate() {
        if culled.len() == MAX_DECALS {
            break;
        }
        let m = d.world_from_decal;
        let center = m.w_axis.truncate();
        // Half the box's diagonal.
        let radius = 0.5
            * (m.x_axis.truncate().length_squared()
                + m.y_axis.truncate().length_squared()
                + m.z_axis.truncate().length_squared())
            .sqrt();
        let visible = center.is_finite()
            && radius.is_finite()
            && radius > 0.0
            && d.tint[3] > 0.0
            && !frustum
                .planes
                .iter()
                .any(|p| p.truncate().dot(center) + p.w < -radius);
        if !visible {
            continue;
        }
        let Some((rect, near, reach)) = box_extent(m, clip_from_world, axis, size, tiles) else {
            continue;
        };
        far = far.max(reach);
        culled.push(Culled {
            order: d.order,
            index: index as u32,
            near,
            far: reach,
            rect,
        });
    }
    culled.sort_unstable_by(|a, b| a.order.cmp(&b.order).then(a.index.cmp(&b.index)));
    far
}

/// The tiles a box (`world_from_box` of `[-0.5, 0.5]³`) covers on screen and its nearest
/// and farthest depths along the view axis: the corners in front of the near plane and,
/// for a box across it, where its edges cross it. `None` when it is off screen.
fn box_extent(
    world_from_box: Mat4,
    clip_from_world: Mat4,
    axis: ViewAxis,
    size: [u32; 2],
    tiles: [u32; 2],
) -> Option<([u16; 4], f32, f32)> {
    let clip_from_box = clip_from_world * world_from_box;
    let corner = |k: usize| {
        Vec3::new(
            if k & 1 == 0 { -0.5 } else { 0.5 },
            if k & 2 == 0 { -0.5 } else { 0.5 },
            if k & 4 == 0 { -0.5 } else { 0.5 },
        )
    };
    let clip: [Vec4; 8] = core::array::from_fn(|k| clip_from_box * corner(k).extend(1.0));
    let (mut lo, mut hi) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
    let mut add = |c: Vec4| {
        let ndc = c.truncate().truncate() / c.w;
        lo = lo.min(ndc);
        hi = hi.max(ndc);
    };
    // In front of the near plane: clip z ≥ 0 (Vulkan's depth range), where w > 0.
    for &c in &clip {
        if c.z >= 0.0 && c.w > 0.0 {
            add(c);
        }
    }
    // The edges (corners one bit apart) across the near plane, where they cross it.
    for a in 0..8 {
        for bit in [1, 2, 4] {
            let b = a | bit;
            if b == a {
                continue;
            }
            let (ca, cb) = (clip[a], clip[b]);
            if (ca.z >= 0.0) != (cb.z >= 0.0) {
                let crossing = ca + (cb - ca) * (ca.z / (ca.z - cb.z));
                if crossing.w > 0.0 {
                    add(crossing);
                }
            }
        }
    }
    if lo.x > hi.x || hi.x < -1.0 || lo.x > 1.0 || hi.y < -1.0 || lo.y > 1.0 {
        return None;
    }
    let (mut near, mut far) = (f32::MAX, f32::MIN);
    for k in 0..8 {
        let depth = axis.depth(world_from_box.transform_point3(corner(k)));
        near = near.min(depth);
        far = far.max(depth);
    }
    let tile = |ndc: f32, axis: usize| {
        let pixel = (ndc.clamp(-1.0, 1.0) * 0.5 + 0.5) * size[axis] as f32;
        ((pixel / LIGHT_TILE as f32).floor().max(0.0) as u32).min(tiles[axis] - 1) as u16
    };
    let rect = [tile(lo.x, 0), tile(lo.y, 1), tile(hi.x, 0), tile(hi.y, 1)];
    Some((rect, near.max(0.0), far))
}

/// Sets in `occupied` (cleared and sized to `tiles`) the bits of the tiles the kept decals'
/// rectangles cover.
fn mark_tiles(culled: &[Culled], tiles: [u32; 2], occupied: &mut Vec<u32>) {
    let count = tiles[0] as usize * tiles[1] as usize;
    occupied.clear();
    occupied.resize(count.div_ceil(32), 0);
    for c in culled {
        for y in usize::from(c.rect[1])..=usize::from(c.rect[3]) {
            let row = y * tiles[0] as usize;
            let (mut first, last) = (row + usize::from(c.rect[0]), row + usize::from(c.rect[2]));
            // Up to a word at a time.
            while first <= last {
                let bit = first & 31;
                let n = (32 - bit).min(last - first + 1);
                let mask = if n == 32 {
                    u32::MAX
                } else {
                    ((1u32 << n) - 1) << bit
                };
                occupied[first >> 5] |= mask;
                first += n;
            }
        }
    }
}

/// The occupied tiles of `occupied` (rows of `tiles_x`), x | y << 16, in `list` (cleared
/// here).
fn list_tiles(occupied: &[u32], tiles_x: u32, list: &mut Vec<u32>) {
    list.clear();
    for (w, &word) in occupied.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let t = w as u32 * 32 + bits.trailing_zeros();
            bits &= bits - 1;
            list.push((t % tiles_x) | ((t / tiles_x) << 16));
        }
    }
}

/// The [`DECAL_BINS`] z-bins (cleared and sized here), each `words` bitmask words of the
/// kept decals whose depth ranges overlap it.
fn write_bins(culled: &[Culled], bin_scale: f32, words: usize, bins: &mut Vec<u32>) {
    bins.clear();
    bins.resize(DECAL_BINS * words, 0);
    let bin = |depth: f32| ((depth * bin_scale).max(0.0) as usize).min(DECAL_BINS - 1);
    for (i, c) in culled.iter().enumerate() {
        let (word, bit) = (i >> 5, 1u32 << (i & 31));
        for b in bin(c.near)..=bin(c.far) {
            bins[b * words + word] |= bit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{Quat, Vec3, look_at, vulkan_perspective};

    fn camera() -> Mat4 {
        vulkan_perspective(1.0, 16.0 / 9.0, 0.1, 100.0)
            * look_at(Vec3::new(0.0, 2.0, 5.0), Vec3::ZERO, Vec3::Y)
    }

    fn decal_at(center: Vec3, order: i32) -> Decal {
        Decal {
            world_from_decal: Mat4::from_scale_rotation_translation(
                Vec3::splat(1.0),
                Quat::IDENTITY,
                center,
            ),
            order,
            ..Decal::default()
        }
    }

    #[test]
    fn culls_by_the_frustum_and_orders_by_order_then_index() {
        let decals = [
            decal_at(Vec3::new(0.0, 0.0, 0.0), 1),
            decal_at(Vec3::new(0.0, 0.0, 50.0), 0), // behind the camera
            decal_at(Vec3::new(1.0, 0.0, 0.0), 0),
            decal_at(Vec3::new(-1.0, 0.0, 0.0), 1),
        ];
        let clip = camera();
        let axis = ViewAxis::new(clip).unwrap();
        let mut culled = Vec::new();
        let far = cull(&decals, clip, axis, [1920, 1080], [120, 68], &mut culled);
        let order: Vec<u32> = culled.iter().map(|c| c.index).collect();
        assert_eq!(order, [2, 0, 3]);
        assert!(far > 4.0 && far < 7.0, "{far}");
    }

    #[test]
    fn a_flat_box_covers_fewer_tiles_than_its_sphere() {
        // A 2 x 2 x 0.1 floor decal under a camera looking down at 22 degrees.
        let flat = Mat4::from_scale_rotation_translation(
            Vec3::new(2.0, 2.0, 0.1),
            Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2),
            Vec3::ZERO,
        );
        let clip = camera();
        let axis = ViewAxis::new(clip).unwrap();
        let (size, tiles) = ([1920, 1080], [120, 68]);
        let (rect, near, far) = box_extent(flat, clip, axis, size, tiles).unwrap();
        let sphere = crate::lights::tile_rect(clip, Vec3::ZERO, 1.42, size, tiles);
        let area = |r: [u16; 4]| (u32::from(r[2] - r[0]) + 1) * (u32::from(r[3] - r[1]) + 1);
        assert!(area(rect) * 2 < area(sphere), "{rect:?} {sphere:?}");
        // Its depths: the near edge (z = 1) to the far one (z = -1), seen from z = 5.
        assert!(near > 3.0 && far < 7.0 && near < far, "{near} {far}");
        // Off screen and behind the camera.
        let behind = Mat4::from_translation(Vec3::new(0.0, 2.0, 9.0));
        assert!(box_extent(behind, clip, axis, size, tiles).is_none());
        // Around the eye: every tile.
        let around = Mat4::from_translation(Vec3::new(0.0, 2.0, 5.0));
        let (rect, near, _) = box_extent(around, clip, axis, size, tiles).unwrap();
        assert_eq!(rect, [0, 0, 119, 67]);
        assert_eq!(near, 0.0);
        // Across the near plane to the right: only the right of the screen.
        let right = Mat4::from_scale_rotation_translation(
            Vec3::splat(1.0),
            Quat::IDENTITY,
            Vec3::new(0.6, 2.0, 5.0),
        );
        let (rect, _, _) = box_extent(right, clip, axis, size, tiles).unwrap();
        assert!(rect[0] > 60 && rect[2] == 119, "{rect:?}");
    }

    #[test]
    fn bins_cover_each_decals_depth_range() {
        let decals = [
            decal_at(Vec3::new(0.0, 0.0, 0.0), 0),
            decal_at(Vec3::new(0.0, 0.0, -10.0), 0),
        ];
        let clip = camera();
        let axis = ViewAxis::new(clip).unwrap();
        let mut culled = Vec::new();
        let far = cull(&decals, clip, axis, [1920, 1080], [120, 68], &mut culled);
        let scale = DECAL_BINS as f32 / far;
        let mut bins = vec![u32::MAX; 3];
        write_bins(&culled, scale, 1, &mut bins);
        assert_eq!(bins.len(), DECAL_BINS);
        let at = |depth: f32| bins[((depth * scale) as usize).min(DECAL_BINS - 1)];
        let middle = |c: &Culled| (c.near + c.far) * 0.5;
        assert_eq!(at(middle(&culled[0])), 0b01);
        assert_eq!(at(middle(&culled[1])), 0b10);
        // Between the two boxes: empty.
        assert_eq!(at((culled[0].far + culled[1].near) * 0.5), 0);
    }

    #[test]
    fn marks_the_tiles_of_every_rectangle() {
        let culled = |rect| Culled {
            order: 0,
            index: 0,
            near: 1.0,
            far: 2.0,
            rect,
        };
        // Rows of 40 tiles: the first rectangle's rows cross the words at 64 and 96.
        let rects = [[20, 1, 35, 2], [0, 0, 0, 0], [39, 2, 39, 2]];
        let kept: Vec<Culled> = rects.iter().map(|&r| culled(r)).collect();
        let mut occupied = vec![u32::MAX; 9];
        mark_tiles(&kept, [40, 3], &mut occupied);
        assert_eq!(occupied.len(), 4);
        let mut list = Vec::new();
        list_tiles(&occupied, 40, &mut list);
        assert_eq!(list.len(), 16 * 2 + 1 + 1);
        assert_eq!(list[0], 0);
        assert!(list.contains(&(35 | 2 << 16)) && list.contains(&(39 | 2 << 16)));
        for y in 0..3u16 {
            for x in 0..40u16 {
                let t = usize::from(y) * 40 + usize::from(x);
                let inside = rects
                    .iter()
                    .any(|r| (r[0]..=r[2]).contains(&x) && (r[1]..=r[3]).contains(&y));
                assert_eq!(
                    occupied[t >> 5] >> (t & 31) & 1 == 1,
                    inside,
                    "tile {x}, {y}"
                );
            }
        }
    }
}
