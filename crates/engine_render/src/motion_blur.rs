//! Motion blur (ADR 0096, docs/research/motion-blur.md): what moves while the shutter is
//! open streaks along its motion, as compute passes of the frame graph over the image the
//! tone mapper reads (after antialiasing, upscaling and depth of field), with the scene's
//! motion vectors and depth (at render size).
//!
//! Guertin, McGuire and Nowrouzezahrai's filter (HPG 2014), written from the paper:
//!
//! 1. each pixel's velocity and depth into one texel, and the largest velocity of every
//!    tile (the tile is the largest blur);
//! 2. the largest of each tile's neighbourhood, a diagonal tile only if it blurs towards
//!    the tile;
//! 3. each pixel's samples along that velocity and its own, alternately, weighted by who
//!    is in front, by how far each velocity reaches and by direction.
//!
//! The team limits its use (ADR 0067): only cinematic views have it
//! (`engine_frame::ViewKind::Cinematic`); a gameplay view cannot ask for it.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::graph::{Access, FrameGraph, ShaderStages, Texture, TextureDesc};
use engine_rhi::ash::vk;
use engine_rhi::{Bindless, Device, PipelineStatistic, RhiError, SamplerKind};

const TILE_MAX: &[u8] = include_bytes!("../shaders/motion_blur.mbTileMax.spv");
const NEIGHBOR_MAX: &[u8] = include_bytes!("../shaders/motion_blur.mbNeighborMax.spv");
const GATHER: &[u8] = include_bytes!("../shaders/motion_blur.mbGather.spv");

/// The tiles' format: a velocity in pixels.
const TILE_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;
/// Each pixel's velocity (pixels), linear depth and speed, for the gather's samples.
const VZ_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// The paper's τ (the samples' largest jitter, in sample spacings) and ψ (its baseline
/// level): the jitter is τ ψ / N.
const TAU: f32 = 0.95;
const PSI: f32 = 27.0;

/// Motion blur settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionBlurSettings {
    /// The share of the frame the shutter is open: 0.5 is film's 180° shutter, 1 a streak
    /// as long as the frame's motion.
    pub shutter: f32,
    /// Samples per pixel (19; 35 the paper's own setting, about twice the cost).
    pub samples: u32,
    /// The longest blur each way along the motion, as a fraction of the image's height
    /// (0.037: 40 pixels at 1080p, the paper's). It is also the tile size.
    pub max_radius: f32,
}

impl Default for MotionBlurSettings {
    fn default() -> Self {
        Self {
            shutter: 0.5,
            samples: 19,
            max_radius: 0.037,
        }
    }
}

/// The view's projection, for linear depth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionBlurCamera {
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance (finite).
    pub far: f32,
}

/// `Push` in `motion_blur.slang` (80 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    color: u32,
    motion: u32,
    depth: u32,
    tiles: u32,
    neighbors: u32,
    output: u32,
    linear_sampler: u32,
    samples: u32,
    size: [u32; 2],
    render_size: [u32; 2],
    tile_count: [u32; 2],
    tile: u32,
    exposure: f32,
    near: f32,
    depth_scale: f32,
    jitter: f32,
    vz: u32,
}

const _: () = assert!(core::mem::size_of::<Push>() == 80);

/// Motion blur (see the module documentation): the pipelines, shared by every view.
#[derive(Debug)]
pub struct MotionBlur {
    tile_max: ComputeShader<Push>,
    neighbor_max: ComputeShader<Push>,
    gather: ComputeShader<Push>,
}

