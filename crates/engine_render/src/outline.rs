//! Outlines of chosen instances (ADR 0109), for the editor: its selection and the instance
//! under the pointer.
//!
//! - **The mask.** [`Outliner::add_passes`] draws the chosen instances alone into an id mask
//!   at the view's output size, with a depth of its own and the camera without jitter, from
//!   commands written on the CPU (each mesh's finest resident level): `outlineVertex` and
//!   `outlineFragment` in `forward.slang`, masked materials alpha-tested. Each pixel keeps
//!   its item's class and number.
//! - **The edge.** `outlineEdge` (`outline.slang`) blends a line around the mask over the
//!   frame: each class its width and color, the strongest class first (the active
//!   instance, then the selection, then the hovered one), with a soft outer edge. Where the
//!   outlined surface lies behind the scene's own depth, the line keeps
//!   [`OutlineStyle::hidden`] of its alpha.
//! - **Cost.** Its pipelines are its own: views without outlines pay nothing (the register
//!   rule, CLAUDE.md §5). Its cost is the chosen instances' vertex work and the pixels
//!   their world boxes cover on the screen with their lines (all of the view when a box
//!   reaches behind the camera): the mask spans those rectangles' union, the edge pass
//!   draws each rectangle (merged until disjoint).

use crate::forward::{MASKED, Push};
use crate::gpu_scene::SceneFrame;
use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture, TextureDesc};
use crate::material::MaterialFrame;
use crate::mesh::GeometryFrame;
use crate::upload::UploadRing;
use engine_core::math::{Mat4, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{
    Bindless, Blend, Device, GraphicsPipelineDesc, Pipeline, PipelineDesc, RhiError,
    ShaderStageDesc, spirv_words,
};

const MASK_VERTEX: &[u8] = include_bytes!("../shaders/forward.outlineVertex.spv");
const MASK_VERTEX_MASKED: &[u8] = include_bytes!("../shaders/forward.outlineVertexMasked.spv");
const MASK_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.outlineFragment.spv");
const MASK_FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.outlineFragmentMasked.spv");
const EDGE_VERTEX: &[u8] = include_bytes!("../shaders/outline.outlineEdgeVertex.spv");
const EDGE_FRAGMENT: &[u8] = include_bytes!("../shaders/outline.outlineEdge.spv");
const MASK_FORMAT: vk::Format = vk::Format::R32_UINT;
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
/// Bytes of an indexed indirect draw.
const COMMAND_BYTES: u32 = core::mem::size_of::<vk::DrawIndexedIndirectCommand>() as u32;

/// How an instance is outlined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineClass {
    /// Selected.
    Selected = 0,
    /// The active one of the selection.
    Active = 1,
    /// Under the pointer.
    Hovered = 2,
}

/// An instance to outline: the GPU scene's slot, and its class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutlineItem {
    /// The GPU scene's slot.
    pub slot: u32,
    /// How it is outlined.
    pub class: OutlineClass,
}

/// How outlines look, by [`OutlineClass`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlineStyle {
    /// Linear colors with straight alpha.
    pub colors: [[f32; 4]; 3],
    /// Widths in the view's pixels.
    pub widths: [f32; 3],
    /// The alpha a line keeps where its surface is hidden behind another.
    pub hidden: f32,
}

impl Default for OutlineStyle {
    /// Orange for the selection, lighter for its active instance, both 2 pixels; a thin
    /// translucent light line for the hovered one; hidden parts at 35 % (ADR 0109).
    fn default() -> Self {
        Self {
            colors: [
                [1.0, 0.32, 0.02, 1.0],
                [1.0, 0.62, 0.18, 1.0],
                [0.85, 0.85, 0.85, 0.6],
            ],
            widths: [2.0, 2.0, 1.0],
            hidden: 0.35,
        }
    }
}

impl OutlineStyle {
    /// The widths times `scale` (the UI's scale on a high-DPI display).
    pub fn scaled(self, scale: f32) -> Self {
        Self {
            widths: self.widths.map(|w| w * scale),
            ..self
        }
    }
}

