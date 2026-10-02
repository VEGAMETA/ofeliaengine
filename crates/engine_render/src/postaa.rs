//! Antialiasing filters after tone mapping (ADR 0075): FXAA, MLAA and SMAA (1x, S2x,
//! T2x, 4x), on the tone mapper's perceptual image ([`PERCEPTUAL_FORMAT`],
//! `Tonemap::new_perceptual`), written into the view's output.
//!
//! - **FXAA** (`postaa.slang`): one pass.
//! - **SMAA** (Jimenez et al. 2012; the reference code `SMAA.hlsl` is MIT): edge
//!   detection (luma) into [`EDGES_FORMAT`], blending weights from the area and search
//!   tables into [`WEIGHTS_FORMAT`], neighbourhood blending into the output. Each preset
//!   ([`SmaaPreset`]) is its own pair of pipelines; **MLAA** is the preset with SMAA's
//!   additions off (`mlaa.slang`).
//! - **S2x**: the two samples of MSAA 2× are tone mapped apart; each goes through SMAA with
//!   its sample's area table offsets, the second blended at half weight over the first.
//! - **T2x**: two jitter positions; SMAA's blending keeps each pixel's speed in alpha, a
//!   resolve mixes this frame with the last along the motion vectors (the history is a
//!   copy of this frame's SMAA result). With S2x: SMAA 4x.
//!
//! Pipelines are made the first time a view asks for their filter ([`PostAa::ensure`]):
//! a game that never uses one compiles nothing for it.

use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc};
use crate::texture::GpuTexture;
use crate::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocator, Bindless, Blend, Device, GraphicsPipelineDesc, Pipeline, PipelineDesc, RhiError,
    SamplerKind, ShaderStageDesc, spirv_words,
};

/// The tone mapper's perceptual image the filters read: sRGB-encoded (linear outputs)
/// or PQ values, 10 bits per channel.
pub const PERCEPTUAL_FORMAT: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;
/// SMAA's edges.
pub const EDGES_FORMAT: vk::Format = vk::Format::R8G8_UNORM;
/// SMAA's blending weights.
pub const WEIGHTS_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

const POST_VERTEX: &[u8] = include_bytes!("../shaders/postaa.postVertex.spv");
const FXAA: &[u8] = include_bytes!("../shaders/postaa.fxaaFragment.spv");
const SPLIT: &[u8] = include_bytes!("../shaders/postaa.splitFragment.spv");
const BLEND_VERTEX: &[u8] = include_bytes!("../shaders/smaa.blendVertex.spv");
const BLEND_FRAGMENT: &[u8] = include_bytes!("../shaders/smaa.blendFragment.spv");
const TEMPORAL_VERTEX: &[u8] = include_bytes!("../shaders/smaa_temporal.blendVertex.spv");
const TEMPORAL_BLEND: &[u8] = include_bytes!("../shaders/smaa_temporal.blendFragment.spv");
const TEMPORAL_RESOLVE: &[u8] = include_bytes!("../shaders/smaa_temporal.resolveFragment.spv");
/// Each preset's edge and weight passes: vertex and fragment shaders.
const PRESETS: [[&[u8]; 4]; 5] = [
    [
        include_bytes!("../shaders/smaa_low.edgesVertex.spv"),
        include_bytes!("../shaders/smaa_low.edgesFragment.spv"),
        include_bytes!("../shaders/smaa_low.weightsVertex.spv"),
        include_bytes!("../shaders/smaa_low.weightsFragment.spv"),
    ],
    [
        include_bytes!("../shaders/smaa_medium.edgesVertex.spv"),
        include_bytes!("../shaders/smaa_medium.edgesFragment.spv"),
        include_bytes!("../shaders/smaa_medium.weightsVertex.spv"),
        include_bytes!("../shaders/smaa_medium.weightsFragment.spv"),
    ],
    [
        include_bytes!("../shaders/smaa_high.edgesVertex.spv"),
        include_bytes!("../shaders/smaa_high.edgesFragment.spv"),
        include_bytes!("../shaders/smaa_high.weightsVertex.spv"),
        include_bytes!("../shaders/smaa_high.weightsFragment.spv"),
    ],
    [
        include_bytes!("../shaders/smaa_ultra.edgesVertex.spv"),
        include_bytes!("../shaders/smaa_ultra.edgesFragment.spv"),
        include_bytes!("../shaders/smaa_ultra.weightsVertex.spv"),
        include_bytes!("../shaders/smaa_ultra.weightsFragment.spv"),
    ],
    [
        include_bytes!("../shaders/mlaa.edgesVertex.spv"),
        include_bytes!("../shaders/mlaa.edgesFragment.spv"),
        include_bytes!("../shaders/mlaa.weightsVertex.spv"),
        include_bytes!("../shaders/mlaa.weightsFragment.spv"),
    ],
];

