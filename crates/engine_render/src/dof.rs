//! Depth of field (ADR 0095, docs/research/depth-of-field.md): a thin lens's blur of what
//! lies before and behind the plane in focus, as compute passes of the frame graph over
//! the image the tone mapper reads (after antialiasing and upscaling) and the scene's
//! depth (at render size).
//!
//! The passes are AMD FidelityFX DOF 1.1's, ported to `shaders/dof.slang` (MIT,
//! THIRD_PARTY_NOTICES.md):
//!
//! 1. the nearest and farthest depth of every 8 × 8 tile, and the widest reach of any
//!    tile;
//! 2. the tiles dilated by what reaches them;
//! 3. four mips of color and depth at half size and below, bilateral;
//! 4. the near and far fields gathered at half size in rings (outside-in, the far field's
//!    rings occluding each other), per tile type;
//! 5. the fields filtered, upscaled and blended over the image.
//!
//! Where FidelityFX keeps signed circles of confusion, the passes keep the inverse depth.
//! A pixel's near and far radii are two separate non-negative values computed from it,
//! each zero outside its own field; no value stands for both (the research note's patent
//! check). [`Lens`] is the CPU description of the radii.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::graph::{Access, BufferDesc, FrameGraph, ShaderStages, Texture, TextureDesc};
use engine_rhi::ash::vk;
use engine_rhi::{Bindless, Device, RhiError, SamplerKind};

const TILES: &[u8] = include_bytes!("../shaders/dof.dofTiles.spv");
const DILATE: &[u8] = include_bytes!("../shaders/dof.dofDilate.spv");
const DOWNSAMPLE: &[u8] = include_bytes!("../shaders/dof.dofDownsample.spv");
const BLUR: &[u8] = include_bytes!("../shaders/dof.dofBlur.spv");
const COMPOSITE: &[u8] = include_bytes!("../shaders/dof.dofComposite.spv");

/// Pixels of the image per tile side (`TILE` in `dof.slang`).
const TILE: u32 = 8;
/// Mips of the gather's source, the first at half size.
const MIPS: usize = 4;
/// The mips' and the fields' format: color and inverse depth (or opacity, or weight).
const FIELD_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// The tiles' format: nearest and farthest inverse depth.
const TILE_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;

/// Depth of field settings: a thin lens focused at `focus_distance`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DofSettings {
    /// Distance to the plane in focus, in world units (meters), from the camera.
    pub focus_distance: f32,
    /// The lens's f-number: its focal length over its aperture's diameter. Halving it
    /// doubles every blur.
    pub f_number: f32,
    /// The sensor's height, in meters (0.024: full frame). With the view's field of view
    /// it gives the focal length: a larger sensor at the same field of view blurs more.
    pub sensor_height: f32,
    /// The largest blur radius, as a fraction of the image's height (0.04: 43 pixels at
    /// 1080p).
    pub max_radius: f32,
    /// Most rings the gather samples (quality; samples per pixel ≈ 6.25 n (n + 1) / 2).
    /// Larger blurs read coarser mips instead of more rings.
    pub rings: u32,
}

impl Default for DofSettings {
    fn default() -> Self {
        Self {
            focus_distance: 5.0,
            f_number: 2.8,
            sensor_height: 0.024,
            max_radius: 0.04,
            rings: 4,
        }
    }
}

/// The view's projection, as the thin lens needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DofCamera {
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance (finite).
    pub far: f32,
}

/// The thin lens of [`DofSettings`] for a camera and an image height: the radii of the
/// circles of confusion, in pixels of the half-size image the gather runs on, as functions
/// of the inverse depth `q = near / distance` (1 at the near plane).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lens {
    /// `q = 1 - depth * depth_scale` for the depth buffer's values (0 near, 1 far).
    pub depth_scale: f32,
    /// The near radius is `slope * q - coc`, the far radius `coc - slope * q`.
    pub slope: f32,
    /// The far radius at an infinite distance.
    pub coc: f32,
    /// The largest radius.
    pub limit: f32,
}