/// Where outlines are drawn and what they are tested against.
#[derive(Debug, Clone, Copy)]
pub struct OutlineView {
    /// The frame the lines are blended into (the UI's target).
    pub target: Texture,
    /// The view's rectangle in it: the mask's size and place.
    pub rect: vk::Rect2D,
    /// The view's camera, without jitter.
    pub clip_from_world: Mat4,
    /// Its near and far planes.
    pub near: f32,
    /// See `near`.
    pub far: f32,
    /// The scene's depth (0 near, 1 far; resolved with MSAA), at the render size.
    pub scene_depth: Texture,
    /// The render size.
    pub render: (u32, u32),
}

/// `Push` in `outline.slang` (112 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct EdgePush {
    colors: [[f32; 4]; 3],
    widths: [f32; 4],
    mask: u32,
    mask_depth: u32,
    scene_depth: u32,
    white: f32,
    scene_scale: [f32; 2],
    origin: [u32; 2],
    near: f32,
    far: f32,
    mask_offset: [u32; 2],
}

const _: () = assert!(core::mem::size_of::<EdgePush>() == 112);

/// Outlines in one view (see the module documentation).
#[derive(Debug)]
pub struct Outliner {
    /// The mask's: opaque and masked.
    mask: [Pipeline; 2],
    edge: Pipeline,
    format: vk::Format,
    white: f32,
    /// This frame's items' slots and classes, their draws, the view's pixels they cover
    /// with their lines (merged until disjoint), and those as the edge pass's scissors.
    slots: Vec<u32>,
    classes: Vec<u32>,
    commands: Vec<vk::DrawIndexedIndirectCommand>,
    rects: Vec<[u32; 4]>,
    scissors: Vec<vk::Rect2D>,
}

fn invalid() -> RhiError {
    RhiError::Vulkan {
        call: "embedded SPIR-V",
        result: vk::Result::ERROR_INVALID_SHADER_NV,
    }
}

fn edge_pipeline(
    device: &Device,
    bindless: &Bindless,
    format: vk::Format,
) -> Result<Pipeline, RhiError> {
    let vertex = spirv_words(EDGE_VERTEX).ok_or_else(invalid)?;
    let fragment = spirv_words(EDGE_FRAGMENT).ok_or_else(invalid)?;
    Pipeline::create_many(
        device,
        bindless.pipeline_layout(),
        &[PipelineDesc::Graphics(GraphicsPipelineDesc {
            color_formats: &[format],
            depth_format: vk::Format::UNDEFINED,
            depth_test: false,
            depth_write: false,
            cull_mode: vk::CullModeFlags::NONE,
            blend: Blend::Premultiplied,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &vertex,
                    entry: c"outlineEdgeVertex",
                },
                Some(ShaderStageDesc {
                    spirv: &fragment,
                    entry: c"outlineEdge",
                }),
            )
        })],
    )?
    .into_iter()
    .next()
    .ok_or(RhiError::Vulkan {
        call: "outline edge pipeline (none made)",
        result: vk::Result::ERROR_UNKNOWN,
    })
}