/// SMAA's precomputed tables, from the reference code (`SMAA/Textures/AreaTex.h` and
/// `SearchTex.h`, MIT): the area of each edge pattern (RG8, 160 × 560) and the search's
/// last step (R8, 64 × 16).
const AREA_TABLE: &[u8] = include_bytes!("../data/smaa_area.bin");
const SEARCH_TABLE: &[u8] = include_bytes!("../data/smaa_search.bin");
const AREA_SIZE: (u32, u32) = (160, 560);
const SEARCH_SIZE: (u32, u32) = (64, 16);

/// The SMAA presets the reference defines, and MLAA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SmaaPreset {
    /// `SMAA_PRESET_LOW`.
    Low,
    /// `SMAA_PRESET_MEDIUM`.
    Medium,
    /// `SMAA_PRESET_HIGH`.
    High,
    /// `SMAA_PRESET_ULTRA`.
    Ultra,
    /// Jimenez et al.'s MLAA: no diagonals, corners or local contrast adaptation.
    Mlaa,
}

impl SmaaPreset {
    fn index(self) -> usize {
        self as usize
    }
}

/// A filter after tone mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PostFilter {
    /// FXAA.
    Fxaa,
    /// SMAA or MLAA.
    Smaa(SmaaPreset),
}

/// How the last pass writes the output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decode {
    /// As it is: perceptual (another filter's intermediate, or a PQ output).
    None,
    /// sRGB-decoded and scaled by the output's brightest value (`peak × white` of the
    /// tone mapper; 1 for SDR).
    Srgb {
        /// The brightest value.
        scale: f32,
    },
}

/// Where a filter writes.
#[derive(Debug, Clone, Copy)]
pub struct PostTarget {
    /// The output image.
    pub texture: Texture,
    /// The rectangle of it the view covers, or `None` for all of it.
    pub rect: Option<vk::Rect2D>,
    /// What happens to the output first.
    pub load: LoadOp,
    /// How values are written.
    pub decode: Decode,
}

/// SMAA T2x's temporal inputs (with them, SMAA's result is resolved with last frame's).
#[derive(Debug, Clone, Copy)]
pub struct SmaaTemporal {
    /// Motion vectors (`MotionOutput::velocity`), the size of the images.
    pub velocity: Texture,
    /// Last frame's SMAA result ([`PERCEPTUAL_FORMAT`], speed in alpha), or `None` after a
    /// cut (this frame is resolved with itself).
    pub previous: Option<Texture>,
    /// Where this frame's SMAA result is copied for the next frame (the same format and
    /// size).
    pub history: Texture,
    /// The jitter phase this frame was drawn with: 0 or 1.
    pub phase: usize,
}

/// What [`PostAa::add_smaa`] filters.
#[derive(Debug, Clone, Copy)]
pub struct SmaaInput {
    /// The perceptual images: one, or with S2x each sample's.
    pub images: [Option<Texture>; 2],
    /// Their size.
    pub size: (u32, u32),
    /// The preset.
    pub preset: SmaaPreset,
    /// T2x (or 4x with two images).
    pub temporal: Option<SmaaTemporal>,
}

/// `Push` in `postaa_common.hlsli` (80 bytes).
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    metrics: [f32; 4],
    subsample_indices: [f32; 4],
    color: u32,
    edges: u32,
    blend: u32,
    area: u32,
    search: u32,
    velocity: u32,
    previous: u32,
    linear_sampler: u32,
    point_sampler: u32,
    decode: u32,
    decode_scale: f32,
    second: u32,
}

const _: () = assert!(core::mem::size_of::<Push>() == 80);

