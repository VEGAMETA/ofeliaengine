//! Texture feedback (ADR 0076): which texture levels the GPU actually samples, measured
//! by drawing the view's visible instances again into a small target.
//!
//! - **What is measured.** For each material slot, the finest level a texture of that
//!   material is sampled at, in units of texture coordinates (a texture of size `S` is
//!   sampled at that plus `log2 S`), and how many feedback pixels the material covers.
//!   Every texture of a material uses the mesh's coordinates (ADR 0040), so one value per
//!   material serves all its textures; a texture shared by several materials takes the
//!   finest of theirs.
//! - **How.** After the view's forward passes, [`TextureFeedback::add_pass`] draws the
//!   same culled commands (the culling pass's visible lists, early and late phases, with
//!   their detail levels) into a depth target `divisor` times smaller on each side, with
//!   `depthVertexMasked` and `feedbackFragment` (`forward.slang`). The fragment shader
//!   computes the level from the texture coordinates' derivatives as the sampler does
//!   (16× anisotropic: the shorter axis, at most 16 times finer than the longer), adds
//!   the sampler's LOD bias, takes away `log2 divisor` (a pixel here spans that many of the
//!   view's), and keeps the minimum per material with one atomic per wave. Opaque surfaces
//!   are drawn first with depth writes, transparent ones after without, so hidden surfaces
//!   mostly fail the early depth test and request nothing.
//! - **Cost.** A separate pass, so the view's shading pipelines carry no code for it (the
//!   register rule, CLAUDE.md §5): views that do not stream pay nothing. Its own cost is
//!   the vertex work of the visible instances at a small fraction of the pixels; it can run
//!   every `interval` frames.
//! - **Results** reach the CPU through a readback ring, `frames_in_flight` frames later
//!   ([`TextureFeedback::poll`]), and feed the texture streamer
//!   (`engine_render::streaming`).
//!
//! Sampler feedback (D3D12) records the same thing in hardware, per texture region; there
//! is no Vulkan equivalent, and this pass uses no part of it (see the research note on
//! patents).

use crate::forward::{BLENDED_STATES, DEPTH_STATES, DEPTH_VERTEX_MASKED, ForwardPasses, Push};
use crate::graph::{
    Access, BufferState, FrameGraph, ImportedBuffer, LoadOp, ShaderStages, StoreOp, TextureDesc,
};
use crate::material::MaterialFrame;
use crate::mesh::GeometryFrame;
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Device, GraphicsPipelineDesc, MemoryLocation, Pipeline,
    PipelineDesc, RhiError, ShaderStageDesc, spirv_words,
};

const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.feedbackFragment.spv");

/// `FEEDBACK_SCALE` and `FEEDBACK_ZERO` in `forward.slang`.
const SCALE: f32 = 256.0;
const ZERO: f32 = 64.0;

/// The feedback target's depth format.
const DEPTH_FORMAT: vk::Format = vk::Format::D16_UNORM;

/// How the feedback pass runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackSettings {
    /// The feedback target is the view's size over this, on each side (at least 1).
    pub divisor: u32,
    /// The pass runs every this many frames (at least 1).
    pub interval: u32,
    /// Each pass draws one of this many slices of the view's draw commands (every
    /// `slices`-th, a different one each time): the cost spread over that many passes, a
    /// material reported at least once per cycle (at least 1).
    pub slices: u32,
}

impl Default for FeedbackSettings {
    /// An eighth of the view's size on each side (240 × 135 at 1080p), every frame, a
    /// quarter of the commands per pass (measured on Sponza: 48 µs a frame on the Radeon
    /// iGPU instead of 204 µs for all of them, the same memory resident).
    fn default() -> Self {
        Self {
            divisor: 8,
            interval: 1,
            slices: 4,
        }
    }
}

/// What one material slot asked for in a feedback frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaterialFeedback {
    /// The material's slot in the table.
    pub slot: u32,
    /// The finest level sampled, in texture-coordinate units: a texture of `size` texels
    /// (its longer side) is sampled at level `level + log2(size)`.
    pub level: f32,
    /// Feedback pixels that showed it.
    pub pixels: u32,
}

