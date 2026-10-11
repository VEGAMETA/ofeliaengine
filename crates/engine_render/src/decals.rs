//! Decals through a decal buffer (ADR 0082, docs/research/decals.md): posters, blood,
//! scorch marks, dirt and paint projected onto whatever lies in their boxes.
//!
//! After the depth prepass a compute pass evaluates each pixel's decals into three
//! render-size targets ([`DecalTargets`]) that the forward shading's decal variants blend
//! in before lighting: albedo, a normal, roughness and metalness, each premultiplied by its
//! coverage. Decals are found as the local lights are (ADR 0045): culled by their bounding
//! spheres, z-bins of index ranges and a bitmask per 16-pixel tile; they are composited in
//! [`Decal::order`] (then list order), not by depth.
//!
//! The surface's position and normal come from the depth, so a decal crossing a corner
//! decals each face by that face's own angle to it (faded by [`Decal::angle_fade`]).
//! Transparent surfaces get no decals: they are not in the depth.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::cull::Frustum;
use crate::graph::{Access, BufferDesc, FrameGraph, ShaderStages, Texture, TextureDesc};
use crate::lights::{LIGHT_TILE, ViewAxis, Z_BINS, tile_rect};
use crate::upload::UploadRing;
use core::cell::RefCell;
use core::mem::MaybeUninit;
use engine_core::math::{Mat4, Vec4};
use engine_rhi::ash::vk;
use engine_rhi::{Bindless, Device, RhiError, SamplerKind};

const TILES: &[u8] = include_bytes!("../shaders/decals.decalTiles.spv");
const APPLY: &[u8] = include_bytes!("../shaders/decals.decalApply.spv");

/// Format of the decal albedo target: rgb premultiplied by coverage, a the coverage.
pub const DECAL_ALBEDO_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// Format of the decal normal target: a world-space normal premultiplied by coverage, w
/// the coverage.
pub const DECAL_NORMAL_FORMAT: vk::Format = vk::Format::R8G8B8A8_SNORM;
/// Format of the decal surface target: roughness and metalness premultiplied by coverage,
/// b the coverage.
pub const DECAL_SURFACE_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// Most decals a view draws (indices are 16-bit in the z-bins); the rest are dropped.
pub const MAX_DECALS: usize = 65_535;
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
    /// `None` to keep the surface's normal.
    pub normal: Option<u32>,
    /// Multiplies the albedo (linear rgb); a is the opacity.
    pub tint: [f32; 4],
    /// The decal's roughness, replacing the surface's by [`Self::surface`] × coverage.
    pub roughness: f32,
    /// The decal's metalness, as roughness.
    pub metalness: f32,
    /// How much of the surface's roughness and metalness the decal replaces (0 keeps them).
    pub surface: f32,
    /// How much of the surface's normal the normal map replaces.
    pub normal_strength: f32,
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
            normal_strength: 1.0,
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
    normal_strength: f32,
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
            normal_strength: d.normal_strength,
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
    near_center: [f32; 4],
    forward: [f32; 4],
    size: [u32; 2],
    tiles_x: u32,
    words: u32,
    sampler: u32,
    count: u32,
    _pad: [u32; 2],
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
    normal: u32,
    surface: u32,
}

/// A decal kept by [`cull`].
#[derive(Debug, Clone, Copy)]
struct Culled {
    order: i32,
    index: u32,
    /// Along the view axis, from the near plane: the bounding sphere's center.
    depth: f32,
    radius: f32,
    /// Tiles: min x, min y, max x, max y (inclusive).
    rect: [u16; 4],
}

/// What the decal passes put in the ring for a view.
#[derive(Debug, Clone, Copy)]
struct Uploaded {
    frame: u64,
    decals: u64,
    bins: u64,
    rects: u64,
    count: u32,
    words: u32,
    tiles: [u32; 2],
}

/// The decal buffer of a view (see the module documentation), its size the depth's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecalTargets {
    /// [`DECAL_ALBEDO_FORMAT`].
    pub albedo: Texture,
    /// [`DECAL_NORMAL_FORMAT`].
    pub normal: Texture,
    /// [`DECAL_SURFACE_FORMAT`].
    pub surface: Texture,
}