/// SMAA's area table offsets (`subsampleIndices`, the reference's @SUBSAMPLE_INDICES):
/// by image count (1: 1x or T2x, 2: S2x or 4x), T2x phase and sample.
fn subsample_indices(samples: usize, temporal: Option<usize>, sample: usize) -> [f32; 4] {
    match (samples, temporal) {
        (0 | 1, None) => [0.0; 4],
        (0 | 1, Some(0)) | (_, None) if sample == 0 => [1.0, 1.0, 1.0, 0.0],
        (0 | 1, Some(_)) | (_, None) => [2.0, 2.0, 2.0, 0.0],
        (_, Some(0)) if sample == 0 => [5.0, 3.0, 1.0, 3.0],
        (_, Some(0)) => [4.0, 6.0, 2.0, 3.0],
        (_, Some(_)) if sample == 0 => [3.0, 5.0, 1.0, 4.0],
        (_, Some(_)) => [6.0, 4.0, 2.0, 4.0],
    }
}

/// The projection jitter (pixels, x right, y down: `Temporal`'s convention) of SMAA T2x's
/// two frames (`samples` = 1) and SMAA 4x's (`samples` = 2): each sample lands where the
/// reference's area table offsets expect it (the MSAA 2× standard positions, ±¼ pixel).
pub fn smaa_jitter(samples: usize) -> &'static [[f32; 2]] {
    // The reference's camera jitters (its y up) are sample positions; ours moves the
    // image, so the sign flips with x and stays with y.
    const T2X: [[f32; 2]; 2] = [[-0.25, -0.25], [0.25, 0.25]];
    const FOUR: [[f32; 2]; 2] = [[-0.125, 0.125], [0.125, -0.125]];
    if samples == 2 { &FOUR } else { &T2X }
}

/// The filters' pipelines for one output format and SMAA's tables, made as views ask for
/// them.
#[derive(Debug)]
pub struct PostAa {
    output_format: vk::Format,
    fxaa: Option<Pipeline>,
    /// Per preset: edges, weights.
    presets: [Option<[Pipeline; 2]>; 5],
    /// Neighbourhood blending into the output: opaque, half (S2x's second sample).
    blend: Option<[Pipeline; 2]>,
    /// T2x: blending that keeps speed (perceptual: opaque, half), the resolve.
    temporal: Option<([Pipeline; 2], Pipeline)>,
    /// S2x: the samples' HDR images from the resolved color and their difference.
    split: Option<Pipeline>,
    tables: Option<[GpuTexture; 2]>,
}

fn invalid() -> RhiError {
    RhiError::Vulkan {
        call: "embedded SPIR-V",
        result: vk::Result::ERROR_INVALID_SHADER_NV,
    }
}

fn words(bytes: &[u8]) -> Result<Vec<u32>, RhiError> {
    spirv_words(bytes).ok_or_else(invalid)
}

fn full_screen<'a>(
    vertex: &'a [u32],
    vertex_entry: &'a core::ffi::CStr,
    fragment: &'a [u32],
    fragment_entry: &'a core::ffi::CStr,
    formats: &'a [vk::Format],
    blend: Blend,
) -> PipelineDesc<'a> {
    PipelineDesc::Graphics(GraphicsPipelineDesc {
        color_formats: formats,
        cull_mode: vk::CullModeFlags::NONE,
        depth_test: false,
        depth_write: false,
        blend,
        ..GraphicsPipelineDesc::new(
            ShaderStageDesc {
                spirv: vertex,
                entry: vertex_entry,
            },
            Some(ShaderStageDesc {
                spirv: fragment,
                entry: fragment_entry,
            }),
        )
    })
}

impl PostAa {
    /// No pipelines yet, for writing `output_format`.
    pub fn new(output_format: vk::Format) -> Self {
        Self {
            output_format,
            fxaa: None,
            presets: Default::default(),
            blend: None,
            temporal: None,
            split: None,
            tables: None,
        }
    }

