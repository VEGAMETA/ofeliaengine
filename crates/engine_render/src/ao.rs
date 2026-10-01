//! Ground-truth ambient occlusion, GTAO (ADR 0058): how much of the sky each pixel sees,
//! from the depth buffer, for the ambient and environment light of the forward pass.
//! See `ao.slang` for the passes. The forward renderer runs them between its depth
//! prepass and its shading pass when [`crate::forward::ForwardRenderer::set_ambient_occlusion`]
//! turns them on.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::forward::ViewSpace;
use crate::graph::{Access, FrameGraph, ShaderStages, Texture, TextureDesc};
use engine_rhi::ash::vk;
use engine_rhi::{Bindless, Device, RhiError, SamplerKind};

const DEPTHS: &[u8] = include_bytes!("../shaders/ao.aoDepths.spv");
const MAIN: &[u8] = include_bytes!("../shaders/ao.aoMain.spv");
const DENOISE: &[u8] = include_bytes!("../shaders/ao.aoDenoise.spv");

/// Format of the visibility the forward pass reads: 1 open, 0 fully occluded.
pub const AO_FORMAT: vk::Format = vk::Format::R8_UNORM;
const DEPTH_FORMAT: vk::Format = vk::Format::R32_SFLOAT;
/// Four edge weights of two bits each (`pack_edges` in `ao.slang`).
const EDGE_FORMAT: vk::Format = vk::Format::R8_UNORM;
/// Depth levels in the atlas (the full resolution and four coarser).
const LEVELS: u32 = 5;
/// Samples `2^(k + MIP_OFFSET)` pixels away and further read depth level `k` (XeGTAO's
/// `DepthMIPSamplingOffset`): coarse enough to stay in the cache, fine enough for the
/// horizons.
const MIP_OFFSET: f32 = 3.3;
/// Before the blur the visibility is stored divided by this (`ao.slang`).
const OCCLUSION_TERM_SCALE: f32 = 1.5;
/// The paper's (and XeGTAO's) radius multiplier: the effect radius covers occluders a
/// little beyond the requested one, where the falloff ends.
const RADIUS_MULTIPLIER: f32 = 1.457;

/// `AoPush` in `ao.slang` (108 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    view_mul: [f32; 2],
    view_add: [f32; 2],
    pixel_size: [f32; 2],
    size: [u32; 2],
    depth_a: f32,
    depth_b: f32,
    radius: f32,
    falloff_mul: f32,
    falloff_add: f32,
    power: f32,
    slices: u32,
    steps: u32,
    frame: u32,
    source: u32,
    target: u32,
    edges: u32,
    depths: u32,
    mip_scale: f32,
    level2_y: u32,
    level3_y: u32,
    level4_y: u32,
    sampler: u32,
    output_scale: f32,
}

/// How many directions and samples each pixel takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AoQuality {
    /// 1 slice, 2 samples per side.
    Low,
    /// 2 slices, 2 samples per side.
    Medium,
    /// 3 slices, 3 samples per side.
    High,
    /// 9 slices, 3 samples per side.
    Ultra,
    /// Any number of slices (1-16) and samples per side (1-32): many samples find the
    /// horizons exactly, a reference for the presets (3 samples per side miss part of the
    /// occlusion, see ADR 0058).
    Custom {
        /// Directions around the view vector.
        slices: u32,
        /// Samples on each side of a direction.
        steps: u32,
    },
}

impl AoQuality {
    fn slices_steps(self) -> (u32, u32) {
        match self {
            Self::Low => (1, 2),
            Self::Medium => (2, 2),
            Self::High => (3, 3),
            Self::Ultra => (9, 3),
            Self::Custom { slices, steps } => (slices.clamp(1, 16), steps.clamp(1, 32)),
        }
    }
}

/// Ambient occlusion's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AoSettings {
    /// Directions and samples per pixel.
    pub quality: AoQuality,
    /// How far occluders count, in world units (0.5: half a metre in a metre-scaled
    /// scene).
    pub radius: f32,
    /// The share of the radius over which occluders fade out (0.615).
    pub falloff: f32,
    /// Visibility is raised to this power: higher is darker (2.2).
    pub power: f32,
    /// Edge-aware blur passes over the result (0-3; 1 by default).
    pub denoise_passes: u32,
}

impl Default for AoSettings {
    fn default() -> Self {
        Self {
            quality: AoQuality::High,
            radius: 0.5,
            falloff: 0.615,
            power: 2.2,
            denoise_passes: 1,
        }
    }
}

/// The ambient occlusion passes.
pub struct AmbientOcclusion {
    depths: ComputeShader<Push>,
    main: ComputeShader<Push>,
    denoise: ComputeShader<Push>,
}

impl core::fmt::Debug for AmbientOcclusion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AmbientOcclusion").finish_non_exhaustive()
    }
}

