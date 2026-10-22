//! Wireframes of a view's instances (ADR 0116), for the editor's view modes.
//!
//! - **Lines.** `outlineVertex` (`forward.slang`, the outline's) and `wireFragment`
//!   (`wire.slang`) in the pipeline's line mode draw each instance's finest resident level
//!   as its triangles' edges. They are drawn after the frame at the view's output size, with
//!   the camera without jitter, from draws written on the CPU for the instances whose boxes
//!   the view sees.
//! - **In front or behind** ([`WireView::scene_depth`]): an edge is drawn where its surface
//!   is the one in front. The scene's depth is sampled against the outline's tolerance, so a
//!   line never fights the surface it lies on. Behind another surface, an edge keeps
//!   [`WireStyle::hidden`] of its alpha.
//! - **Lines only:** without the scene's depth, every edge is drawn over
//!   [`WireStyle::background`], as Unreal's and Godot's wireframes are.
//! - **Cost.** Its pipelines are its own, made with the wireframer: views without wireframes
//!   pay nothing. It needs the device's line mode
//!   ([`engine_rhi::OptionalCaps::fill_mode_non_solid`]).

use crate::forward::Push;
use crate::gpu_scene::SceneFrame;
use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture};
use crate::mesh::GeometryFrame;
use crate::outline::{on_screen, set_area};
use crate::upload::UploadRing;
use engine_core::math::Mat4;
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, Blend, Device, GraphicsPipelineDesc, Pipeline, PipelineDesc, RhiError,
    ShaderStageDesc, spirv_words,
};

const LINE_VERTEX: &[u8] = include_bytes!("../shaders/forward.outlineVertex.spv");
const LINE_FRAGMENT: &[u8] = include_bytes!("../shaders/wire.wireFragment.spv");
const FILL_VERTEX: &[u8] = include_bytes!("../shaders/wire.wireFillVertex.spv");
const FILL_FRAGMENT: &[u8] = include_bytes!("../shaders/wire.wireFill.spv");
/// `NO_TEXTURE` in `wire.slang`: no scene depth, every edge drawn.
const NO_TEXTURE: u32 = u32::MAX;
/// Bytes of an indexed indirect draw.
const COMMAND_BYTES: u32 = core::mem::size_of::<vk::DrawIndexedIndirectCommand>() as u32;

/// How wireframes look.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WireStyle {
    /// The lines': linear, straight alpha.
    pub color: [f32; 4],
    /// The alpha an edge keeps behind another surface (with the scene's depth): 0 draws
    /// only what is in front.
    pub hidden: f32,
    /// What the view is filled with first (lines only), or nothing (lines over the frame).
    pub background: Option<[f32; 4]>,
}

impl WireStyle {
    /// Lines only: light edges on a dark background, every one drawn (Wireframe).
    pub const LINES: Self = Self {
        color: [0.72, 0.8, 0.95, 1.0],
        hidden: 1.0,
        background: Some([0.018, 0.02, 0.026, 1.0]),
    };

    /// Over the shaded frame: dark edges of what is in front (Lit + Wireframe).
    pub const OVER: Self = Self {
        color: [0.0, 0.0, 0.0, 0.55],
        hidden: 0.0,
        background: None,
    };
}

/// Where wireframes are drawn and what they are tested against.
#[derive(Debug, Clone, Copy)]
pub struct WireView {
    /// The frame the lines are blended into (the UI's target).
    pub target: Texture,
    /// The view's rectangle in it.
    pub rect: vk::Rect2D,
    /// The view's camera, without jitter.
    pub clip_from_world: Mat4,
    /// Its near and far planes.
    pub near: f32,
    /// See `near`.
    pub far: f32,
    /// The scene's depth (0 near, 1 far; resolved with MSAA) at the render size, for edges
    /// in front only; `None` draws every edge.
    pub scene_depth: Option<Texture>,
    /// The render size.
    pub render: (u32, u32),
}