/// A buffer of the feedback's: device-local for the pass, host-cached for readbacks.
#[derive(Debug)]
struct FeedbackBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    /// In material slots.
    capacity: u32,
}

impl FeedbackBuffer {
    fn new(
        device: &Device,
        allocator: &mut Allocator,
        capacity: u32,
        location: MemoryLocation,
    ) -> Result<Self, RhiError> {
        let usage = if location == MemoryLocation::Device {
            vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                | vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::TRANSFER_DST
        } else {
            vk::BufferUsageFlags::TRANSFER_DST
        };
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(u64::from(capacity) * 8)
                .usage(usage),
            location,
        )?;
        let address = if location == MemoryLocation::Device {
            // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
            unsafe {
                device.raw().get_buffer_device_address(
                    &vk::BufferDeviceAddressInfo::default().buffer(buffer),
                )
            }
        } else {
            0
        };
        Ok(Self {
            buffer,
            allocation: Some(allocation),
            address,
            capacity,
        })
    }

    /// # Safety
    ///
    /// The GPU no longer uses it.
    unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// A readback of the ring: the frame that wrote it, and how many slots.
#[derive(Debug)]
struct Readback {
    buffer: FeedbackBuffer,
    written: Option<(u64, u32)>,
}

/// One view's texture feedback (see the module documentation).
#[derive(Debug)]
pub struct TextureFeedback {
    settings: FeedbackSettings,
    /// Depth-writing (opaque) and not (transparent) pipelines.
    pipelines: [Pipeline; 2],
    target: Option<FeedbackBuffer>,
    /// Whether the target holds cleared values (it is cleared after each readback).
    cleared: bool,
    passes: u64,
    readbacks: Vec<Readback>,
    retired: Vec<(u64, FeedbackBuffer)>,
    frames_in_flight: u64,
    /// The latest results: per slot, the fixed-point minimum and the inverted count.
    latest: Vec<[u32; 2]>,
    latest_frame: Option<u64>,
    frames: u64,
}