/// Size of depth level `k` of a `width × height` target.
fn level_size(width: u32, height: u32, k: u32) -> (u32, u32) {
    (((width - 1) >> k) + 1, ((height - 1) >> k) + 1)
}

impl AmbientOcclusion {
    /// Creates the pipelines.
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        let [depths, main, denoise] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(DEPTHS, c"aoDepths", &[])?,
                ComputeSource::new(MAIN, c"aoMain", &[])?,
                ComputeSource::new(DENOISE, c"aoDenoise", &[])?,
            ],
        )?;
        Ok(Self {
            depths,
            main,
            denoise,
        })
    }

    /// Adds the passes computing the visibility of `depth` (sampled) seen by `view`, and
    /// returns it ([`AO_FORMAT`], the depth's size). `frame` rotates the noise; with a
    /// temporal resolve (TAA) the frames' noise averages out.
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        depth: Texture,
        view: &ViewSpace,
        settings: &AoSettings,
        frame: u32,
    ) -> Texture {
        let desc = fg.texture_desc(depth);
        let (width, height) = (desc.width.max(1), desc.height.max(1));
        let (level1_width, _) = level_size(width, height, 1);
        let stacked: u32 = (1..LEVELS).map(|k| level_size(width, height, k).1).sum();
        let atlas = fg.create_texture(
            "ao depths",
            TextureDesc::new_2d(DEPTH_FORMAT, width + level1_width, height.max(stacked)),
        );
        let (slices, steps) = settings.quality.slices_steps();
        let passes = settings.denoise_passes.min(3);
        let radius = settings.radius.max(1e-4) * RADIUS_MULTIPLIER;
        let falloff_range = settings.falloff.clamp(0.01, 1.0) * radius;
        let falloff_from = radius - falloff_range;
        let base = Push {
            view_mul: [2.0 / view.p00, 2.0 / view.p11],
            view_add: [(view.p20 - 1.0) / view.p00, (view.p21 - 1.0) / view.p11],
            pixel_size: [1.0 / width as f32, 1.0 / height as f32],
            size: [width, height],
            depth_a: view.p22,
            depth_b: view.p32,
            radius,
            falloff_mul: -1.0 / falloff_range,
            falloff_add: falloff_from / falloff_range + 1.0,
            power: settings.power,
            slices,
            steps,
            frame,
            mip_scale: (-MIP_OFFSET).exp2(),
            level2_y: level_size(width, height, 1).1,
            level3_y: (1..3).map(|k| level_size(width, height, k).1).sum(),
            level4_y: (1..4).map(|k| level_size(width, height, k).1).sum(),
            sampler: Bindless::sampler(SamplerKind::NearestClamp),
            ..Push::default()
        };
        fg.add_compute("ao depths", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .write(atlas, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.depths,
                Dispatch::Groups([width.div_ceil(16), height.div_ceil(16), 1]),
                move |ctx| {
                    Some(Push {
                        source: ctx.sampled_index(depth)?,
                        target: ctx.storage_index(atlas)?,
                        ..base
                    })
                },
            );
        let visibility = fg.create_texture("ao", TextureDesc::new_2d(AO_FORMAT, width, height));
        let edges = fg.create_texture("ao edges", TextureDesc::new_2d(EDGE_FORMAT, width, height));
        fg.add_compute("ao", bindless)
            .read(atlas, Access::Sampled(ShaderStages::COMPUTE))
            .write(visibility, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(edges, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &self.main,
                Dispatch::Threads([width, height, 1]),
                move |ctx| {
                    Some(Push {
                        depths: ctx.sampled_index(atlas)?,
                        target: ctx.storage_index(visibility)?,
                        edges: ctx.storage_index(edges)?,
                        output_scale: if passes > 0 {
                            1.0 / OCCLUSION_TERM_SCALE
                        } else {
                            1.0
                        },
                        ..base
                    })
                },
            );
        let mut result = visibility;
        for pass in 0..passes {
            let output_scale = if pass + 1 == passes {
                OCCLUSION_TERM_SCALE
            } else {
                1.0
            };
            let blurred =
                fg.create_texture("ao denoised", TextureDesc::new_2d(AO_FORMAT, width, height));
            let source = result;
            fg.add_compute("ao denoise", bindless)
                .read(source, Access::Sampled(ShaderStages::COMPUTE))
                .read(edges, Access::Sampled(ShaderStages::COMPUTE))
                .write(blurred, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(
                    &self.denoise,
                    Dispatch::Threads([width, height, 1]),
                    move |ctx| {
                        Some(Push {
                            source: ctx.sampled_index(source)?,
                            edges: ctx.sampled_index(edges)?,
                            target: ctx.storage_index(blurred)?,
                            output_scale,
                            ..base
                        })
                    },
                );
            result = blurred;
        }
        result
    }
}
