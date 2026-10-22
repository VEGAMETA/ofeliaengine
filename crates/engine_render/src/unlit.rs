//! The unlit view (ADR 0116 V1b), for the editor's view modes: each instance the view sees
//! drawn again over the frame in its material's base color and emission, without light.
//!
//! - **Its own pass,** as the wireframes are: `unlitVertex` and `unlitFragment*`
//!   (`forward.slang`) draw each instance's finest resident level at the view's output size,
//!   with the camera without jitter and a depth of their own, over a background that fills
//!   the view first. Opaque and masked surfaces go first (masked ones alpha-tested), then
//!   blended ones over them, unsorted.
//! - **Materials** are read as the engine's own pipelines read them (a material type's glTF
//!   fallback, ADR 0092): a type's own base color code does not run here.
//! - **Cost.** Its pipelines are its own: views that are not unlit pay nothing, and the
//!   forward pass is untouched.

use crate::forward::{BLENDED, DOUBLE_SIDED, MASKED, MIRRORED, Push};
use crate::gpu_scene::SceneFrame;
use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc};
use crate::material::MaterialFrame;
use crate::mesh::GeometryFrame;
use crate::outline::{on_screen, set_area};
use crate::upload::UploadRing;
use crate::wire::WireParams;
use engine_core::math::Mat4;
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, Blend, Device, GraphicsPipelineDesc, Pipeline, PipelineDesc, RhiError,
    ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.unlitVertex.spv");
const OPAQUE_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.unlitFragment.spv");
const MASKED_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.unlitFragmentMasked.spv");
const BLENDED_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.unlitFragmentBlended.spv");
const FILL_VERTEX: &[u8] = include_bytes!("../shaders/wire.wireFillVertex.spv");
const FILL_FRAGMENT: &[u8] = include_bytes!("../shaders/wire.wireFill.spv");
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
/// Bytes of an indexed indirect draw.
const COMMAND_BYTES: u32 = core::mem::size_of::<vk::DrawIndexedIndirectCommand>() as u32;

/// What the view shows where no surface is: a dark gray, linear.
pub const BACKGROUND: [f32; 4] = [0.05, 0.05, 0.056, 1.0];

/// Where the unlit view is drawn.
#[derive(Debug, Clone, Copy)]
pub struct UnlitView {
    /// The frame the surfaces are drawn into (the UI's target).
    pub target: Texture,
    /// The view's rectangle in it.
    pub rect: vk::Rect2D,
    /// The view's camera, without jitter.
    pub clip_from_world: Mat4,
}

/// The unlit view (see the module documentation).
#[derive(Debug)]
pub struct Unlit {
    /// The background's, then the surfaces': opaque, masked, blended.
    fill: Pipeline,
    surfaces: [Pipeline; 3],
    format: vk::Format,
    white: f32,
    /// This frame's draws, in drawing order: each instance's state, slot and draw.
    draws: Vec<(u8, u32, vk::DrawIndexedIndirectCommand)>,
    slots: Vec<u32>,
    commands: Vec<vk::DrawIndexedIndirectCommand>,
    /// Runs of one state: the state, the first draw, how many.
    runs: Vec<(u8, u32, u32)>,
}

fn invalid() -> RhiError {
    RhiError::Vulkan {
        call: "embedded SPIR-V",
        result: vk::Result::ERROR_INVALID_SHADER_NV,
    }
}

