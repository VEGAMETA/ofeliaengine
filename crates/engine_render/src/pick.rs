//! Picking in a view (ADR 0109): which instance is under a pixel, for the editor.
//!
//! - **How.** After the view's forward passes, [`Picker::add_pass`] draws the same culled
//!   commands (the culling pass's visible lists, early and late phases, with their detail
//!   levels) again into a [`SQUARE`] × [`SQUARE`] square centred on the pixel, at the
//!   view's size: the viewport shifts the view's pixel grid onto the square, so only it is
//!   rasterized. Each pixel keeps the nearest surface's instance over a depth of its own
//!   (`pickVertex`, `pickFragment` in `forward.slang`); masked materials are alpha-tested
//!   (`pickVertexMasked`, `pickFragmentMasked`), and blended surfaces are drawn too, with
//!   depth writes, unless the pick leaves them out.
//! - **What an id is:** the index the view's vertex shaders read the instance with: the
//!   persistent scene's slot ([`crate::gpu_scene::GpuScene`]), or a draw list's item.
//! - **Cost.** A separate pass, so the view's pipelines carry no code for it (the register
//!   rule, CLAUDE.md §5): views nobody picks in pay nothing. Its own cost is the visible
//!   instances' vertex work, as texture feedback's ([`crate::feedback`]).
//! - **Results** reach the CPU through a readback ring, `frames_in_flight` frames later
//!   ([`Picker::poll`]).

use crate::forward::{BLENDED_STATES, DEPTH_STATES, DrawArea, ForwardPasses, Push, TypedPipelines};
use crate::graph::{Access, BufferState, FrameGraph, ImportedBuffer, LoadOp, StoreOp, TextureDesc};
use crate::material::MaterialFrame;
use crate::mesh::GeometryFrame;
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Device, GraphicsPipelineDesc, MemoryLocation, Pipeline,
    PipelineDesc, RhiError, ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.pickVertex.spv");
const VERTEX_MASKED: &[u8] = include_bytes!("../shaders/forward.pickVertexMasked.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.pickFragment.spv");
const FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.pickFragmentMasked.spv");

/// The side of the square a pick reads around its pixel, in the view's pixels.
pub const SQUARE: u32 = 32;
/// Where the pick's pixel is in the square.
const CENTER: u32 = SQUARE / 2;
const ID_FORMAT: vk::Format = vk::Format::R32_UINT;
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
/// Bytes of a square's ids.
const BYTES: u64 = (SQUARE * SQUARE * 4) as u64;

/// A pick: a pixel of the view, at the view's size (its render size under upscaling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickRequest {
    /// The pixel's column, from the view's left.
    pub x: u32,
    /// Its row, from the view's top.
    pub y: u32,
    /// Blended surfaces can be picked: they hide what is behind them, as opaque ones do.
    pub transparent: bool,
}

/// What a pick saw: the ids of the square around its pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickResult {
    /// The frame that drew it.
    pub frame: u64,
    /// What was asked.
    pub request: PickRequest,
    /// [`SQUARE`]² ids by rows from the top: an instance's index plus one, 0 where none
    /// was. The pick's pixel is at ([`SQUARE`] / 2, [`SQUARE`] / 2).
    pub ids: Vec<u32>,
}

impl PickResult {
    /// The instance at the pick's pixel, or the nearest one within `radius` pixels of it
    /// (the first in reading order among equally near ones): thin objects stay easy to hit.
    pub fn nearest(&self, radius: u32) -> Option<u32> {
        let r = radius.min(CENTER - 1) as i32;
        let mut best: Option<(i32, u32)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                let d = dx * dx + dy * dy;
                if d > r * r || best.is_some_and(|(b, _)| d >= b) {
                    continue;
                }
                let (x, y) = (CENTER as i32 + dx, CENTER as i32 + dy);
                let id = self.ids[(y * SQUARE as i32 + x) as usize];
                if id != 0 {
                    best = Some((d, id - 1));
                }
            }
        }
        best.map(|(_, instance)| instance)
    }
}

/// What [`Picker::add_pass`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickPass {
    /// The pick is declared: its result comes with [`Picker::poll`].
    Declared,
    /// The view drew nothing, so nothing is under any pixel: nothing was declared.
    Empty,
    /// Every readback holds a pick not yet polled: nothing was declared.
    Busy,
}

/// A readback of the ring: its host buffer, and the pick it holds until polled.
#[derive(Debug)]
struct Readback {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    written: Option<(u64, PickRequest)>,
}