/// `Params` in `wire.slang` (80 bytes): the unlit view's background reads them too.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct WireParams {
    color: [f32; 4],
    background: [f32; 4],
    scene_scale: [f32; 2],
    origin: [u32; 2],
    near: f32,
    far: f32,
    hidden: f32,
    white: f32,
    _pad: [f32; 4],
}

const _: () = assert!(core::mem::size_of::<WireParams>() == 80);

impl WireParams {
    /// For `wireFill` alone: `background` (linear, straight alpha) at the output's `white`.
    pub(crate) fn fill(background: [f32; 4], white: f32) -> Self {
        Self {
            background,
            white,
            ..bytemuck::Zeroable::zeroed()
        }
    }
}

/// Wireframes in one view (see the module documentation).
#[derive(Debug)]
pub struct Wireframer {
    lines: Pipeline,
    fill: Pipeline,
    format: vk::Format,
    white: f32,
    /// This frame's instances' slots and their draws.
    slots: Vec<u32>,
    commands: Vec<vk::DrawIndexedIndirectCommand>,
}

fn invalid() -> RhiError {
    RhiError::Vulkan {
        call: "embedded SPIR-V",
        result: vk::Result::ERROR_INVALID_SHADER_NV,
    }
}

/// The lines' and the background's pipelines, drawing into frames of `format`.
fn pipelines(
    device: &Device,
    bindless: &Bindless,
    format: vk::Format,
) -> Result<(Pipeline, Pipeline), RhiError> {
    let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
    let (line_vertex, line_fragment) = (words(LINE_VERTEX)?, words(LINE_FRAGMENT)?);
    let (fill_vertex, fill_fragment) = (words(FILL_VERTEX)?, words(FILL_FRAGMENT)?);
    let formats = [format];
    let made = Pipeline::create_many(
        device,
        bindless.pipeline_layout(),
        &[
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats: &formats,
                depth_format: vk::Format::UNDEFINED,
                polygon_mode: vk::PolygonMode::LINE,
                depth_test: false,
                depth_write: false,
                dynamic_culling: true,
                blend: Blend::Premultiplied,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &line_vertex,
                        entry: c"outlineVertex",
                    },
                    Some(ShaderStageDesc {
                        spirv: &line_fragment,
                        entry: c"wireFragment",
                    }),
                )
            }),
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats: &formats,
                depth_format: vk::Format::UNDEFINED,
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
            }),
        ],
    )?;
    let mut made = made.into_iter();
    match (made.next(), made.next()) {
        (Some(lines), Some(fill)) => Ok((lines, fill)),
        _ => Err(RhiError::Vulkan {
            call: "wireframe pipelines (fewer than described)",
            result: vk::Result::ERROR_UNKNOWN,
        }),
    }
}