impl Outliner {
    /// An outliner drawing into frames of `format`, whose white is `white` (1, or SDR
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
        fn desc<'a>(
            (vertex, v): (&'a [u32], &'static core::ffi::CStr),
            (fragment, f): (&'a [u32], &'static core::ffi::CStr),
        ) -> PipelineDesc<'a> {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats: &[MASK_FORMAT],
                depth_format: DEPTH_FORMAT,
                dynamic_culling: true,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: vertex,
                        entry: v,
                    },
                    Some(ShaderStageDesc {
                        spirv: fragment,
                        entry: f,
                    }),
                )
            })
        }
        let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
        let (vertex, vertex_masked) = (words(MASK_VERTEX)?, words(MASK_VERTEX_MASKED)?);
        let (fragment, fragment_masked) = (words(MASK_FRAGMENT)?, words(MASK_FRAGMENT_MASKED)?);
        let mut mask = Pipeline::create_many(
            device,
            bindless.pipeline_layout(),
            &[
                desc((&vertex, c"outlineVertex"), (&fragment, c"outlineFragment")),
                desc(
                    (&vertex_masked, c"outlineVertexMasked"),
                    (&fragment_masked, c"outlineFragmentMasked"),
                ),
            ],
        )?
        .into_iter();
        let (Some(opaque), Some(masked)) = (mask.next(), mask.next()) else {
            return Err(RhiError::Vulkan {
                call: "outline mask pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            });
        };
        Ok(Self {
            mask: [opaque, masked],
            edge: edge_pipeline(device, bindless, format)?,
            format,
            white,
            slots: Vec::new(),
            classes: Vec::new(),
            commands: Vec::new(),
            rects: Vec::new(),
            scissors: Vec::new(),
        })
    }

    /// The frames' format or white changed (HDR turned on or off): the edge's pipeline is
    /// made again for a new format. Call with the device idle.
    ///
    /// # Errors
    ///
    /// When the pipeline cannot be made.
    pub fn set_output(
        &mut self,
        device: &Device,
        bindless: &Bindless,
        format: vk::Format,
        white: f32,
    ) -> Result<(), RhiError> {
        if format != self.format {
            self.edge = edge_pipeline(device, bindless, format)?;
            self.format = format;
        }
        self.white = white;
        Ok(())
    }

    /// The driver's statistics of the pipelines (the mask's two, the edge's), when the
    /// device captures them.
    pub fn statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 3] {
        [
            self.mask[0].statistics(device),
            self.mask[1].statistics(device),
            self.edge.statistics(device),
        ]
    }

    /// Declares the outlines of `items` (the persistent scene's slots) in a view drawn this
    /// frame. False, declaring nothing, when no item can be drawn (gone, or without a
    /// resident mesh), the view is empty, or `ring` is full; true without passes when
    /// every item is off the view.
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        scene: &SceneFrame<'g>,
        items: &[OutlineItem],
        view: &OutlineView,
        style: &OutlineStyle,
    ) -> bool {
        engine_profiler::zone!("render::Outliner::add_passes");
        let (width, height) = (view.rect.extent.width, view.rect.extent.height);
        if items.is_empty() || width == 0 || height == 0 {
            return false;
        }
        // The lines reach this far out of an item's box on the screen.
        let margin = style.widths.iter().fold(0.0f32, |a, &w| a.max(w)) + 2.0;
        // The items' draws, opaque ones first, then masked ones: an item's number is its
        // draw's first instance. And the pixels each covers with its line, in the view.
        self.slots.clear();
        self.classes.clear();
        self.commands.clear();
        self.rects.clear();
        let mut whole = false;
        let mut opaque = 0;
        for masked in [false, true] {
            for item in items {
                let Some((mesh, state)) = scene.slot_draw(item.slot) else {
                    continue;
                };
                if (usize::from(state) & MASKED != 0) != masked {
                    continue;
                }
                let Some((first_index, index_count, first_vertex)) = geometry.finest_draw(mesh)
                else {
                    continue;
                };
                self.commands.push(vk::DrawIndexedIndirectCommand {
                    index_count,
                    instance_count: 1,
                    first_index,
                    vertex_offset: first_vertex as i32,
                    first_instance: self.slots.len() as u32,
                });
                self.slots.push(item.slot);
                self.classes.push(item.class as u32);
                match scene
                    .slot_box(item.slot)
                    .and_then(|b| on_screen(&view.clip_from_world, b, (width, height)))
                {
                    Some([x0, y0, x1, y1]) => {
                        let r = [
                            (x0 - margin).floor().clamp(0.0, width as f32) as u32,
                            (y0 - margin).floor().clamp(0.0, height as f32) as u32,
                            (x1 + margin).ceil().clamp(0.0, width as f32) as u32,
                            (y1 + margin).ceil().clamp(0.0, height as f32) as u32,
                        ];
                        if r[0] < r[2] && r[1] < r[3] {
                            self.rects.push(r);
                        }
                    }
                    None => whole = true,
                }
            }
            if !masked {
                opaque = self.commands.len() as u32;
            }
        }
        let total = self.commands.len() as u32;
        if total == 0 {
            return false;
        }
        if whole {
            self.rects.clear();
            self.rects.push([0, 0, width, height]);
        }
        if self.rects.is_empty() {
            // Every item is off the view.
            return true;
        }
        disjoint(&mut self.rects);
        // The mask covers the rectangles' union only: the view's pixel grid shifted onto it.
        let [ux0, uy0, ux1, uy1] = self.rects.iter().fold([u32::MAX, u32::MAX, 0, 0], |u, r| {
            [
                u[0].min(r[0]),
                u[1].min(r[1]),
                u[2].max(r[2]),
                u[3].max(r[3]),
            ]
        });
        let (mask_width, mask_height) = (ux1 - ux0, uy1 - uy0);
        let (Some(frame), Some(visible), Some(classes), Some(commands)) = (
            ring.push(&[view.clip_from_world.to_cols_array()]),
            ring.push(&self.slots),
            ring.push(&self.classes),
            ring.stage(&self.commands),
        ) else {
            return false;
        };
        let push = Push {
            frame,
            instances: scene.current_address,
            visible,
            positions: geometry.position_address(),
            attributes: geometry.attribute_address(),
            materials: materials.address(),
            light_tiles: classes,
            ..bytemuck::Zeroable::zeroed()
        };
        let mask = fg.create_texture(
            "outline mask",
            TextureDesc::new_2d(MASK_FORMAT, mask_width, mask_height),
        );
        let depth = fg.create_texture(
            "outline depth",
            TextureDesc::new_2d(DEPTH_FORMAT, mask_width, mask_height),
        );
        let shifted = vk::Rect2D {
            offset: vk::Offset2D {
                x: -(ux0 as i32),
                y: -(uy0 as i32),
            },
            extent: vk::Extent2D { width, height },
        };
        let mask_area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: mask_width,
                height: mask_height,
            },
        };
        let pipelines = &self.mask;
        let geometry_copy = *geometry;
        materials
            .read(geometry.read(fg.add_pass("outline mask")))
            .read(scene.current, Access::StorageRead(ShaderStages::VERTEX))
            .color(mask, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .depth(depth, LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), StoreOp::Store)
            .execute(move |ctx| {
                for (k, pipeline) in pipelines.iter().enumerate() {
                    let (first, count) = if k == 0 {
                        (0, opaque)
                    } else {
                        (opaque, total - opaque)
                    };
                    if count == 0 {
                        continue;
                    }
                    // SAFETY: recording inside the pass's rendering scope; the pass reads the
                    // pool, the material table and the scene's instances, and the draws lie
                    // in the ring's slot for this frame.
                    unsafe {
                        pipeline.bind(ctx.cmd);
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        set_viewport(ctx.device, ctx.cmd, shifted);
                        ctx.device.cmd_set_scissor(ctx.cmd, 0, &[mask_area]);
                        bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                        geometry_copy.bind_indices(ctx);
                        // Both faces: a single-sided mesh seen from behind is outlined too.
                        ctx.device
                            .cmd_set_cull_mode(ctx.cmd, vk::CullModeFlags::NONE);
                        ctx.device
                            .cmd_set_front_face(ctx.cmd, vk::FrontFace::COUNTER_CLOCKWISE);
                        ctx.device.cmd_draw_indexed_indirect(
                            ctx.cmd,
                            commands.buffer,
                            commands.offset + u64::from(first * COMMAND_BYTES),
                            count,
                            COMMAND_BYTES,
                        );
                    }
                }
            });
        let edge = &self.edge;
        let (left, top) = (view.rect.offset.x, view.rect.offset.y);
        let base = EdgePush {
            colors: style.colors,
            widths: [
                style.widths[0],
                style.widths[1],
                style.widths[2],
                style.hidden,
            ],
            mask: 0,
            mask_depth: 0,
            scene_depth: 0,
            white: self.white,
            scene_scale: [
                view.render.0 as f32 / width as f32,
                view.render.1 as f32 / height as f32,
            ],
            origin: [
                (left + ux0 as i32).max(0) as u32,
                (top + uy0 as i32).max(0) as u32,
            ],
            near: view.near,
            far: view.far,
            mask_offset: [ux0, uy0],
        };
        // Each rectangle drawn once, in the target's pixels.
        self.scissors.clear();
        self.scissors.extend(self.rects.iter().map(|r| vk::Rect2D {
            offset: vk::Offset2D {
                x: left + r[0] as i32,
                y: top + r[1] as i32,
            },
            extent: vk::Extent2D {
                width: r[2] - r[0],
                height: r[3] - r[1],
            },
        }));
        let scissors = &self.scissors;
        let (rect, scene_depth) = (view.rect, view.scene_depth);
        fg.add_pass("outline")
            .read(mask, Access::Sampled(ShaderStages::FRAGMENT))
            .read(depth, Access::Sampled(ShaderStages::FRAGMENT))
            .read(scene_depth, Access::Sampled(ShaderStages::FRAGMENT))
            .color(view.target, LoadOp::Load, StoreOp::Store)
            .execute(move |ctx| {
                let (Some(mask), Some(mask_depth), Some(scene_depth)) = (
                    ctx.sampled_index(mask),
                    ctx.sampled_index(depth),
                    ctx.sampled_index(scene_depth),
                ) else {
                    return;
                };
                let push = EdgePush {
                    mask,
                    mask_depth,
                    scene_depth,
                    ..base
                };
                // SAFETY: recording inside the pass's rendering scope; the pass reads the
                // three images it samples.
                unsafe {
                    edge.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_area(ctx.device, ctx.cmd, rect);
                    bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                    for scissor in scissors {
                        ctx.device
                            .cmd_set_scissor(ctx.cmd, 0, core::slice::from_ref(scissor));
                        ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                    }
                }
            });
        true
    }
}