/// The decal passes (see the module documentation).
pub struct DecalPasses {
    tiles: ComputeShader<Push>,
    apply: ComputeShader<Push>,
    culled: RefCell<Vec<Culled>>,
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
        })
    }

    /// Declares the decal buffer of `decals` over `depth` (sampled; drawn with
    /// `clip_from_world`), or returns `None` when no decal reaches the view. The frame must
    /// execute with `bindless`. Allocates nothing once its lists have grown to the view's
    /// decal count.
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        depth: Texture,
        clip_from_world: Mat4,
        decals: &[Decal],
    ) -> Option<DecalTargets> {
        engine_profiler::zone!("render::DecalPasses::add_passes");
        let desc = fg.texture_desc(depth);
        let size = [desc.width.max(1), desc.height.max(1)];
        let uploaded = self.upload(ring, decals, clip_from_world, size)?;
        let albedo = fg.create_texture(
            "decal albedo",
            TextureDesc::new_2d(DECAL_ALBEDO_FORMAT, size[0], size[1]),
        );
        let normal = fg.create_texture(
            "decal normal",
            TextureDesc::new_2d(DECAL_NORMAL_FORMAT, size[0], size[1]),
        );
        let surface = fg.create_texture(
            "decal surface",
            TextureDesc::new_2d(DECAL_SURFACE_FORMAT, size[0], size[1]),
        );
        let words =
            u64::from(uploaded.tiles[0]) * u64::from(uploaded.tiles[1]) * u64::from(uploaded.words);
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
            frame: uploaded.frame,
            decals: uploaded.decals,
            bins: uploaded.bins,
            rects: uploaded.rects,
            ..Push::default()
        };
        // One workgroup per decal: it sets the decal's bit over its rectangle.
        fg.add_compute("decal tiles", bindless)
            .read(tiles, Access::StorageReadWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.tiles,
                Dispatch::Groups([uploaded.count, 1, 1]),
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
            .write(normal, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(surface, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.apply,
                Dispatch::Threads([size[0], size[1], 1]),
                move |ctx| {
                    Some(Push {
                        tiles: ctx.address(tiles),
                        depth: ctx.sampled_index(depth)?,
                        albedo: ctx.storage_index(albedo)?,
                        normal: ctx.storage_index(normal)?,
                        surface: ctx.storage_index(surface)?,
                        ..base
                    })
                },
            );
        Some(DecalTargets {
            albedo,
            normal,
            surface,
        })
    }

    /// Culls and bins `decals` and puts them in `ring`.
    fn upload(
        &self,
        ring: &mut UploadRing,
        decals: &[Decal],
        clip_from_world: Mat4,
        size: [u32; 2],
    ) -> Option<Uploaded> {
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
        let bin_scale = Z_BINS as f32 / far.max(1e-6);
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
        let (out, staged_bins) = ring.alloc::<u32>(Z_BINS)?;
        write_bins(&culled, bin_scale, out);
        let words = (n as u32).div_ceil(32);
        let frame = ring.push(&[DecalFrame {
            world_from_clip: clip_from_world.inverse().to_cols_array(),
            near_center: axis.near_center.extend(bin_scale).to_array(),
            forward: axis.forward.extend(0.0).to_array(),
            size,
            tiles_x: tiles[0],
            words,
            sampler: Bindless::sampler(SamplerKind::LinearClamp),
            count: n as u32,
            _pad: [0; 2],
        }])?;
        Some(Uploaded {
            frame,
            decals: staged_decals.address,
            bins: staged_bins.address,
            rects: staged_rects.address,
            count: n as u32,
            words,
            tiles,
        })
    }
}

/// Keeps the decals whose bounding spheres reach the view, in their drawing order (order,
/// then index); returns the farthest depth they reach along the view axis.
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
        let depth = axis.depth(center);
        far = far.max(depth + radius);
        culled.push(Culled {
            order: d.order,
            index: index as u32,
            depth,
            radius,
            rect: tile_rect(clip_from_world, center, radius, size, tiles),
        });
    }
    culled.sort_unstable_by(|a, b| a.order.cmp(&b.order).then(a.index.cmp(&b.index)));
    far
}

/// The [`Z_BINS`] z-bins: lowest | highest index << 16 of the kept decals overlapping each
/// (empty bins have lowest > highest). Indices follow the drawing order, so a bin's range
/// may hold decals that are elsewhere: the tile bits and the box test sort them out.
fn write_bins(culled: &[Culled], bin_scale: f32, out: &mut [MaybeUninit<u32>]) {
    let mut bins = [(u16::MAX, 0u16); Z_BINS];
    let bin = |depth: f32| ((depth * bin_scale).max(0.0) as usize).min(Z_BINS - 1);
    for (i, c) in culled.iter().enumerate() {
        let i = i as u16;
        for b in &mut bins[bin(c.depth - c.radius)..=bin(c.depth + c.radius)] {
            b.0 = b.0.min(i);
            b.1 = b.1.max(i);
        }
    }
    for (out, (low, high)) in out.iter_mut().zip(bins) {
        out.write(u32::from(low) | u32::from(high) << 16);
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
    fn bins_cover_each_decals_depth_range() {
        let decals = [
            decal_at(Vec3::new(0.0, 0.0, 0.0), 0),
            decal_at(Vec3::new(0.0, 0.0, -10.0), 0),
        ];
        let clip = camera();
        let axis = ViewAxis::new(clip).unwrap();
        let mut culled = Vec::new();
        let far = cull(&decals, clip, axis, [1920, 1080], [120, 68], &mut culled);
        let scale = Z_BINS as f32 / far;
        let mut out = vec![MaybeUninit::uninit(); Z_BINS];
        write_bins(&culled, scale, &mut out);
        // SAFETY: `write_bins` writes every element.
        let bins: Vec<u32> = out.iter().map(|b| unsafe { b.assume_init() }).collect();
        let at = |depth: f32| bins[((depth * scale) as usize).min(Z_BINS - 1)];
        let (near, far_one) = (culled[0].depth, culled[1].depth);
        assert_eq!(at(near) & 0xFFFF, 0);
        assert_eq!(at(far_one) >> 16, 1);
        // Between the two decals' spheres: empty.
        let gap = at((near + far_one) * 0.5);
        assert!((gap & 0xFFFF) > (gap >> 16), "{gap:#x}");
    }
}