impl MotionBlur {
    /// Creates the pipelines.
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        let [tile_max, neighbor_max, gather] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(TILE_MAX, c"mbTileMax", &[])?,
                ComputeSource::new(NEIGHBOR_MAX, c"mbNeighborMax", &[])?,
                ComputeSource::new(GATHER, c"mbGather", &[])?,
            ],
        )?;
        Ok(Self {
            tile_max,
            neighbor_max,
            gather,
        })
    }

    /// The driver's statistics of each pass's pipeline (registers, occupancy), by pass
    /// name; empty unless the device captures them (`ENGINE_PIPELINE_STATS=1`).
    #[doc(hidden)]
    pub fn statistics(&self, device: &Device) -> [(&'static str, Vec<PipelineStatistic>); 3] {
        [
            ("tile max", self.tile_max.pipeline().statistics(device)),
            (
                "neighbor max",
                self.neighbor_max.pipeline().statistics(device),
            ),
            ("gather", self.gather.pipeline().statistics(device)),
        ]
    }

    /// Declares the motion blur of `color` (any size; HDR) by the scene's `motion` vectors
    /// (texture coordinates now minus last frame's, as the forward renderer writes them)
    /// and `depth` (0 near, 1 far; both any size), and returns the result, a new image of
    /// `color`'s size and format. The frame must execute with `bindless`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        color: Texture,
        motion: Texture,
        depth: Texture,
        camera: MotionBlurCamera,
        settings: &MotionBlurSettings,
    ) -> Texture {
        engine_profiler::zone!("render::MotionBlur::add_passes");
        let desc = fg.texture_desc(color);
        let render = fg.texture_desc(motion);
        let (w, h) = (desc.width, desc.height);
        let tile = ((settings.max_radius.max(0.0) * h as f32).round() as u32).max(8);
        let tile_count = [w.div_ceil(tile), h.div_ceil(tile)];
        let tiles = fg.create_texture(
            "motion blur tiles",
            TextureDesc::new_2d(TILE_FORMAT, tile_count[0], tile_count[1]),
        );
        let neighbors = fg.create_texture(
            "motion blur neighbors",
            TextureDesc::new_2d(TILE_FORMAT, tile_count[0], tile_count[1]),
        );
        let output = fg.create_texture("motion blur", TextureDesc::new_2d(desc.format, w, h));
        let vz = fg.create_texture(
            "motion blur velocity",
            TextureDesc::new_2d(VZ_FORMAT, w, h),
        );
        let samples = settings.samples.clamp(1, 64);
        let near = camera.near.max(1e-6);
        let base = Push {
            linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
            samples,
            size: [w, h],
            render_size: [render.width, render.height],
            tile_count,
            tile,
            exposure: 0.5 * settings.shutter.max(0.0),
            near,
            depth_scale: 1.0 - near / camera.far.max(near * 1.0001),
            jitter: TAU * PSI / samples as f32,
            ..Push::default()
        };
        let stage = ShaderStages::COMPUTE;
        fg.add_compute("motion blur tiles", bindless)
            .read(motion, Access::Sampled(stage))
            .read(depth, Access::Sampled(stage))
            .write(tiles, Access::StorageWrite(stage))
            .write(vz, Access::StorageWrite(stage))
            .dispatch(
                &self.tile_max,
                Dispatch::Groups([tile_count[0], tile_count[1], 1]),
                move |ctx| {
                    Some(Push {
                        motion: ctx.sampled_index(motion)?,
                        depth: ctx.sampled_index(depth)?,
                        tiles: ctx.storage_index(tiles)?,
                        vz: ctx.storage_index(vz)?,
                        ..base
                    })
                },
            );
        fg.add_compute("motion blur neighbors", bindless)
            .read(tiles, Access::Sampled(stage))
            .write(neighbors, Access::StorageWrite(stage))
            .dispatch(
                &self.neighbor_max,
                Dispatch::Threads([tile_count[0], tile_count[1], 1]),
                move |ctx| {
                    Some(Push {
                        tiles: ctx.sampled_index(tiles)?,
                        neighbors: ctx.storage_index(neighbors)?,
                        ..base
                    })
                },
            );
        fg.add_compute("motion blur gather", bindless)
            .read(color, Access::Sampled(stage))
            .read(vz, Access::Sampled(stage))
            .read(neighbors, Access::Sampled(stage))
            .write(output, Access::StorageWrite(stage))
            .dispatch(&self.gather, Dispatch::Threads([w, h, 1]), move |ctx| {
                Some(Push {
                    color: ctx.sampled_index(color)?,
                    vz: ctx.sampled_index(vz)?,
                    neighbors: ctx.sampled_index(neighbors)?,
                    output: ctx.storage_index(output)?,
                    ..base
                })
            });
        output
    }
}