impl Lens {
    /// The lens for `settings` seen through `camera` into an image `height` pixels high.
    pub fn new(settings: &DofSettings, camera: DofCamera, height: u32) -> Self {
        let sensor = settings.sensor_height.max(1e-4);
        let focal = 0.5 * sensor / (0.5 * camera.fov_y.clamp(1e-3, 3.1)).tan();
        let aperture = focal / (2.0 * settings.f_number.max(0.1));
        // The plane in focus lies beyond the focal length.
        let focus = settings.focus_distance.max(focal * 1.01);
        // A distance D's circle on the sensor has the radius
        // aperture · focal / (focus − focal) · |focus / D − 1| (meters), here in pixels of
        // the half-size image.
        let half_pixels = 0.5 * height as f32 / sensor;
        let coc = aperture * focal / (focus - focal) * half_pixels;
        let near = camera.near.max(1e-6);
        Self {
            depth_scale: 1.0 - near / camera.far.max(near * 1.0001),
            slope: coc * focus / near,
            coc,
            limit: (settings.max_radius.max(0.0) * 0.5 * height as f32).max(0.0),
        }
    }

    /// The inverse depth of a depth buffer value.
    pub fn q(&self, depth: f32) -> f32 {
        1.0 - depth * self.depth_scale
    }

    /// The near field's radius at `q`: positive in front of the plane in focus, else 0.
    pub fn near_radius(&self, q: f32) -> f32 {
        (self.slope * q - self.coc).clamp(0.0, self.limit)
    }

    /// The far field's radius at `q`: positive behind the plane in focus, else 0.
    pub fn far_radius(&self, q: f32) -> f32 {
        (self.coc - self.slope * q).clamp(0.0, self.limit)
    }
}

/// `Push` in `dof.slang` (104 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    max_tile_radius: u64,
    color: u32,
    depth: u32,
    tiles: u32,
    dilated: u32,
    // A literal length: `[u32; MIPS]` here makes rustc 1.94 crash (broken MIR) in the
    // closures that copy `Push`.
    mips: [u32; 4],
    near_field: u32,
    far_field: u32,
    output: u32,
    linear_sampler: u32,
    point_sampler: u32,
    max_rings: u32,
    size: [u32; 2],
    half_size: [u32; 2],
    depth_size: [u32; 2],
    depth_scale: f32,
    coc_slope: f32,
    coc: f32,
    limit: f32,
}

const _: () = assert!(core::mem::size_of::<Push>() == 104);

/// Depth of field (see the module documentation): the pipelines, shared by every view.
#[derive(Debug)]
pub struct DepthOfField {
    tiles: ComputeShader<Push>,
    dilate: ComputeShader<Push>,
    downsample: ComputeShader<Push>,
    blur: ComputeShader<Push>,
    composite: ComputeShader<Push>,
}

