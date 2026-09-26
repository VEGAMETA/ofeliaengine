//! Drawing a [`Canvas`] into a render graph target.

use crate::canvas::{Canvas, Instance, PIPELINE_MULTIPLY};
use crate::view::View2d;
use engine_render::graph::{FrameGraph, LoadOp, StoreOp, Texture};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Blend, Device, GraphicsPipelineDesc, MemoryLocation, Pipeline,
    RhiError, ShaderStageDesc, spirv_words,
};

const VERTEX_INDEXED: &[u8] = include_bytes!("../shaders/sprite.vertexIndexed.spv");
const VERTEX_LIST: &[u8] = include_bytes!("../shaders/sprite.vertexList.spv");
const VERTEX_INSTANCED: &[u8] = include_bytes!("../shaders/sprite.vertexInstanced.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/sprite.fragmentMain.spv");

/// How quads become vertices. [`QuadMode::for_vendor`] picks the fastest measured one for
/// a GPU vendor (ADR 0015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuadMode {
    /// A shared index buffer (`0 1 2 2 1 3` + 4k): four vertex shader invocations per
    /// quad, reused through the post-transform cache.
    #[default]
    Indexed,
    /// No index buffer, six vertices per quad.
    List,
    /// One six-vertex instance per quad.
    Instanced,
}

impl QuadMode {
    /// The fastest mode measured for a PCI vendor id (ref-1, ADR 0015): indexed on NVIDIA
    /// (about 11% faster than the others), a plain vertex list on AMD (indexed is 13–37%
    /// slower there) and, until measured, on everything else.
    pub fn for_vendor(vendor_id: u32) -> Self {
        match vendor_id {
            0x10DE => Self::Indexed,
            _ => Self::List,
        }
    }

    /// [`Self::for_vendor`] for `device`'s adapter.
    pub fn for_device(device: &Device) -> Self {
        Self::for_vendor(device.adapter().vendor_id)
    }
}

/// Quads per indexed draw: the index buffer covers this many with 16-bit indices, and
/// longer batches are split (a draw call per 16384 quads).
const QUADS_PER_DRAW: u32 = 16_384;

/// `Push` in `shaders/sprite.slang` (88 bytes).
#[repr(C)]
struct Push {
    clip_from_world: [f32; 16],
    instances: vk::DeviceAddress,
    order: vk::DeviceAddress,
    aa_expand: f32,
    view_flags: u32,
}

const _: () = assert!(core::mem::size_of::<Push>() == 88);
const VIEW_Y_DOWN: u32 = 1;
const VIEW_INDIRECT: u32 = 2;

/// Why [`Renderer2d::add_pass`] declared nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Render2dError {
    /// The upload ring has no room for this frame's quads.
    RingFull {
        /// Bytes the quads needed.
        needed: u64,
    },
    /// The canvas changed after its last [`Canvas::prepare`].
    NotPrepared,
}

impl core::fmt::Display for Render2dError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RingFull { needed } => {
                write!(f, "upload ring has no room for {needed} bytes of quads")
            }
            Self::NotPrepared => write!(f, "canvas changed after Canvas::prepare"),
        }
    }
}

impl std::error::Error for Render2dError {}

/// Draws canvases into one color format.
pub struct Renderer2d {
    /// Indexed by the batch pipeline selector.
    pipelines: [Pipeline; 2],
    mode: QuadMode,
    index_buffer: vk::Buffer,
    index_allocation: Option<Allocation>,
}