/// Picks in one view (see the module documentation).
#[derive(Debug)]
pub struct Picker {
    /// Opaque and masked: blended surfaces are drawn with them too.
    pipelines: [Pipeline; 2],
    readbacks: Vec<Readback>,
    frames_in_flight: u64,
    latest: Option<PickResult>,
}

impl Picker {
    /// A picker for one view; `frames_in_flight` sizes the readback ring.
    ///
    /// # Errors
    ///
    /// When a pipeline or a buffer cannot be made.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
        let (vertex, vertex_masked) = (words(VERTEX)?, words(VERTEX_MASKED)?);
        let (fragment, fragment_masked) = (words(FRAGMENT)?, words(FRAGMENT_MASKED)?);
        fn desc<'a>(
            (vertex, v): (&'a [u32], &'static core::ffi::CStr),
            (fragment, f): (&'a [u32], &'static core::ffi::CStr),
        ) -> PipelineDesc<'a> {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats: &[ID_FORMAT],
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
        let mut pipelines = Pipeline::create_many(
            device,
            bindless.pipeline_layout(),
            &[
                desc((&vertex, c"pickVertex"), (&fragment, c"pickFragment")),
                desc(
                    (&vertex_masked, c"pickVertexMasked"),
                    (&fragment_masked, c"pickFragmentMasked"),
                ),
            ],
        )?
        .into_iter();
        let (Some(opaque), Some(masked)) = (pipelines.next(), pipelines.next()) else {
            return Err(RhiError::Vulkan {
                call: "pick pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            });
        };
        let frames_in_flight = frames_in_flight.max(1) as u64;
        let mut readbacks = Vec::new();
        // One more than the frames in flight: with `poll` every frame, one is free.
        for _ in 0..=frames_in_flight {
            let (buffer, allocation) = allocator.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(BYTES)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )?;
            readbacks.push(Readback {
                buffer,
                allocation: Some(allocation),
                written: None,
            });
        }
        Ok(Self {
            pipelines: [opaque, masked],
            readbacks,
            frames_in_flight,
            latest: None,
        })
    }

    /// The driver's statistics of the pick pipelines (registers, occupancy), when the
    /// device captures them.
    pub fn statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 2] {
        self.pipelines.each_ref().map(|p| p.statistics(device))
    }

    /// Takes the newest pick whose frame has finished (call each frame before
    /// [`Self::add_pass`], with the same frame numbers): the latest result, when one
    /// arrived since the last call.
    pub fn poll(&mut self, frame_number: u64) -> Option<&PickResult> {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut newest: Option<(usize, u64, PickRequest)> = None;
        for (i, r) in self.readbacks.iter().enumerate() {
            if let Some((frame, request)) = r.written
                && frame <= completed
                && newest.is_none_or(|(_, f, _)| frame > f)
            {
                newest = Some((i, frame, request));
            }
        }
        // Older ones are done with: the newest replaces them.
        for r in &mut self.readbacks {
            if r.written.is_some_and(|(frame, _)| frame <= completed) {
                r.written = None;
            }
        }
        let (i, frame, request) = newest?;
        let ptr = self.readbacks[i]
            .allocation
            .as_ref()
            .and_then(Allocation::mapped_ptr)?;
        // SAFETY: the frame that wrote it has finished (its graph made the copy visible to
        // the host); the mapping holds a square of ids.
        let ids = unsafe {
            core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), (SQUARE * SQUARE) as usize)
        };
        let latest = self.latest.get_or_insert_with(|| PickResult {
            frame,
            request,
            ids: Vec::new(),
        });
        latest.frame = frame;
        latest.request = request;
        latest.ids.clear();
        latest.ids.extend_from_slice(ids);
        self.latest.as_ref()
    }

    /// The latest result [`Self::poll`] took.
    pub fn latest(&self) -> Option<&PickResult> {
        self.latest.as_ref()
    }

    /// Declares a pick of `request` in a view drawn this frame (`passes`, from its
    /// [`crate::forward::ForwardRenderer::add_passes`]) of `width × height` pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        passes: &ForwardPasses,
        (width, height): (u32, u32),
        request: PickRequest,
        frame_number: u64,
    ) -> PickPass {
        engine_profiler::zone!("render::Picker::add_pass");
        let Some(draws) = passes.draws else {
            return PickPass::Empty;
        };
        let Some(readback) = self.readbacks.iter_mut().find(|r| r.written.is_none()) else {
            return PickPass::Busy;
        };
        readback.written = Some((frame_number, request));
        let out = fg.import_buffer(
            "pick readback",
            ImportedBuffer {
                buffer: readback.buffer,
                size: BYTES,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        let ids = fg.create_texture("pick ids", TextureDesc::new_2d(ID_FORMAT, SQUARE, SQUARE));
        let depth = fg.create_texture(
            "pick depth",
            TextureDesc::new_2d(DEPTH_FORMAT, SQUARE, SQUARE),
        );
        // The view's pixel grid shifted so that the pick's pixel lands at the square's
        // center; only the square is rasterized.
        let area = DrawArea {
            viewport: vk::Rect2D {
                offset: vk::Offset2D {
                    x: CENTER as i32 - request.x as i32,
                    y: CENTER as i32 - request.y as i32,
                },
                extent: vk::Extent2D { width, height },
            },
            scissor: vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: SQUARE,
                    height: SQUARE,
                },
            },
        };
        let mask = if request.transparent {
            DEPTH_STATES | BLENDED_STATES
        } else {
            DEPTH_STATES
        };
        let pipelines = &self.pipelines;
        let geometry_copy = *geometry;
        materials
            .read(geometry.read(fg.add_pass("pick")))
            .read(draws.commands, Access::Indirect)
            .read(
                draws.visible,
                Access::StorageRead(crate::graph::ShaderStages::VERTEX),
            )
            .maybe_read(
                draws.instances,
                Access::StorageRead(crate::graph::ShaderStages::VERTEX),
            )
            .color(ids, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .depth(
                depth,
                LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                StoreOp::DontCare,
            )
            .execute(move |ctx| {
                let push = Push {
                    visible: ctx.address(draws.visible),
                    ..draws.push
                };
                let [opaque, masked] = pipelines;
                for phase in 0..draws.phases {
                    // SAFETY: recording inside the pass's rendering scope; the pass reads
                    // the pool, the material table and the view's lists.
                    unsafe {
                        draws.states.draw_slice(
                            ctx,
                            bindless,
                            &geometry_copy,
                            [opaque, masked],
                            // Picks run no material code of the types' own (ADR 0092).
                            &TypedPipelines::BUILTIN,
                            &push,
                            false,
                            mask,
                            ctx.buffer(draws.commands),
                            u64::from(phase) * draws.command_bytes,
                            area,
                            (0, 1),
                        );
                        // Instances drawn by meshlet (ADR 0085; none are blended).
                        if let Some(m) = draws.meshlets {
                            draws.states.draw_meshlets(
                                ctx,
                                bindless,
                                &geometry_copy,
                                [opaque, masked],
                                &push,
                                false,
                                mask,
                                ctx.buffer(draws.commands),
                                m.lists(phase),
                                &m,
                                area,
                            );
                        }
                    }
                }
            });
        fg.add_pass("pick copy")
            .read(ids, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope; the image is in
                // TRANSFER_SRC layout for this pass, the buffer holds a square.
                unsafe {
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(ids),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(out),
                        &[vk::BufferImageCopy {
                            image_subresource: vk::ImageSubresourceLayers {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                mip_level: 0,
                                base_array_layer: 0,
                                layer_count: 1,
                            },
                            image_extent: vk::Extent3D {
                                width: SQUARE,
                                height: SQUARE,
                                depth: 1,
                            },
                            ..Default::default()
                        }],
                    );
                }
            });
        PickPass::Declared
    }

    /// Frees the readback buffers (the pipelines go when it is dropped).
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        for r in &mut self.readbacks {
            if let Some(allocation) = r.allocation.take() {
                // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
                unsafe { allocator.destroy_buffer(r.buffer, allocation) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(at: &[(u32, u32, u32)]) -> PickResult {
        let mut ids = vec![0; (SQUARE * SQUARE) as usize];
        for &(x, y, id) in at {
            ids[(y * SQUARE + x) as usize] = id;
        }
        PickResult {
            frame: 1,
            request: PickRequest {
                x: 100,
                y: 100,
                transparent: true,
            },
            ids,
        }
    }

    #[test]
    fn the_nearest_id_within_the_radius_wins() {
        // Nothing at the center: the nearest within 3 pixels.
        let r = result(&[(CENTER + 2, CENTER, 8), (CENTER, CENTER + 1, 5)]);
        assert_eq!(r.nearest(3), Some(4), "one pixel away beats two");
        assert_eq!(r.nearest(0), None);
        let r = result(&[(CENTER, CENTER, 1), (CENTER + 1, CENTER, 9)]);
        assert_eq!(r.nearest(3), Some(0), "the pixel itself first");
        let r = result(&[(CENTER + 3, CENTER + 3, 2)]);
        assert_eq!(r.nearest(3), None, "outside the circle");
        assert_eq!(r.nearest(5), Some(1));
    }
}