/// The background's pipeline and the surfaces' (opaque, masked, blended), drawing into
/// frames of `format` with a depth of their own.
fn pipelines(
    device: &Device,
    bindless: &Bindless,
    format: vk::Format,
) -> Result<(Pipeline, [Pipeline; 3]), RhiError> {
    let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
    let (vertex, fill_vertex, fill_fragment) =
        (words(VERTEX)?, words(FILL_VERTEX)?, words(FILL_FRAGMENT)?);
    let fragments = [
        (words(OPAQUE_FRAGMENT)?, c"unlitFragment"),
        (words(MASKED_FRAGMENT)?, c"unlitFragmentMasked"),
        (words(BLENDED_FRAGMENT)?, c"unlitFragmentBlended"),
    ];
    let formats = [format];
    let mut descs = vec![PipelineDesc::Graphics(GraphicsPipelineDesc {
        color_formats: &formats,
        depth_format: DEPTH_FORMAT,
        depth_test: false,
        depth_write: false,
        cull_mode: vk::CullModeFlags::NONE,
        blend: Blend::Premultiplied,
        ..GraphicsPipelineDesc::new(
            ShaderStageDesc {
                spirv: &fill_vertex,
                entry: c"wireFillVertex",
            },
            Some(ShaderStageDesc {
                spirv: &fill_fragment,
                entry: c"wireFill",
            }),
        )
    })];
    for (k, (fragment, entry)) in fragments.iter().enumerate() {
        let blended = k == 2;
        descs.push(PipelineDesc::Graphics(GraphicsPipelineDesc {
            color_formats: &formats,
            depth_format: DEPTH_FORMAT,
            depth_write: !blended,
            dynamic_culling: true,
            blend: if blended {
                Blend::Premultiplied
            } else {
                Blend::Opaque
            },
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &vertex,
                    entry: c"unlitVertex",
                },
                Some(ShaderStageDesc {
                    spirv: fragment,
                    entry,
                }),
            )
        }));
    }
    let mut made = Pipeline::create_many(device, bindless.pipeline_layout(), &descs)?.into_iter();
    match (made.next(), made.next(), made.next(), made.next()) {
        (Some(fill), Some(opaque), Some(masked), Some(blended)) => {
            Ok((fill, [opaque, masked, blended]))
        }
        _ => Err(RhiError::Vulkan {
            call: "unlit pipelines (fewer than described)",
            result: vk::Result::ERROR_UNKNOWN,
        }),
    }
}