    /// Makes what `filter` needs, with T2x's passes (`temporal`) and S2x's split
    /// (`split`): pipelines through the pipeline cache, SMAA's tables (uploaded by the
    /// first frame that uses them).
    pub fn ensure(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        filter: PostFilter,
        temporal: bool,
        split: bool,
    ) -> Result<(), RhiError> {
        let layout = bindless.pipeline_layout();
        let output = [self.output_format];
        let perceptual = [PERCEPTUAL_FORMAT];
        let hdr = [crate::post::HDR_FORMAT, crate::post::HDR_FORMAT];
        let post_vertex = words(POST_VERTEX)?;
        if split && self.split.is_none() {
            let fragment = words(SPLIT)?;
            let desc = full_screen(
                &post_vertex,
                c"postVertex",
                &fragment,
                c"splitFragment",
                &hdr,
                Blend::Opaque,
            );
            self.split = Pipeline::create_many(device, layout, &[desc])?.pop();
        }
        match filter {
            PostFilter::Fxaa => {
                if self.fxaa.is_none() {
                    let fragment = words(FXAA)?;
                    let desc = full_screen(
                        &post_vertex,
                        c"postVertex",
                        &fragment,
                        c"fxaaFragment",
                        &output,
                        Blend::Opaque,
                    );
                    self.fxaa = Pipeline::create_many(device, layout, &[desc])?.pop();
                }
            }
            PostFilter::Smaa(preset) => {
                let k = preset.index();
                let [ev, ef, wv, wf] = PRESETS[k].map(words);
                let (ev, ef, wv, wf) = (ev?, ef?, wv?, wf?);
                let blend_vertex = words(BLEND_VERTEX)?;
                let blend_fragment = words(BLEND_FRAGMENT)?;
                let temporal_vertex = words(TEMPORAL_VERTEX)?;
                let temporal_blend = words(TEMPORAL_BLEND)?;
                let temporal_resolve = words(TEMPORAL_RESOLVE)?;
                let edges = [EDGES_FORMAT];
                let weights = [WEIGHTS_FORMAT];
                let mut descs = Vec::with_capacity(7);
                let make_preset = self.presets[k].is_none();
                let make_blend = self.blend.is_none();
                let make_temporal = temporal && self.temporal.is_none();
                if make_preset {
                    descs.push(full_screen(
                        &ev,
                        c"edgesVertex",
                        &ef,
                        c"edgesFragment",
                        &edges,
                        Blend::Opaque,
                    ));
                    descs.push(full_screen(
                        &wv,
                        c"weightsVertex",
                        &wf,
                        c"weightsFragment",
                        &weights,
                        Blend::Opaque,
                    ));
                }
                if make_blend {
                    for blend in [Blend::Opaque, Blend::Half] {
                        descs.push(full_screen(
                            &blend_vertex,
                            c"blendVertex",
                            &blend_fragment,
                            c"blendFragment",
                            &output,
                            blend,
                        ));
                    }
                }
                if make_temporal {
                    for blend in [Blend::Opaque, Blend::Half] {
                        descs.push(full_screen(
                            &temporal_vertex,
                            c"blendVertex",
                            &temporal_blend,
                            c"blendFragment",
                            &perceptual,
                            blend,
                        ));
                    }
                    descs.push(full_screen(
                        &temporal_vertex,
                        c"blendVertex",
                        &temporal_resolve,
                        c"resolveFragment",
                        &output,
                        Blend::Opaque,
                    ));
                }
                let mut pipelines = Pipeline::create_many(device, layout, &descs)?.into_iter();
                let mut next = || {
                    pipelines.next().ok_or(RhiError::Vulkan {
                        call: "SMAA pipelines (fewer than described)",
                        result: vk::Result::ERROR_UNKNOWN,
                    })
                };
                if make_preset {
                    self.presets[k] = Some([next()?, next()?]);
                }
                if make_blend {
                    self.blend = Some([next()?, next()?]);
                }
                if make_temporal {
                    self.temporal = Some(([next()?, next()?], next()?));
                }
                if self.tables.is_none() {
                    let area = GpuTexture::new(
                        device,
                        allocator,
                        bindless,
                        vk::Format::R8G8_UNORM,
                        AREA_SIZE.0,
                        AREA_SIZE.1,
                    )?;
                    let search = match GpuTexture::new(
                        device,
                        allocator,
                        bindless,
                        vk::Format::R8_UNORM,
                        SEARCH_SIZE.0,
                        SEARCH_SIZE.1,
                    ) {
                        Ok(search) => search,
                        Err(e) => {
                            let mut area = area;
                            // SAFETY: never used by the GPU.
                            unsafe { area.destroy(device, allocator, bindless, 0) };
                            return Err(e);
                        }
                    };
                    self.tables = Some([area, search]);
                }
            }
        }
        Ok(())
    }