impl core::fmt::Debug for Renderer2d {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Renderer2d")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Renderer2d {
    /// Creates the pipelines for `color_format` (and the shared index buffer).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        color_format: vk::Format,
        mode: QuadMode,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let (vertex_spirv, vertex_entry) = match mode {
            QuadMode::Indexed => (VERTEX_INDEXED, c"vertexIndexed"),
            QuadMode::List => (VERTEX_LIST, c"vertexList"),
            QuadMode::Instanced => (VERTEX_INSTANCED, c"vertexInstanced"),
        };
        let vertex = spirv_words(vertex_spirv).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT).ok_or_else(invalid)?;
        let formats = [color_format];
        let pipeline = |blend| {
            Pipeline::graphics(
                device,
                bindless.pipeline_layout(),
                &GraphicsPipelineDesc {
                    color_formats: &formats,
                    cull_mode: vk::CullModeFlags::NONE,
                    depth_test: false,
                    depth_write: false,
                    blend,
                    ..GraphicsPipelineDesc::new(
                        ShaderStageDesc {
                            spirv: &vertex,
                            entry: vertex_entry,
                        },
                        Some(ShaderStageDesc {
                            spirv: &fragment,
                            entry: c"fragmentMain",
                        }),
                    )
                },
                vk::PipelineCache::null(),
            )
        };
        let pipelines = [pipeline(Blend::Premultiplied)?, pipeline(Blend::Multiply)?];
        debug_assert_eq!(PIPELINE_MULTIPLY, 1);

        let (index_buffer, index_allocation) = if mode == QuadMode::Indexed {
            let indices = QUADS_PER_DRAW as usize * 6;
            let (buffer, mut allocation) = allocator.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size((indices * 2) as u64)
                    .usage(vk::BufferUsageFlags::INDEX_BUFFER),
                MemoryLocation::Dynamic,
            )?;
            // Written once by the host before any submission that reads it.
            if let Some(bytes) = allocation.mapped_slice_mut() {
                for (quad, chunk) in bytes.chunks_exact_mut(12).enumerate() {
                    let base = (quad * 4) as u16;
                    for (slot, corner) in chunk.chunks_exact_mut(2).zip([0, 1, 2, 2, 1, 3]) {
                        slot.copy_from_slice(&(base + corner).to_le_bytes());
                    }
                }
            }
            (buffer, Some(allocation))
        } else {
            (vk::Buffer::null(), None)
        };
        Ok(Self {
            pipelines,
            mode,
            index_buffer,
            index_allocation,
        })
    }

    /// The vertex generation mode.
    pub fn mode(&self) -> QuadMode {
        self.mode
    }

    /// Declares a pass named "2d" that draws `canvas` into `target`, cleared first when
    /// `clear` is set. The canvas must be prepared ([`Canvas::prepare`]); its instances are
    /// copied into `ring` in submission order, plus the draw order (4 bytes per quad) when
    /// sorting moved anything. The frame must execute with `bindless`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        canvas: &'g Canvas,
        view: &View2d,
        target: Texture,
        clear: Option<[f32; 4]>,
    ) -> Result<(), Render2dError> {
        engine_profiler::zone!("render2d::add_pass");
        if !canvas.is_prepared() {
            return Err(Render2dError::NotPrepared);
        }
        let count = canvas.len();
        let needed = (count * (core::mem::size_of::<Instance>() + 4)) as u64;
        let instances = ring
            .stage(canvas.instances())
            .ok_or(Render2dError::RingFull { needed })?;
        // Quads already in draw order are read directly; otherwise through the order.
        let mut view_flags = if view.y_down { VIEW_Y_DOWN } else { 0 };
        let mut order_address = 0;
        if !canvas.stats().presorted {
            let (order, staged) = ring
                .alloc::<u32>(count)
                .ok_or(Render2dError::RingFull { needed })?;
            canvas.write_order(order);
            order_address = staged.address;
            view_flags |= VIEW_INDIRECT;
        }
        let push = Push {
            clip_from_world: view.clip_from_world.to_cols_array(),
            instances: instances.address,
            order: order_address,
            aa_expand: view.units_per_pixel,
            view_flags,
        };
        let load = clear.map_or(LoadOp::Load, LoadOp::Clear);
        let (pipelines, mode, index_buffer) = (&self.pipelines, self.mode, self.index_buffer);
        fg.add_pass("2d")
            .color(target, load, StoreOp::Store)
            .execute(move |ctx| {
                if canvas.is_empty() {
                    return;
                }
                let area = ctx.render_area();
                let device = ctx.device;
                let cmd = ctx.cmd;
                // SAFETY: `Push` is plain `repr(C)` data without padding.
                let push_bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&raw const push).cast::<u8>(),
                        core::mem::size_of::<Push>(),
                    )
                };
                // SAFETY: recording inside the pass's rendering scope; the instances are
                // in the ring slot of this frame; the index buffer lives as long as the
                // renderer ('g).
                unsafe {
                    bindless.bind(cmd, vk::PipelineBindPoint::GRAPHICS);
                    bindless.push_constants(cmd, push_bytes);
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
                    if mode == QuadMode::Indexed {
                        device.cmd_bind_index_buffer(cmd, index_buffer, 0, vk::IndexType::UINT16);
                    }
                    let (mut bound_pipeline, mut bound_clip) = (u16::MAX, usize::MAX);
                    for batch in canvas.batches() {
                        if batch.pipeline() != bound_pipeline {
                            bound_pipeline = batch.pipeline();
                            pipelines[usize::from(bound_pipeline)].bind(cmd);
                        }
                        if batch.clip() != bound_clip {
                            bound_clip = batch.clip();
                            let scissor = canvas
                                .clip_rect(bound_clip)
                                .map_or(area, |clip| intersect(area, clip));
                            device.cmd_set_scissor(cmd, 0, &[scissor]);
                        }
                        match mode {
                            QuadMode::Indexed => {
                                let mut first = batch.first;
                                let end = batch.first + batch.count;
                                while first < end {
                                    let n = (end - first).min(QUADS_PER_DRAW);
                                    device.cmd_draw_indexed(
                                        cmd,
                                        n * 6,
                                        1,
                                        0,
                                        (first * 4) as i32,
                                        0,
                                    );
                                    first += n;
                                }
                            }
                            QuadMode::List => {
                                device.cmd_draw(cmd, batch.count * 6, 1, batch.first * 6, 0);
                            }
                            QuadMode::Instanced => {
                                device.cmd_draw(cmd, 6, batch.count, 0, batch.first);
                            }
                        }
                    }
                }
            });
        Ok(())
    }

    /// Frees the index buffer (pipelines are freed on drop).
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the renderer.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.index_allocation.take() {
            // SAFETY: guaranteed by the caller.
            unsafe { allocator.destroy_buffer(self.index_buffer, allocation) };
        }
    }
}

/// `clip` limited to `area`; empty when they do not overlap.
fn intersect(area: vk::Rect2D, clip: crate::ClipRect) -> vk::Rect2D {
    let x0 = clip.x.max(area.offset.x);
    let y0 = clip.y.max(area.offset.y);
    let x1 = (i64::from(clip.x) + i64::from(clip.width))
        .min(i64::from(area.offset.x) + i64::from(area.extent.width));
    let y1 = (i64::from(clip.y) + i64::from(clip.height))
        .min(i64::from(area.offset.y) + i64::from(area.extent.height));
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: (x1 - i64::from(x0)).max(0) as u32,
            height: (y1 - i64::from(y0)).max(0) as u32,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_intersection() {
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 100,
                height: 50,
            },
        };
        let clip = |x, y, width, height| crate::ClipRect {
            x,
            y,
            width,
            height,
        };
        let r = intersect(area, clip(-10, 10, 50, 100));
        assert_eq!(
            (r.offset.x, r.offset.y, r.extent.width, r.extent.height),
            (0, 10, 40, 40)
        );
        let outside = intersect(area, clip(200, 0, 10, 10));
        assert_eq!(outside.extent.width, 0);
    }
}