impl Unlit {
    /// An unlit view drawing into frames of `format`, whose white is `white` (1, or SDR
    /// white in scRGB units for HDR).
    ///
    /// # Errors
    ///
    /// When a pipeline cannot be made.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        format: vk::Format,
        white: f32,
    ) -> Result<Self, RhiError> {
        let (fill, surfaces) = pipelines(device, bindless, format)?;
        Ok(Self {
            fill,
            surfaces,
            format,
            white,
            draws: Vec::new(),
            slots: Vec::new(),
            commands: Vec::new(),
            runs: Vec::new(),
        })
    }

    /// The frames' format or white changed (HDR turned on or off): the pipelines are made
    /// again for a new format. Call with the device idle.
    ///
    /// # Errors
    ///
    /// When a pipeline cannot be made.
    pub fn set_output(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        format: vk::Format,
        white: f32,
    ) -> Result<(), RhiError> {
        if format != self.format {
            (self.fill, self.surfaces) = pipelines(device, bindless, format)?;
            self.format = format;
        }
        self.white = white;
        Ok(())
    }

    /// The driver's statistics of the pipelines (the background's, then the opaque,
    /// masked and blended surfaces'), when the device captures them.
    pub fn statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 4] {
        [
            self.fill.statistics(device),
            self.surfaces[0].statistics(device),
            self.surfaces[1].statistics(device),
            self.surfaces[2].statistics(device),
        ]
    }

    /// Declares the unlit view of the persistent scene's instances that `view` sees, over
    /// `background`: how many instances it draws. Nothing is declared when the view is
    /// empty or `ring` is full.
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        scene: &SceneFrame<'g>,
        view: &UnlitView,
        background: [f32; 4],
    ) -> u32 {
        engine_profiler::zone!("render::Unlit::add_passes");
        let (width, height) = (view.rect.extent.width, view.rect.extent.height);
        if width == 0 || height == 0 {
            return 0;
        }
        // The instances in view, each its finest resident level, ordered by state: the
        // blended ones last.
        self.draws.clear();
        for slot in 0..scene.slot_count() {
            let Some((mesh, state)) = scene.slot_draw(slot) else {
                continue;
            };
            if let Some(b) = scene.slot_box(slot)
                && let Some([x0, y0, x1, y1]) = on_screen(&view.clip_from_world, b, (width, height))
                && (x1 < 0.0 || y1 < 0.0 || x0 > width as f32 || y0 > height as f32)
            {
                continue;
            }
            let Some((first_index, index_count, first_vertex)) = geometry.finest_draw(mesh) else {
                continue;
            };
            let draw = vk::DrawIndexedIndirectCommand {
                index_count,
                instance_count: 1,
                first_index,
                vertex_offset: first_vertex as i32,
                first_instance: 0,
            };
            self.draws.push((state, slot, draw));
        }
        self.draws
            .sort_by_key(|&(state, slot, _)| (usize::from(state) & BLENDED != 0, state, slot));
        // An instance's number is its draw's first instance.
        self.slots.clear();
        self.commands.clear();
        self.runs.clear();
        for (i, &(state, slot, draw)) in self.draws.iter().enumerate() {
            self.slots.push(slot);
            self.commands.push(vk::DrawIndexedIndirectCommand {
                first_instance: i as u32,
                ..draw
            });
            match self.runs.last_mut() {
                Some(run) if run.0 == state => run.2 += 1,
                _ => self.runs.push((state, i as u32, 1)),
            }
        }
        let count = self.commands.len() as u32;
        // Empty lists still need addresses the pass may give the shaders.
        let slots: &[u32] = if self.slots.is_empty() {
            &[0]
        } else {
            &self.slots
        };
        let none = [vk::DrawIndexedIndirectCommand::default()];
        let draws: &[vk::DrawIndexedIndirectCommand] = if self.commands.is_empty() {
            &none
        } else {
            &self.commands
        };
        let (Some(frame), Some(visible), Some(params), Some(commands)) = (
            ring.push(&[view.clip_from_world.to_cols_array()]),
            ring.push(slots),
            ring.push(&[WireParams::fill(background, self.white)]),
            ring.stage(draws),
        ) else {
            return 0;
        };
        let push = Push {
            frame,
            instances: scene.current_address,
            visible,
            positions: geometry.position_address(),
            attributes: geometry.attribute_address(),
            materials: materials.address(),
            // The background's parameters (`wire.slang`'s), and the output's white.
            batches: params,
            phase: self.white.to_bits(),
            ..bytemuck::Zeroable::zeroed()
        };
        // A depth of its own, as large as the target: a pass's attachments share its area.
        let target = fg.texture_desc(view.target);
        let depth = fg.create_texture(
            "unlit depth",
            TextureDesc::new_2d(DEPTH_FORMAT, target.width, target.height),
        );
        let (fill, surfaces, runs) = (&self.fill, &self.surfaces, &self.runs);
        let rect = view.rect;
        let geometry_copy = *geometry;
        materials
            .read(geometry.read(fg.add_pass("unlit")))
            .read(scene.current, Access::StorageRead(ShaderStages::VERTEX))
            .color(view.target, LoadOp::Load, StoreOp::Store)
            .depth(
                depth,
                LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                StoreOp::DontCare,
            )
            .execute(move |ctx| {
                // SAFETY: recording inside the pass's rendering scope; the pass reads the
                // pool, the material table and the scene's instances, and the draws lie in
                // the ring's slot for this frame.
                unsafe {
                    set_area(ctx.device, ctx.cmd, rect);
                    fill.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                    if count == 0 {
                        return;
                    }
                    geometry_copy.bind_indices(ctx);
                    for &(state, first, n) in runs {
                        let state = usize::from(state);
                        let k = if state & BLENDED != 0 {
                            2
                        } else if state & MASKED != 0 {
                            1
                        } else {
                            0
                        };
                        surfaces[k].bind(ctx.cmd);
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                        let cull = if state & DOUBLE_SIDED != 0 {
                            vk::CullModeFlags::NONE
                        } else {
                            vk::CullModeFlags::BACK
                        };
                        // A mirrored instance's front faces wind the other way on screen.
                        let front = if state & MIRRORED != 0 {
                            vk::FrontFace::CLOCKWISE
                        } else {
                            vk::FrontFace::COUNTER_CLOCKWISE
                        };
                        ctx.device.cmd_set_cull_mode(ctx.cmd, cull);
                        ctx.device.cmd_set_front_face(ctx.cmd, front);
                        ctx.device.cmd_draw_indexed_indirect(
                            ctx.cmd,
                            commands.buffer,
                            commands.offset + u64::from(first * COMMAND_BYTES),
                            n,
                            COMMAND_BYTES,
                        );
                    }
                }
            });
        count
    }
}