impl DepthOfField {
    /// Creates the pipelines.
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        let [tiles, dilate, downsample, blur, composite] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(TILES, c"dofTiles", &[])?,
                ComputeSource::new(DILATE, c"dofDilate", &[])?,
                ComputeSource::new(DOWNSAMPLE, c"dofDownsample", &[])?,
                ComputeSource::new(BLUR, c"dofBlur", &[])?,
                ComputeSource::new(COMPOSITE, c"dofComposite", &[])?,
            ],
        )?;
        Ok(Self {
            tiles,
            dilate,
            downsample,
            blur,
            composite,
        })
    }

    /// Declares the depth of field of `color` (any size; HDR) with the scene's `depth`
    /// (its values 0 near, 1 far; any size) seen through `camera`, and returns the result,
    /// a new image of `color`'s size and format. The frame must execute with `bindless`.
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        color: Texture,
        depth: Texture,
        camera: DofCamera,
        settings: &DofSettings,
    ) -> Texture {
        engine_profiler::zone!("render::DepthOfField::add_passes");
        let desc = fg.texture_desc(color);
        let depth_desc = fg.texture_desc(depth);
        let (w, h) = (desc.width, desc.height);
        let lens = Lens::new(settings, camera, h);
        let half = [w.div_ceil(2), h.div_ceil(2)];
        let tile_count = [w.div_ceil(TILE), h.div_ceil(TILE)];
        let tiles = fg.create_texture(
            "dof tiles",
            TextureDesc::new_2d(TILE_FORMAT, tile_count[0], tile_count[1]),
        );
        let dilated = fg.create_texture(
            "dof dilated tiles",
            TextureDesc::new_2d(TILE_FORMAT, tile_count[0], tile_count[1]),
        );
        const MIP_NAMES: [&str; MIPS] = ["dof mip 0", "dof mip 1", "dof mip 2", "dof mip 3"];
        let mips: [Texture; MIPS] = core::array::from_fn(|k| {
            let d = 2u32 << k;
            fg.create_texture(
                MIP_NAMES[k],
                TextureDesc::new_2d(FIELD_FORMAT, w.div_ceil(d), h.div_ceil(d)),
            )
        });
        let near = fg.create_texture(
            "dof near",
            TextureDesc::new_2d(FIELD_FORMAT, half[0], half[1]),
        );
        let far = fg.create_texture(
            "dof far",
            TextureDesc::new_2d(FIELD_FORMAT, half[0], half[1]),
        );
        let output = fg.create_texture("dof", TextureDesc::new_2d(desc.format, w, h));
        let reach = fg.create_buffer("dof tile reach", BufferDesc { size: 4 });
        fg.add_pass("dof reach clear")
            .write(reach, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope.
                unsafe {
                    ctx.device
                        .cmd_fill_buffer(ctx.cmd, ctx.buffer(reach), 0, vk::WHOLE_SIZE, 0);
                }
            });
        let base = Push {
            linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
            point_sampler: Bindless::sampler(SamplerKind::NearestClamp),
            max_rings: settings.rings.clamp(1, 16),
            size: [w, h],
            half_size: half,
            depth_size: [depth_desc.width, depth_desc.height],
            depth_scale: lens.depth_scale,
            coc_slope: lens.slope,
            coc: lens.coc,
            limit: lens.limit,
            ..Push::default()
        };
        let stage = ShaderStages::COMPUTE;
        fg.add_compute("dof tiles", bindless)
            .read(depth, Access::Sampled(stage))
            .read(reach, Access::StorageReadWrite(stage))
            .write(tiles, Access::StorageWrite(stage))
            .dispatch(
                &self.tiles,
                Dispatch::Threads([tile_count[0], tile_count[1], 1]),
                move |ctx| {
                    Some(Push {
                        max_tile_radius: ctx.address(reach),
                        depth: ctx.sampled_index(depth)?,
                        tiles: ctx.storage_index(tiles)?,
                        ..base
                    })
                },
            );
        fg.add_compute("dof dilate", bindless)
            .read(tiles, Access::Sampled(stage))
            .read(reach, Access::StorageRead(stage))
            .write(dilated, Access::StorageWrite(stage))
            .dispatch(
                &self.dilate,
                Dispatch::Threads([tile_count[0], tile_count[1], 1]),
                move |ctx| {
                    Some(Push {
                        max_tile_radius: ctx.address(reach),
                        tiles: ctx.sampled_index(tiles)?,
                        dilated: ctx.storage_index(dilated)?,
                        ..base
                    })
                },
            );
        let mut downsample = fg
            .add_compute("dof downsample", bindless)
            .read(color, Access::Sampled(stage))
            .read(depth, Access::Sampled(stage));
        for mip in mips {
            downsample = downsample.write(mip, Access::StorageWrite(stage));
        }
        downsample.dispatch(
            &self.downsample,
            Dispatch::Groups([w.div_ceil(32), h.div_ceil(32), 1]),
            move |ctx| {
                let mut storage = [0; MIPS];
                for (index, mip) in storage.iter_mut().zip(mips) {
                    *index = ctx.storage_index(mip)?;
                }
                Some(Push {
                    color: ctx.sampled_index(color)?,
                    depth: ctx.sampled_index(depth)?,
                    mips: storage,
                    ..base
                })
            },
        );
        let mut blur = fg
            .add_compute("dof blur", bindless)
            .read(dilated, Access::Sampled(stage));
        for mip in mips {
            blur = blur.read(mip, Access::Sampled(stage));
        }
        blur.write(near, Access::StorageWrite(stage))
            .write(far, Access::StorageWrite(stage))
            .dispatch(
                &self.blur,
                Dispatch::Threads([half[0], half[1], 1]),
                move |ctx| {
                    let mut sampled = [0; MIPS];
                    for (index, mip) in sampled.iter_mut().zip(mips) {
                        *index = ctx.sampled_index(mip)?;
                    }
                    Some(Push {
                        dilated: ctx.sampled_index(dilated)?,
                        mips: sampled,
                        near_field: ctx.storage_index(near)?,
                        far_field: ctx.storage_index(far)?,
                        ..base
                    })
                },
            );
        fg.add_compute("dof composite", bindless)
            .read(color, Access::Sampled(stage))
            .read(depth, Access::Sampled(stage))
            .read(dilated, Access::Sampled(stage))
            .read(near, Access::Sampled(stage))
            .read(far, Access::Sampled(stage))
            .write(output, Access::StorageWrite(stage))
            .dispatch(
                &self.composite,
                Dispatch::Groups([w.div_ceil(16), h.div_ceil(16), 1]),
                move |ctx| {
                    Some(Push {
                        color: ctx.sampled_index(color)?,
                        depth: ctx.sampled_index(depth)?,
                        dilated: ctx.sampled_index(dilated)?,
                        near_field: ctx.sampled_index(near)?,
                        far_field: ctx.sampled_index(far)?,
                        output: ctx.storage_index(output)?,
                        ..base
                    })
                },
            );
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> DofCamera {
        DofCamera {
            fov_y: 0.5,
            near: 0.1,
            far: 1000.0,
        }
    }

    /// The thin lens's radius of a point at `distance`, in pixels of the half-size image,
    /// from the textbook formula (diameter (f² / N) · |D − S| / (D (S − f))).
    fn thin_lens(settings: &DofSettings, camera: DofCamera, height: u32, distance: f32) -> f32 {
        let f = 0.5 * settings.sensor_height / (0.5 * camera.fov_y).tan();
        let s = settings.focus_distance;
        let diameter = f * f / settings.f_number * (distance - s).abs() / (distance * (s - f));
        0.5 * diameter * (height as f32 / settings.sensor_height) * 0.5
    }

    /// The depth buffer value of a distance (Vulkan's 0..1 perspective depth).
    fn depth_of(camera: DofCamera, distance: f32) -> f32 {
        camera.far / (camera.far - camera.near) * (1.0 - camera.near / distance)
    }

    #[test]
    fn radii_follow_the_thin_lens_on_either_side_of_focus() {
        let settings = DofSettings {
            focus_distance: 4.0,
            f_number: 1.8,
            max_radius: 1.0,
            ..DofSettings::default()
        };
        let lens = Lens::new(&settings, camera(), 1080);
        for distance in [0.3, 1.0, 2.0, 3.9, 4.0, 4.1, 8.0, 40.0, 900.0] {
            let q = lens.q(depth_of(camera(), distance));
            let expected = thin_lens(&settings, camera(), 1080, distance);
            let (near, far) = (lens.near_radius(q), lens.far_radius(q));
            // One field at a time, never both.
            assert!(near == 0.0 || far == 0.0, "{distance}: {near} {far}");
            if distance < 4.0 {
                assert_eq!(far, 0.0, "{distance}");
            } else if distance > 4.0 {
                assert_eq!(near, 0.0, "{distance}");
            }
            let got = near + far;
            assert!(
                (got - expected).abs() <= 1e-3 * expected.max(1.0),
                "{distance} m: {got} px, the thin lens {expected} px"
            );
        }
    }

    #[test]
    fn radii_are_capped_and_vanish_at_the_focus() {
        let settings = DofSettings {
            focus_distance: 2.0,
            f_number: 0.8,
            max_radius: 0.02,
            ..DofSettings::default()
        };
        let lens = Lens::new(&settings, camera(), 1000);
        // 2 % of 1000 pixels, in half-size pixels.
        assert!((lens.limit - 10.0).abs() < 1e-4, "{}", lens.limit);
        let at = |distance: f32| {
            let q = lens.q(depth_of(camera(), distance));
            (lens.near_radius(q), lens.far_radius(q))
        };
        let (near, far) = at(2.0);
        assert!(near < 1e-2 && far < 1e-2, "{near} {far}");
        assert_eq!(at(0.15).0, lens.limit);
        assert_eq!(at(900.0).1, lens.limit);
        // A wider aperture (smaller f-number) blurs more.
        let wide = Lens::new(
            &DofSettings {
                f_number: 0.4,
                ..settings
            },
            camera(),
            1000,
        );
        assert!((wide.coc - 2.0 * lens.coc).abs() < 1e-4 * lens.coc);
    }
}