/// More rectangles than this are drawn as their union.
const MAX_RECTS: usize = 32;

/// Merges overlapping rectangles (`[x0, y0, x1, y1]`) until none overlap, so the edge pass
/// draws each pixel once; more than [`MAX_RECTS`] become their union.
fn disjoint(rects: &mut Vec<[u32; 4]>) {
    let union = |a: [u32; 4], b: [u32; 4]| {
        [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ]
    };
    if rects.len() > MAX_RECTS {
        let all = rects.iter().copied().reduce(union);
        rects.clear();
        rects.extend(all);
        return;
    }
    let overlap =
        |a: [u32; 4], b: [u32; 4]| a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3];
    let mut i = 0;
    while i < rects.len() {
        let mut grew = false;
        let mut j = i + 1;
        while j < rects.len() {
            if overlap(rects[i], rects[j]) {
                rects[i] = union(rects[i], rects[j]);
                rects.swap_remove(j);
                grew = true;
            } else {
                j += 1;
            }
        }
        // A grown rectangle may now reach one before it.
        i = if grew { 0 } else { i + 1 };
    }
}

/// The pixels a world box covers in a view of `size` through `clip_from_world`, as
/// `[x0, y0, x1, y1]`; `None` when a corner lies behind the camera (it may cover anything).
fn on_screen(
    clip_from_world: &Mat4,
    (min, max): (Vec3, Vec3),
    size: (u32, u32),
) -> Option<[f32; 4]> {
    let mut r = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for k in 0..8 {
        let corner = Vec3::new(
            if k & 1 == 0 { min.x } else { max.x },
            if k & 2 == 0 { min.y } else { max.y },
            if k & 4 == 0 { min.z } else { max.z },
        );
        let c = *clip_from_world * corner.extend(1.0);
        if c.w <= 1e-6 {
            return None;
        }
        let x = (c.x / c.w * 0.5 + 0.5) * size.0 as f32;
        let y = (c.y / c.w * 0.5 + 0.5) * size.1 as f32;
        r = [r[0].min(x), r[1].min(y), r[2].max(x), r[3].max(y)];
    }
    Some(r)
}

/// Sets the viewport to `area` (its offset may be negative: a shifted pixel grid).
///
/// # Safety
///
/// `cmd` is recording.
unsafe fn set_viewport(device: &engine_rhi::ash::Device, cmd: vk::CommandBuffer, area: vk::Rect2D) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_set_viewport(
            cmd,
            0,
            &[vk::Viewport {
                x: area.offset.x as f32,
                y: area.offset.y as f32,
                width: area.extent.width as f32,
                height: area.extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
    }
}

/// Sets the viewport and the scissor to `area`.
///
/// # Safety
///
/// `cmd` is recording; `area` lies in the attachments.
unsafe fn set_area(device: &engine_rhi::ash::Device, cmd: vk::CommandBuffer, area: vk::Rect2D) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        set_viewport(device, cmd, area);
        device.cmd_set_scissor(cmd, 0, &[area]);
    }
}