impl TextureFeedback {
    /// A feedback pass for one view; `frames_in_flight` sizes the readback ring.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        settings: FeedbackSettings,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(DEPTH_VERTEX_MASKED).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT).ok_or_else(invalid)?;
        let desc = |depth_write| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                depth_format: DEPTH_FORMAT,
                dynamic_culling: true,
                depth_write,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"depthVertexMasked",
                    },
                    Some(ShaderStageDesc {
                        spirv: &fragment,
                        entry: c"feedbackFragment",
                    }),
                )
            })
        };
        let mut pipelines = Pipeline::create_many(
            device,
            bindless.pipeline_layout(),
            &[desc(true), desc(false)],
        )?
        .into_iter();
        let (Some(opaque), Some(blended)) = (pipelines.next(), pipelines.next()) else {
            return Err(RhiError::Vulkan {
                call: "feedback pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            });
        };
        let frames_in_flight = frames_in_flight.max(1) as u64;
        Ok(Self {
            settings: FeedbackSettings {
                divisor: settings.divisor.max(1),
                interval: settings.interval.max(1),
                slices: settings.slices.max(1),
            },
            pipelines: [opaque, blended],
            target: None,
            cleared: false,
            passes: 0,
            readbacks: Vec::new(),
            retired: Vec::new(),
            frames_in_flight,
            latest: Vec::new(),
            latest_frame: None,
            frames: 0,
        })
    }

    /// The settings.
    pub fn settings(&self) -> FeedbackSettings {
        self.settings
    }

    /// The driver's statistics of the feedback pipeline (registers, occupancy), when the
    /// device captures them.
    pub fn statistics(&self, device: &Device) -> Vec<engine_rhi::PipelineStatistic> {
        self.pipelines[0].statistics(device)
    }

    /// Takes the results of feedback passes whose frames have finished (call each frame
    /// before [`Self::add_pass`], with the same frame numbers). Returns the frame the
    /// newest results come from, if any arrived since the last call; read them with
    /// [`Self::results`].
    pub fn poll(&mut self, frame_number: u64) -> Option<u64> {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut newest = None;
        for r in &mut self.readbacks {
            let Some((written, count)) = r.written else {
                continue;
            };
            if written > completed || newest.is_some_and(|(f, _)| f >= written) {
                continue;
            }
            newest = Some((written, count));
        }
        let (written, count) = newest?;
        if self.latest_frame.is_some_and(|f| f >= written) {
            return None;
        }
        let r = self
            .readbacks
            .iter_mut()
            .find(|r| r.written == Some((written, count)))?;
        let ptr = r
            .buffer
            .allocation
            .as_ref()
            .and_then(Allocation::mapped_ptr)?;
        self.latest.clear();
        // SAFETY: the frame that wrote it has finished (its graph made the copy visible to
        // the host); the mapping holds `capacity ≥ count` pairs.
        let pairs =
            unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast::<[u32; 2]>(), count as usize) };
        self.latest.extend_from_slice(pairs);
        self.latest_frame = Some(written);
        r.written = None;
        Some(written)
    }

    /// The latest results: every material slot that showed in the feedback target.
    pub fn results(&self) -> impl Iterator<Item = MaterialFeedback> + '_ {
        self.latest
            .iter()
            .enumerate()
            .filter(|(_, p)| p[0] != u32::MAX)
            .map(|(slot, p)| MaterialFeedback {
                slot: slot as u32,
                level: p[0] as f32 / SCALE - ZERO,
                pixels: !p[1],
            })
    }

    /// The frame of [`Self::results`].
    pub fn results_frame(&self) -> Option<u64> {
        self.latest_frame
    }

    /// Declares the feedback pass for a view drawn this frame (`passes`, from its
    /// [`crate::forward::ForwardRenderer::add_passes`]) of `width × height` pixels, whose
    /// materials sample with `lod_bias` ([`crate::material::MaterialTable::texture_lod_bias`]).
    /// Does nothing on frames between [`FeedbackSettings::interval`]s or when the view drew
    /// nothing. Allocates only when the material table grows.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        passes: &ForwardPasses,
        (width, height): (u32, u32),
        lod_bias: f32,
        frame_number: u64,
    ) -> Result<(), RhiError> {
        engine_profiler::zone!("render::TextureFeedback::add_pass");
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, mut buffer) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it have finished.
            unsafe { buffer.destroy(allocator) };
        }
        self.frames += 1;
        let Some(draws) = passes.draws else {
            return Ok(());
        };
        if !(self.frames - 1).is_multiple_of(u64::from(self.settings.interval)) {
            return Ok(());
        }
        // A readback this frame may write: one not holding unread results (the ring has
        // one more than the frames in flight, so with `poll` every frame there is one).
        let ring = self.frames_in_flight as usize + 1;
        let slots = materials.capacity().max(1);
        if self.target.as_ref().is_none_or(|t| t.capacity < slots) {
            let capacity = slots.next_power_of_two().max(64);
            let target = FeedbackBuffer::new(device, allocator, capacity, MemoryLocation::Device)?;
            if let Some(old) = self.target.replace(target) {
                self.retired.push((frame_number, old));
            }
            self.cleared = false;
            for r in self.readbacks.drain(..) {
                self.retired.push((frame_number, r.buffer));
            }
            for _ in 0..ring {
                self.readbacks.push(Readback {
                    buffer: FeedbackBuffer::new(
                        device,
                        allocator,
                        capacity,
                        MemoryLocation::Readback,
                    )?,
                    written: None,
                });
            }
        }
        let Some(readback) = self.readbacks.iter_mut().find(|r| r.written.is_none()) else {
            // Results nobody polled: skip a feedback frame rather than overwrite them.
            return Ok(());
        };
        readback.written = Some((frame_number, slots));
        let Some(target) = self.target.as_ref() else {
            unreachable!("made above");
        };
        let bytes = u64::from(slots) * 8;
        let feedback = fg.import_buffer(
            "texture feedback",
            ImportedBuffer {
                buffer: target.buffer,
                size: u64::from(target.capacity) * 8,
                address: target.address,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        let out = fg.import_buffer(
            "texture feedback readback",
            ImportedBuffer {
                buffer: readback.buffer.buffer,
                size: bytes,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        let capacity_bytes = u64::from(target.capacity) * 8;
        let clear = move |fg: &mut FrameGraph<'g>| {
            fg.add_pass("texture feedback clear")
                .write(feedback, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; within the buffer.
                    unsafe {
                        ctx.device.cmd_fill_buffer(
                            ctx.cmd,
                            ctx.buffer(feedback),
                            0,
                            capacity_bytes,
                            u32::MAX,
                        );
                    }
                });
        };
        // A new buffer is cleared first; after that, each readback clears it for the next
        // pass (at the end of the frame, where the clear waits for nothing).
        if !self.cleared {
            clear(fg);
        }
        self.cleared = true;
        let slices = self.settings.slices;
        let slice = (self.passes % u64::from(slices)) as u32;
        self.passes += 1;
        let divisor = self.settings.divisor;
        let depth = fg.create_texture(
            "texture feedback depth",
            TextureDesc::new_2d(
                DEPTH_FORMAT,
                width.div_ceil(divisor).max(1),
                height.div_ceil(divisor).max(1),
            ),
        );
        let offset = lod_bias - (divisor as f32).log2();
        let pipelines = &self.pipelines;
        let geometry_copy = *geometry;
        geometry
            .read(fg.add_pass("texture feedback"))
            .read(draws.commands, Access::Indirect)
            .read(draws.visible, Access::StorageRead(ShaderStages::VERTEX))
            .maybe_read(draws.instances, Access::StorageRead(ShaderStages::VERTEX))
            .read(feedback, Access::StorageReadWrite(ShaderStages::FRAGMENT))
            .depth(
                depth,
                LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                StoreOp::DontCare,
            )
            .execute(move |ctx| {
                let push = Push {
                    visible: ctx.address(draws.visible),
                    light_tiles: ctx.address(feedback),
                    texture: offset.to_bits(),
                    ..draws.push
                };
                // Opaque (and subsurface) surfaces with depth writes, of every phase;
                // transparent ones after, tested but not written.
                for (pipeline, mask) in [
                    (&pipelines[0], DEPTH_STATES),
                    (&pipelines[1], BLENDED_STATES),
                ] {
                    for phase in 0..draws.phases {
                        // SAFETY: recording inside the pass's rendering scope; the pass
                        // reads the pool and the material table.
                        unsafe {
                            draws.states.draw_slice(
                                ctx,
                                bindless,
                                &geometry_copy,
                                [pipeline, pipeline],
                                // Feedback reads no material code (ADR 0092).
                                &crate::forward::TypedPipelines::BUILTIN,
                                &push,
                                false,
                                mask,
                                ctx.buffer(draws.commands),
                                u64::from(phase) * draws.command_bytes,
                                ctx.render_area(),
                                (slice, slices),
                            );
                        }
                    }
                }
            });
        fg.add_pass("texture feedback readback")
            .read(feedback, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope; within both buffers.
                unsafe {
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        ctx.buffer(feedback),
                        ctx.buffer(out),
                        &[vk::BufferCopy {
                            src_offset: 0,
                            dst_offset: 0,
                            size: bytes,
                        }],
                    );
                }
            });
        clear(fg);
        Ok(())
    }

    /// Frees the buffers (the pipelines go when it is dropped).
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if let Some(mut target) = self.target.take() {
                target.destroy(allocator);
            }
            for mut r in self.readbacks.drain(..) {
                r.buffer.destroy(allocator);
            }
            for (_, mut buffer) in self.retired.drain(..) {
                buffer.destroy(allocator);
            }
        }
    }
}
