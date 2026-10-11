//! Drawing a [`Canvas`] into a render graph target.

use crate::canvas::{Canvas, Instance, PIPELINE_MULTIPLY};
use crate::view::View2d;
use engine_render::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture};
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
const FRAGMENT_DEPTH_TESTED: &[u8] = include_bytes!("../shaders/sprite.fragmentDepthTested.spv");

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

/// `Push` in `shaders/sprite.slang` (112 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
struct Push {
    clip_from_world: [f32; 16],
    instances: vk::DeviceAddress,
    order: vk::DeviceAddress,
    aa_expand: f32,
    view_flags: u32,
    white: f32,
    depth_texture: u32,
    depth_scale: [f32; 2],
    depth_bias: f32,
    _pad: u32,
}

const _: () = assert!(core::mem::size_of::<Push>() == 112);

impl Push {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `Push` is plain `repr(C)` data without padding (asserted size).
        unsafe {
            core::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }
}
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

/// The scene's depth a canvas placed in the scene is tested against
/// ([`Renderer2d::add_depth_tested_pass`], ADR 0091).
#[derive(Debug, Clone, Copy)]
pub struct SceneDepth {
    /// The depth, 0 at the near plane and 1 at the far plane (sampled).
    pub texture: Texture,
    /// The depth texture's size over the target's, per axis (the render size over the
    /// output's).
    pub scale: [f32; 2],
    /// A fragment is hidden when it is deeper than the scene by more than this (in depth
    /// units: a little, so that a panel flush with a wall stays visible).
    pub bias: f32,
}

/// Draws canvases into one color format.
pub struct Renderer2d {
    /// Indexed by the batch pipeline selector.
    pipelines: [Pipeline; 2],
    /// The same, testing against the scene's depth ([`Renderer2d::with_depth_test`]).
    depth_tested: Option<[Pipeline; 2]>,
    mode: QuadMode,
    index_buffer: vk::Buffer,
    index_allocation: Option<Allocation>,
    white: f32,
}

impl core::fmt::Debug for Renderer2d {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Renderer2d")
            .field("mode", &self.mode)
            .field("white", &self.white)
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
        Self::create(device, allocator, bindless, color_format, mode, false)
    }

    /// [`Self::new`], also with the pipelines that test against the scene's depth, for
    /// canvases placed in the scene ([`Self::add_depth_tested_pass`], ADR 0091). Their own
    /// fragment entry point: the others pay nothing for it.
    pub fn with_depth_test(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        color_format: vk::Format,
        mode: QuadMode,
    ) -> Result<Self, RhiError> {
        Self::create(device, allocator, bindless, color_format, mode, true)
    }

    fn create(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        color_format: vk::Format,
        mode: QuadMode,
        depth_test: bool,
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
        let pipeline = |blend, fragment: &[u32], entry: &core::ffi::CStr| {
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
                            spirv: fragment,
                            entry,
                        }),
                    )
                },
            )
        };
        let pipelines = [
            pipeline(Blend::Premultiplied, &fragment, c"fragmentMain")?,
            pipeline(Blend::Multiply, &fragment, c"fragmentMain")?,
        ];
        debug_assert_eq!(PIPELINE_MULTIPLY, 1);
        let depth_tested = if depth_test {
            let fragment = spirv_words(FRAGMENT_DEPTH_TESTED).ok_or_else(invalid)?;
            Some([
                pipeline(Blend::Premultiplied, &fragment, c"fragmentDepthTested")?,
                pipeline(Blend::Multiply, &fragment, c"fragmentDepthTested")?,
            ])
        } else {
            None
        };

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
            depth_tested,
            mode,
            index_buffer,
            index_allocation,
            white: 1.0,
        })
    }

    /// The vertex generation mode.
    pub fn mode(&self) -> QuadMode {
        self.mode
    }

    /// The value white (1.0) is written as in later passes: 1 for SDR targets and HDR
    /// intermediates (the default), SDR white in scRGB units (nits / 80) for an scRGB
    /// swapchain (ADR 0038). Scales colors, textures and the clear color; multiplied quads
    /// are factors and stay as they are.
    pub fn set_white(&mut self, white: f32) {
        self.white = white;
    }

    /// See [`Self::set_white`].
    pub fn white(&self) -> f32 {
        self.white
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
        self.declare(fg, ring, bindless, canvas, view, target, clear, None)
    }

    /// [`Self::add_pass`] for a canvas placed in the scene (a panel in the world, a
    /// helmet's visor): `view` is the scene camera's projection times the canvas's place,
    /// and each fragment the scene drew something in front of is hidden. Needs
    /// [`Self::with_depth_test`] (else it draws untested).
    #[allow(clippy::too_many_arguments)]
    pub fn add_depth_tested_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        canvas: &'g Canvas,
        view: &View2d,
        target: Texture,
        depth: SceneDepth,
    ) -> Result<(), Render2dError> {
        self.declare(fg, ring, bindless, canvas, view, target, None, Some(depth))
    }

    #[allow(clippy::too_many_arguments)]
    fn declare<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        canvas: &'g Canvas,
        view: &View2d,
        target: Texture,
        clear: Option<[f32; 4]>,
        depth: Option<SceneDepth>,
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
        let white = self.white;
        let mut push = Push {
            clip_from_world: view.clip_from_world.to_cols_array(),
            instances: instances.address,
            order: order_address,
            aa_expand: view.units_per_pixel,
            view_flags,
            white,
            depth_texture: 0,
            depth_scale: depth.map_or([1.0; 2], |d| d.scale),
            depth_bias: depth.map_or(0.0, |d| d.bias),
            _pad: 0,
        };
        let load = clear.map_or(LoadOp::Load, |[r, g, b, a]| {
            LoadOp::Clear([r * white, g * white, b * white, a])
        });
        let pipelines = match (depth, &self.depth_tested) {
            (Some(_), Some(tested)) => tested,
            _ => &self.pipelines,
        };
        let (mode, index_buffer) = (self.mode, self.index_buffer);
        let mut pass = fg.add_pass(if depth.is_some() { "2d in scene" } else { "2d" });
        if let Some(depth) = depth {
            pass = pass.read(depth.texture, Access::Sampled(ShaderStages::FRAGMENT));
        }
        pass.color(target, load, StoreOp::Store)
            .execute(move |ctx| {
                if canvas.is_empty() {
                    return;
                }
                if let Some(depth) = depth {
                    push.depth_texture = ctx.sampled_index(depth.texture).unwrap_or(0);
                }
                // Multiplied quads scale what is below them: factors, not light.
                let multiply_push = Push { white: 1.0, ..push };
                let area = ctx.render_area();
                let device = ctx.device;
                let cmd = ctx.cmd;
                // SAFETY: recording inside the pass's rendering scope; the instances are
                // in the ring slot of this frame; the index buffer lives as long as the
                // renderer ('g).
                unsafe {
                    bindless.bind(cmd, vk::PipelineBindPoint::GRAPHICS);
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
                            let constants = if bound_pipeline == PIPELINE_MULTIPLY {
                                &multiply_push
                            } else {
                                &push
                            };
                            bindless.push_constants(cmd, constants.bytes());
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