impl Wireframer {
    /// A wireframer drawing into frames of `format`, whose white is `white` (1, or SDR
    /// white in scRGB units for HDR); `None` on a device without the line mode.
    ///
    /// # Errors
    ///
    /// When a pipeline cannot be made.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        format: vk::Format,
        white: f32,
    ) -> Result<Option<Self>, RhiError> {
        if !device.adapter().caps.fill_mode_non_solid {
            return Ok(None);
        }
        let (lines, fill) = pipelines(device, bindless, format)?;
        Ok(Some(Self {
            lines,
            fill,
            format,
            white,
            slots: Vec::new(),
            commands: Vec::new(),
        }))
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
            (self.lines, self.fill) = pipelines(device, bindless, format)?;
            self.format = format;
        }
        self.white = white;
        Ok(())
    }

    /// The driver's statistics of the pipelines (the lines', the background's), when the
    /// device captures them.
    pub fn statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 2] {
        [self.lines.statistics(device), self.fill.statistics(device)]
    }

    /// Declares the wireframes of the persistent scene's instances that `view` sees, drawn
    /// this frame: how many. Nothing is declared when the view is empty or `ring` is full;
    /// the background alone when no instance is in view.
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        scene: &SceneFrame<'g>,
        view: &WireView,
        style: &WireStyle,
    ) -> u32 {
        engine_profiler::zone!("render::Wireframer::add_passes");
        let (width, height) = (view.rect.extent.width, view.rect.extent.height);
        if width == 0 || height == 0 {
            return 0;
        }
        // The instances in view, each its finest resident level; an instance's number is
        // its draw's first instance.
        self.slots.clear();
        self.commands.clear();
        for slot in 0..scene.slot_count() {
            let Some((mesh, _)) = scene.slot_draw(slot) else {
                continue;
            };
            // Off the view: its box's pixels miss it (one reaching behind the camera may
            // cover anything).
            if let Some(b) = scene.slot_box(slot)
                && let Some([x0, y0, x1, y1]) = on_screen(&view.clip_from_world, b, (width, height))
                && (x1 < 0.0 || y1 < 0.0 || x0 > width as f32 || y0 > height as f32)
            {
                continue;
            }
            let Some((first_index, index_count, first_vertex)) = geometry.finest_draw(mesh) else {
                continue;
            };
            self.commands.push(vk::DrawIndexedIndirectCommand {
                index_count,
                instance_count: 1,
                first_index,
                vertex_offset: first_vertex as i32,
                first_instance: self.slots.len() as u32,
            });
            self.slots.push(slot);
        }
        let count = self.commands.len() as u32;
        if count == 0 && style.background.is_none() {
            return 0;
        }
        let params = WireParams {
            color: style.color,
            background: style.background.unwrap_or([0.0; 4]),
            scene_scale: [
                view.render.0 as f32 / width as f32,
                view.render.1 as f32 / height as f32,
            ],
            origin: [
                view.rect.offset.x.max(0) as u32,
                view.rect.offset.y.max(0) as u32,
            ],
            near: view.near,
            far: view.far,
            hidden: style.hidden,
            white: self.white,
            _pad: [0.0; 4],
        };
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
            ring.push(&[params]),
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
            // The outline's vertex stage reads an item's class here; the wireframe's draws
            // ignore it, so the slots stand in.
            light_tiles: visible,
            batches: params,
            texture: NO_TEXTURE,
            ..bytemuck::Zeroable::zeroed()
        };
        let (lines, fill) = (&self.lines, &self.fill);
        let background = style.background.is_some();
        let rect = view.rect;
        let geometry_copy = *geometry;
        let mut pass = geometry
            .read(fg.add_pass("wireframe"))
            .read(scene.current, Access::StorageRead(ShaderStages::VERTEX));
        if let Some(depth) = view.scene_depth {
            pass = pass.read(depth, Access::Sampled(ShaderStages::FRAGMENT));
        }
        let scene_depth = view.scene_depth;
        pass.color(view.target, LoadOp::Load, StoreOp::Store)
            .execute(move |ctx| {
                let depth = match scene_depth {
                    Some(t) => match ctx.sampled_index(t) {
                        Some(i) => i,
                        None => return,
                    },
                    None => NO_TEXTURE,
                };
                let push = Push {
                    texture: depth,
                    ..push
                };
                // SAFETY: recording inside the pass's rendering scope; the pass reads the
                // pool, the scene's instances and the depth it samples, and the draws lie in
                // the ring's slot for this frame.
                unsafe {
                    set_area(ctx.device, ctx.cmd, rect);
                    if background {
                        fill.bind(ctx.cmd);
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                        ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                    }
                    if count > 0 {
                        lines.bind(ctx.cmd);
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                        geometry_copy.bind_indices(ctx);
                        // Both faces: a back face's edges are a model's too.
                        ctx.device
                            .cmd_set_cull_mode(ctx.cmd, vk::CullModeFlags::NONE);
                        ctx.device
                            .cmd_set_front_face(ctx.cmd, vk::FrontFace::COUNTER_CLOCKWISE);
                        ctx.device.cmd_draw_indexed_indirect(
                            ctx.cmd,
                            commands.buffer,
                            commands.offset,
                            count,
                            COMMAND_BYTES,
                        );
                    }
                }
            });
        count
    }
}