    /// Frees SMAA's tables.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) {
        if let Some(tables) = self.tables.take() {
            for mut table in tables {
                // SAFETY: guaranteed by the caller.
                unsafe { table.destroy(device, allocator, bindless, 0) };
            }
        }
    }

    fn push(&self, size: (u32, u32)) -> Push {
        Push {
            metrics: [
                1.0 / size.0.max(1) as f32,
                1.0 / size.1.max(1) as f32,
                size.0 as f32,
                size.1 as f32,
            ],
            subsample_indices: [0.0; 4],
            color: 0,
            edges: 0,
            blend: 0,
            area: 0,
            search: 0,
            velocity: 0,
            previous: 0,
            linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
            point_sampler: Bindless::sampler(SamplerKind::NearestClamp),
            decode: 0,
            decode_scale: 1.0,
            second: 0,
        }
    }

    /// Declares FXAA from `image` (perceptual, `size`) into `target`. Nothing when FXAA
    /// was not made ([`Self::ensure`]).
    pub fn add_fxaa<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        image: Texture,
        size: (u32, u32),
        target: PostTarget,
    ) {
        let Some(pipeline) = &self.fxaa else {
            return;
        };
        let base = self.push(size);
        draw(
            fg,
            bindless,
            "fxaa",
            pipeline,
            &[(image, Access::Sampled(ShaderStages::FRAGMENT))],
            target,
            move |ctx| {
                Some(Push {
                    color: ctx.sampled_index(image)?,
                    ..decoded(base, target.decode)
                })
            },
        );
    }

    /// Declares S2x's split: from the scene's resolved HDR `color` (after the hook) and
    /// `difference` (half sample 0 minus sample 1, from the MSAA resolve), the two
    /// samples' HDR images, for the tone mapper to map one by one.
    pub fn add_split<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        color: Texture,
        difference: Texture,
        size: (u32, u32),
    ) -> Option<[Texture; 2]> {
        let pipeline = self.split.as_ref()?;
        let desc = TextureDesc::new_2d(crate::post::HDR_FORMAT, size.0, size.1);
        let images = [
            fg.create_texture("smaa sample 0", desc),
            fg.create_texture("smaa sample 1", desc),
        ];
        let base = self.push(size);
        fg.add_pass("smaa split")
            .read(color, Access::Sampled(ShaderStages::FRAGMENT))
            .read(difference, Access::Sampled(ShaderStages::FRAGMENT))
            .color(images[0], LoadOp::DontCare, StoreOp::Store)
            .color(images[1], LoadOp::DontCare, StoreOp::Store)
            .execute(move |ctx| {
                let (Some(color), Some(second)) =
                    (ctx.sampled_index(color), ctx.sampled_index(difference))
                else {
                    return;
                };
                let push = Push {
                    color,
                    second,
                    ..base
                };
                let area = ctx.render_area();
                record(ctx, bindless, pipeline, &push, area);
            });
        Some(images)
    }

    /// Declares SMAA (or MLAA) of `input` into `target`: 1x, S2x with two images, T2x or
    /// 4x with temporal inputs. Nothing when its pipelines were not made
    /// ([`Self::ensure`]) or the ring is full (the tables' first upload).
    pub fn add_smaa<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        ring: &mut UploadRing,
        input: &SmaaInput,
        target: PostTarget,
    ) -> Option<()> {
        let [edges_pipeline, weights_pipeline] = self.presets[input.preset.index()].as_ref()?;
        let blend = self.blend.as_ref()?;
        let tables = self.tables.as_ref()?;
        if !tables[0].is_initialized() {
            tables[0].upload(fg, ring, AREA_TABLE).ok()?;
            tables[1].upload(fg, ring, SEARCH_TABLE).ok()?;
        }
        let [area, search] = [tables[0].sampled_index(), tables[1].sampled_index()];
        let size = input.size;
        let samples = if input.images[1].is_some() { 2 } else { 1 };
        let phase = input.temporal.map(|t| t.phase);
        let temporal = match input.temporal {
            Some(t) => Some((t, self.temporal.as_ref()?)),
            None => None,
        };
        // The result before the resolve (T2x), or the output.
        let current = temporal.map(|_| {
            fg.create_texture(
                "smaa current",
                TextureDesc::new_2d(PERCEPTUAL_FORMAT, size.0, size.1),
            )
        });
        let base = Push {
            area,
            search,
            ..self.push(size)
        };
        for (sample, image) in input.images.iter().enumerate() {
            let Some(image) = *image else {
                continue;
            };
            let indices = subsample_indices(samples, phase, sample);
            let edges = fg.create_texture(
                "smaa edges",
                TextureDesc::new_2d(EDGES_FORMAT, size.0, size.1),
            );
            let weights = fg.create_texture(
                "smaa weights",
                TextureDesc::new_2d(WEIGHTS_FORMAT, size.0, size.1),
            );
            fg.add_pass("smaa edges")
                .read(image, Access::Sampled(ShaderStages::FRAGMENT))
                .color(edges, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .execute(move |ctx| {
                    let Some(color) = ctx.sampled_index(image) else {
                        return;
                    };
                    let area = ctx.render_area();
                    record(ctx, bindless, edges_pipeline, &Push { color, ..base }, area);
                });
            fg.add_pass("smaa weights")
                .read(edges, Access::Sampled(ShaderStages::FRAGMENT))
                .color(weights, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .execute(move |ctx| {
                    let Some(edges) = ctx.sampled_index(edges) else {
                        return;
                    };
                    let push = Push {
                        edges,
                        subsample_indices: indices,
                        ..base
                    };
                    let area = ctx.render_area();
                    record(ctx, bindless, weights_pipeline, &push, area);
                });
            let half = sample == 1;
            match (temporal, current) {
                (Some((t, (pipelines, _))), Some(current)) => {
                    let pipeline = &pipelines[usize::from(half)];
                    let velocity = t.velocity;
                    fg.add_pass("smaa blend")
                        .read(image, Access::Sampled(ShaderStages::FRAGMENT))
                        .read(weights, Access::Sampled(ShaderStages::FRAGMENT))
                        .read(velocity, Access::Sampled(ShaderStages::FRAGMENT))
                        .color(
                            current,
                            if half { LoadOp::Load } else { LoadOp::DontCare },
                            StoreOp::Store,
                        )
                        .execute(move |ctx| {
                            let (Some(color), Some(blend), Some(velocity)) = (
                                ctx.sampled_index(image),
                                ctx.sampled_index(weights),
                                ctx.sampled_index(velocity),
                            ) else {
                                return;
                            };
                            let push = Push {
                                color,
                                blend,
                                velocity,
                                ..base
                            };
                            let area = ctx.render_area();
                            record(ctx, bindless, pipeline, &push, area);
                        });
                }
                _ => {
                    let pipeline = &blend[usize::from(half)];
                    let target = PostTarget {
                        load: if half { LoadOp::Load } else { target.load },
                        ..target
                    };
                    draw(
                        fg,
                        bindless,
                        "smaa blend",
                        pipeline,
                        &[
                            (image, Access::Sampled(ShaderStages::FRAGMENT)),
                            (weights, Access::Sampled(ShaderStages::FRAGMENT)),
                        ],
                        target,
                        move |ctx| {
                            Some(Push {
                                color: ctx.sampled_index(image)?,
                                blend: ctx.sampled_index(weights)?,
                                ..decoded(base, target.decode)
                            })
                        },
                    );
                }
            }
        }
        if let (Some((t, (_, resolve))), Some(current)) = (temporal, current) {
            let previous = t.previous.unwrap_or(current);
            let velocity = t.velocity;
            let reads = [
                (current, Access::Sampled(ShaderStages::FRAGMENT)),
                (velocity, Access::Sampled(ShaderStages::FRAGMENT)),
                (previous, Access::Sampled(ShaderStages::FRAGMENT)),
            ];
            let count = if previous == current { 2 } else { 3 };
            draw(
                fg,
                bindless,
                "smaa resolve",
                resolve,
                &reads[..count],
                target,
                move |ctx| {
                    Some(Push {
                        color: ctx.sampled_index(current)?,
                        velocity: ctx.sampled_index(velocity)?,
                        previous: ctx.sampled_index(previous)?,
                        ..decoded(base, target.decode)
                    })
                },
            );
            let history = t.history;
            fg.add_pass("smaa history")
                .read(current, Access::TransferRead)
                .write(history, Access::TransferWrite)
                .execute(move |ctx| {
                    let layers = vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    };
                    // SAFETY: recording outside a rendering scope; the graph arranged
                    // both layouts.
                    unsafe {
                        ctx.device.cmd_copy_image(
                            ctx.cmd,
                            ctx.image(current),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.image(history),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[vk::ImageCopy {
                                src_subresource: layers,
                                dst_subresource: layers,
                                extent: vk::Extent3D {
                                    width: size.0,
                                    height: size.1,
                                    depth: 1,
                                },
                                ..Default::default()
                            }],
                        );
                    }
                });
        }
        Some(())
    }
}

/// `push` with `decode`'s fields.
fn decoded(push: Push, decode: Decode) -> Push {
    match decode {
        Decode::None => Push { decode: 0, ..push },
        Decode::Srgb { scale } => Push {
            decode: 1,
            decode_scale: scale,
            ..push
        },
    }
}

/// A full-screen pass into `target`'s rectangle, reading `reads`; `push` makes the
/// constants when the pass records (`None` skips it).
#[allow(clippy::too_many_arguments)]
fn draw<'g>(
    fg: &mut FrameGraph<'g>,
    bindless: &'g Bindless,
    name: &'static str,
    pipeline: &'g Pipeline,
    reads: &[(Texture, Access)],
    target: PostTarget,
    push: impl FnOnce(&crate::graph::PassContext<'_>) -> Option<Push> + 'g,
) {
    let mut pass = fg.add_pass(name);
    for &(texture, access) in reads {
        pass = pass.read(texture, access);
    }
    pass.color(target.texture, target.load, StoreOp::Store)
        .execute(move |ctx| {
            let Some(push) = push(ctx) else {
                return;
            };
            let rect = target.rect.unwrap_or(ctx.render_area());
            record(ctx, bindless, pipeline, &push, rect);
        });
}

/// Binds `pipeline` and draws the full-screen triangle over `rect`.
fn record(
    ctx: &crate::graph::PassContext<'_>,
    bindless: &Bindless,
    pipeline: &Pipeline,
    push: &Push,
    rect: vk::Rect2D,
) {
    // SAFETY: recording inside the pass's rendering scope; `rect` lies within it.
    unsafe {
        pipeline.bind(ctx.cmd);
        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
        bindless.push_constants(ctx.cmd, bytemuck::bytes_of(push));
        ctx.device.cmd_set_viewport(
            ctx.cmd,
            0,
            &[vk::Viewport {
                x: rect.offset.x as f32,
                y: rect.offset.y as f32,
                width: rect.extent.width as f32,
                height: rect.extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
        ctx.device.cmd_set_scissor(ctx.cmd, 0, &[rect]);
        ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_have_the_reference_sizes() {
        assert_eq!(AREA_TABLE.len() as u32, AREA_SIZE.0 * AREA_SIZE.1 * 2);
        assert_eq!(SEARCH_TABLE.len() as u32, SEARCH_SIZE.0 * SEARCH_SIZE.1);
    }

    #[test]
    fn subsample_indices_follow_the_reference() {
        assert_eq!(subsample_indices(1, None, 0), [0.0; 4]);
        assert_eq!(subsample_indices(1, Some(0), 0), [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(subsample_indices(1, Some(1), 0), [2.0, 2.0, 2.0, 0.0]);
        assert_eq!(subsample_indices(2, None, 0), [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(subsample_indices(2, None, 1), [2.0, 2.0, 2.0, 0.0]);
        assert_eq!(subsample_indices(2, Some(0), 0), [5.0, 3.0, 1.0, 3.0]);
        assert_eq!(subsample_indices(2, Some(0), 1), [4.0, 6.0, 2.0, 3.0]);
        assert_eq!(subsample_indices(2, Some(1), 0), [3.0, 5.0, 1.0, 4.0]);
        assert_eq!(subsample_indices(2, Some(1), 1), [6.0, 4.0, 2.0, 4.0]);
    }
}
