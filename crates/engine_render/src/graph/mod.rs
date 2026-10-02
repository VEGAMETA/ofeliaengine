//! Render graph (ADR 0011).
//!
//! Every frame the renderer declares its passes and the resources they use; the graph
//! then:
//!
//! - **culls** passes whose results nobody reads (imported resources and passes marked
//!   with side effects are the roots);
//! - **infers** image and buffer usage flags from the declared accesses;
//! - **aliases** transient resources whose lifetimes do not overlap in one memory heap per
//!   frame in flight, and keeps the Vulkan objects across frames while the layout repeats;
//! - **derives** every barrier (synchronization2), batched per pass, including layout
//!   transitions and the final transitions of imported resources (for example to
//!   `PRESENT_SRC`);
//! - **records** the passes with dynamic rendering set up from the declared attachments,
//!   each inside a debug label (RenderDoc, Nsight), and between GPU breadcrumbs
//!   (`engine_rhi::Breadcrumbs`, ADR 0061) that tell which pass a crashed GPU was in;
//! - **runs async compute** (ADR 0065): passes declared with
//!   [`PassBuilder::async_compute`] go to the device's async compute queue when
//!   [`RenderGraph::async_compute`] is on, scheduled as early as their inputs allow so the
//!   graphics work that does not lead to them overlaps them; timeline semaphores join the
//!   queues where results cross, with queue family ownership transfers or concurrent
//!   sharing.
//!
//! Pass callbacks live in a frame arena and all bookkeeping is in vectors reused across
//! frames: a steady-state frame makes no heap allocations.
//!
//! ```no_run
//! # use engine_render::graph::*;
//! # use engine_rhi::ash::vk;
//! # fn demo(graph: &mut RenderGraph, frame: &engine_rhi::Frame, allocator: &mut engine_rhi::Allocator,
//! #         swapchain: ImportedTexture) -> Result<(), engine_rhi::RhiError> {
//! let mut fg = graph.begin(frame);
//! let backbuffer = fg.import_texture("backbuffer", swapchain);
//! let hdr = fg.create_texture("hdr", TextureDesc::new_2d(vk::Format::R16G16B16A16_SFLOAT, 1920, 1080));
//! fg.add_pass("scene")
//!     .color(hdr, LoadOp::Clear([0.0; 4]), StoreOp::Store)
//!     .execute(|ctx| { /* draw */ });
//! fg.add_pass("tonemap")
//!     .read(hdr, Access::Sampled(ShaderStages::FRAGMENT))
//!     .color(backbuffer, LoadOp::DontCare, StoreOp::Store)
//!     .execute(|ctx| { /* fullscreen triangle */ });
//! fg.execute(allocator)?;
//! # Ok(()) }
//! ```

mod access;
mod compile;
mod physical;
mod placement;
mod schedule;
mod timing;

pub use access::{Access, BufferState, ShaderStages, TextureState};
pub use compile::QueueKind;
pub use timing::PassTime;

use access::AccessInfo;
use compile::{
    Boundary, BufferBarrier, ImageBarrier, Kind, MemoryBarrier, NO_SEGMENT, PassRecord, Records,
    ResourceRecord,
};
use engine_core::arena::Arena;
use engine_rhi::ash::{self, vk};
use engine_rhi::{Allocator, Bindless, Device, Queue, RhiError};
use physical::{Physical, PhysicalResources};
use schedule::ScheduleOptions;
use std::ptr::NonNull;

/// A texture in the current frame's graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Texture(u32);

/// A buffer in the current frame's graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Buffer(u32);

/// Size and format of a graph texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureDesc {
    /// Pixel format.
    pub format: vk::Format,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Mip levels.
    pub mip_levels: u32,
    /// Array layers.
    pub layers: u32,
    /// Samples per pixel.
    pub samples: vk::SampleCountFlags,
}

impl TextureDesc {
    /// A single-sampled 2D texture without mips.
    pub fn new_2d(format: vk::Format, width: u32, height: u32) -> Self {
        Self {
            format,
            width,
            height,
            mip_levels: 1,
            layers: 1,
            samples: vk::SampleCountFlags::TYPE_1,
        }
    }

    fn extent(&self) -> vk::Extent2D {
        vk::Extent2D {
            width: self.width,
            height: self.height,
        }
    }
}

/// Size of a graph buffer. Graph buffers always support buffer device address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferDesc {
    /// Size in bytes.
    pub size: u64,
}

/// A texture owned outside the graph (swapchain image, persistent history buffer).
#[derive(Debug, Clone, Copy)]
pub struct ImportedTexture {
    /// The image.
    pub image: vk::Image,
    /// A view of all its subresources.
    pub view: vk::ImageView,
    /// Its size and format.
    pub desc: TextureDesc,
    /// State before the graph's first use.
    pub initial: TextureState,
    /// State to leave it in, if the owner needs one (for example
    /// [`TextureState::present`]).
    pub final_state: Option<TextureState>,
    /// Its index in the bindless sampled-image binding, if its owner registered it.
    pub sampled_index: Option<u32>,
    /// Its index in the bindless storage-image binding, if its owner registered it.
    pub storage_index: Option<u32>,
}

/// A buffer owned outside the graph.
#[derive(Debug, Clone, Copy)]
pub struct ImportedBuffer {
    /// The buffer.
    pub buffer: vk::Buffer,
    /// Its size.
    pub size: u64,
    /// Its device address, or 0.
    pub address: vk::DeviceAddress,
    /// State before the graph's first use.
    pub initial: BufferState,
    /// State to leave it in, if the owner needs one.
    pub final_state: Option<BufferState>,
}

/// What happens to an attachment's contents when a pass starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoadOp {
    /// Keep what earlier passes wrote.
    Load,
    /// Clear to a color (color attachments) or to `[depth, stencil, _, _]` (depth).
    Clear([f32; 4]),
    /// Contents are undefined (the pass overwrites every pixel).
    DontCare,
}

/// Whether an attachment's contents are kept after a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    /// Keep them.
    Store,
    /// Discard them (for example a depth buffer nobody reads later).
    DontCare,
}

#[derive(Debug, Clone, Copy)]
struct AttachmentRecord {
    resource: u32,
    depth: bool,
    read_only: bool,
    load: LoadOp,
    store: StoreOp,
    /// The single-sampled texture it is resolved into at the end of the pass, and how.
    resolve: Option<(u32, vk::ResolveModeFlags)>,
}

/// How a multisampled attachment is resolved into a single-sampled one at the end of its
/// pass ([`PassBuilder::resolve`]; ADR 0075).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    /// The samples' average (color attachments of float and normalized formats).
    Average,
    /// Sample 0 (any format; depth always supports it).
    SampleZero,
    /// The smallest sample (depth: the nearest surface), where the device supports it.
    Min,
    /// The largest sample (depth: the farthest), where the device supports it.
    Max,
}

impl ResolveMode {
    fn flags(self) -> vk::ResolveModeFlags {
        match self {
            Self::Average => vk::ResolveModeFlags::AVERAGE,
            Self::SampleZero => vk::ResolveModeFlags::SAMPLE_ZERO,
            Self::Min => vk::ResolveModeFlags::MIN,
            Self::Max => vk::ResolveModeFlags::MAX,
        }
    }
}

/// Counts from the last executed frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphStats {
    /// Passes declared.
    pub passes: u32,
    /// Passes culled.
    pub culled_passes: u32,
    /// Image barriers recorded.
    pub image_barriers: u32,
    /// Global memory barriers recorded.
    pub memory_barriers: u32,
    /// Transient textures and buffers used.
    pub transient_resources: u32,
    /// Bytes reserved in this frame slot's transient heaps (with headroom).
    pub transient_heap_bytes: u64,
    /// Bytes the transient resources need with aliasing.
    pub transient_placed_bytes: u64,
    /// Bytes the transient resources would need without aliasing.
    pub transient_unaliased_bytes: u64,
    /// Vulkan images and buffers created this frame (0 in steady state).
    pub created_objects: u32,
    /// Passes that ran on the async compute queue.
    pub async_passes: u32,
    /// Batches the graph submitted itself (async compute: every segment but the last
    /// graphics one, which is the caller's command buffer).
    pub submits: u32,
    /// Queue family ownership transfers (each counted once, at its acquire).
    pub ownership_transfers: u32,
}

/// How transient resources used on both queues are shared between their families
/// (imported ones always move with ownership transfers: their owners created them for
/// one family).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncSharing {
    /// `VK_SHARING_MODE_CONCURRENT` for both families: no transfers, but drivers may
    /// turn off compression for such images.
    Concurrent,
    /// Exclusive to one family at a time, moved with release and acquire barriers.
    Exclusive,
}

/// Whether async compute is on by default on `device` (ADR 0065): the
/// `ENGINE_ASYNC_COMPUTE` variable (`1`/`on`, `0`/`off`) if set, else the default measured
/// for the GPU's vendor. Always `false` without an async compute queue.
pub fn async_compute_default(device: &Device) -> bool {
    if device.async_compute.is_none() {
        return false;
    }
    match std::env::var("ENGINE_ASYNC_COMPUTE").as_deref() {
        Ok("1" | "on" | "true") => true,
        Ok("0" | "off" | "false") => false,
        _ => vendor_default(device.adapter().vendor_id),
    }
}

/// The measured defaults (ADR 0065).
fn vendor_default(vendor_id: u32) -> bool {
    const NVIDIA: u32 = 0x10de;
    vendor_id == NVIDIA
}

/// The sharing of transients across queues: `ENGINE_ASYNC_SHARING` (`concurrent` or
/// `exclusive`) if set, else exclusive (ADR 0065: the same speed on the RTX 4060 Ti, and
/// drivers keep compressing exclusive images).
fn sharing_default() -> AsyncSharing {
    match std::env::var("ENGINE_ASYNC_SHARING").as_deref() {
        Ok("concurrent") => AsyncSharing::Concurrent,
        _ => AsyncSharing::Exclusive,
    }
}

type Callback = dyn FnMut(&mut PassContext<'_>);

/// What a pass callback gets.
pub struct PassContext<'x> {
    /// The command buffer being recorded (inside `vkCmdBeginRendering` for passes with
    /// attachments).
    pub cmd: vk::CommandBuffer,
    /// The device, for recording commands.
    pub device: &'x ash::Device,
    physical: &'x [Physical],
    render_area: vk::Rect2D,
}

impl PassContext<'_> {
    /// The Vulkan image of a graph texture.
    pub fn image(&self, texture: Texture) -> vk::Image {
        self.physical[texture.0 as usize].image
    }

    /// A view of all subresources of a graph texture (null for textures used only for
    /// transfers, which cannot have views).
    pub fn view(&self, texture: Texture) -> vk::ImageView {
        self.physical[texture.0 as usize].view
    }

    /// The Vulkan buffer of a graph buffer.
    pub fn buffer(&self, buffer: Buffer) -> vk::Buffer {
        self.physical[buffer.0 as usize].buffer
    }

    /// The device address of a graph buffer.
    pub fn address(&self, buffer: Buffer) -> vk::DeviceAddress {
        self.physical[buffer.0 as usize].address
    }

    /// Width and height of a graph texture.
    pub fn extent(&self, texture: Texture) -> vk::Extent2D {
        self.physical[texture.0 as usize].extent
    }

    /// Index of a graph texture in the bindless `g_textures` array. Transient textures read
    /// with [`Access::Sampled`] get one when the frame executes with
    /// [`FrameGraph::execute_with_bindless`]; imported ones carry their owner's.
    pub fn sampled_index(&self, texture: Texture) -> Option<u32> {
        self.physical[texture.0 as usize].sampled_index
    }

    /// Index of a graph texture in the bindless `g_images` array (storage accesses), as
    /// [`Self::sampled_index`].
    pub fn storage_index(&self, texture: Texture) -> Option<u32> {
        self.physical[texture.0 as usize].storage_index
    }

    /// The area covered by the pass's attachments (zero for passes without any).
    pub fn render_area(&self) -> vk::Rect2D {
        self.render_area
    }
}

/// The persistent render graph: per-frame bookkeeping, the transient heaps and the Vulkan
/// objects of transient resources, reused across frames. Drop it (after the device is
/// idle) with [`RenderGraph::destroy`] before the device.
pub struct RenderGraph {
    device: ash::Device,
    debug_utils: Option<ash::ext::debug_utils::Device>,
    records: Records,
    attachments: Vec<AttachmentRecord>,
    textures: Vec<TextureDesc>,
    buffers: Vec<BufferDesc>,
    imports: Vec<Physical>,
    callbacks: Vec<Option<NonNull<Callback>>>,
    arena: Arena,
    physical: PhysicalResources,
    vk_image_barriers: Vec<vk::ImageMemoryBarrier2<'static>>,
    vk_buffer_barriers: Vec<vk::BufferMemoryBarrier2<'static>>,
    color_attachments: Vec<vk::RenderingAttachmentInfo<'static>>,
    /// Per queue (graphics, async compute).
    timing: [Option<timing::Timing>; 2],
    times: Vec<PassTime>,
    frame_us: Option<f32>,
    /// The graphics and async compute queues (the async compute queue only if the device
    /// has one), and their families.
    queues: [Option<Queue>; 2],
    families: [u32; 2],
    async_compute: bool,
    sharing: AsyncSharing,
    /// Command pools and semaphores for async compute frames, created by the first one.
    multi_queue: Option<MultiQueue>,
    /// One breadcrumb track per frame slot, and the frames recorded (ADR 0061).
    breadcrumbs: Vec<engine_rhi::Track>,
    frames_recorded: u64,
    /// This frame's [`FrameGraph::serial`].
    serial: u64,
    stats: GraphStats,
    /// Scopes of the frame being built, and the one new passes go into.
    scopes: Vec<ScopeRecord>,
    current_scope: u32,
}

/// A named group of passes (a viewport, a subsystem): labelled in captures, attributed in
/// GPU timings.
struct ScopeRecord {
    id: u32,
    label: [u8; 64],
}

/// What frames on two queues need: per frame slot and queue a command pool with its
/// command buffers (one per segment), and a timeline semaphore per queue that each
/// segment signals.
struct MultiQueue {
    timelines: [vk::Semaphore; 2],
    values: [u64; 2],
    pools: Vec<[CommandPool; 2]>,
    /// Per segment of the frame: its command buffer and the value it signals.
    segment_cmds: Vec<vk::CommandBuffer>,
    segment_values: Vec<u64>,
}

#[derive(Default)]
struct CommandPool {
    pool: vk::CommandPool,
    buffers: Vec<vk::CommandBuffer>,
    used: usize,
}

impl MultiQueue {
    fn new(device: &ash::Device, families: [u32; 2], slots: usize) -> Result<Self, RhiError> {
        let mut this = Self {
            timelines: [vk::Semaphore::null(); 2],
            values: [0; 2],
            pools: Vec::new(),
            segment_cmds: Vec::new(),
            segment_values: Vec::new(),
        };
        let result = (|| {
            for timeline in &mut this.timelines {
                let mut kind = vk::SemaphoreTypeCreateInfo::default()
                    .semaphore_type(vk::SemaphoreType::TIMELINE)
                    .initial_value(0);
                // SAFETY: valid create info.
                *timeline = unsafe {
                    device.create_semaphore(
                        &vk::SemaphoreCreateInfo::default().push_next(&mut kind),
                        None,
                    )
                }
                .map_err(RhiError::vk("vkCreateSemaphore(timeline)"))?;
            }
            for _ in 0..slots {
                let mut pools: [CommandPool; 2] = Default::default();
                for (pool, &family) in pools.iter_mut().zip(&families) {
                    // SAFETY: valid create info.
                    pool.pool = unsafe {
                        device.create_command_pool(
                            &vk::CommandPoolCreateInfo::default()
                                .queue_family_index(family)
                                .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                            None,
                        )
                    }
                    .map_err(RhiError::vk("vkCreateCommandPool"))?;
                }
                this.pools.push(pools);
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(this),
            Err(e) => {
                // SAFETY: nothing was submitted with these objects.
                unsafe { this.destroy(device) };
                Err(e)
            }
        }
    }

    /// A command buffer of `queue`'s pool in `slot`, begun.
    fn begin(
        &mut self,
        device: &ash::Device,
        slot: usize,
        queue: QueueKind,
    ) -> Result<vk::CommandBuffer, RhiError> {
        let pool = &mut self.pools[slot][queue.index()];
        if pool.used == pool.buffers.len() {
            // SAFETY: valid allocate info; the pool belongs to this device.
            let cmd = unsafe {
                device.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
            }
            .map_err(RhiError::vk("vkAllocateCommandBuffers"))?[0];
            pool.buffers.push(cmd);
        }
        let cmd = pool.buffers[pool.used];
        pool.used += 1;
        // SAFETY: the pool was reset at the start of the frame, after the slot's previous
        // work finished.
        unsafe {
            device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .map_err(RhiError::vk("vkBeginCommandBuffer"))?;
        Ok(cmd)
    }

    /// Resets `slot`'s pools for a new frame.
    ///
    /// # Safety
    ///
    /// The slot's previous frame has finished on the GPU.
    unsafe fn reset(&mut self, device: &ash::Device, slot: usize) -> Result<(), RhiError> {
        for pool in &mut self.pools[slot] {
            if pool.used > 0 {
                // SAFETY: guaranteed by the caller.
                unsafe { device.reset_command_pool(pool.pool, vk::CommandPoolResetFlags::empty()) }
                    .map_err(RhiError::vk("vkResetCommandPool"))?;
            }
            pool.used = 0;
        }
        Ok(())
    }

    /// # Safety
    ///
    /// The GPU no longer uses any of the objects.
    unsafe fn destroy(&mut self, device: &ash::Device) {
        // SAFETY: guaranteed by the caller; null handles are ignored.
        unsafe {
            for pools in self.pools.drain(..) {
                for pool in pools {
                    if pool.pool != vk::CommandPool::null() {
                        device.destroy_command_pool(pool.pool, None);
                    }
                }
            }
            for timeline in &mut self.timelines {
                if *timeline != vk::Semaphore::null() {
                    device.destroy_semaphore(*timeline, None);
                    *timeline = vk::Semaphore::null();
                }
            }
        }
    }
}

// SAFETY: the callback pointers are only set and used between `begin` and the end of
// `execute` (or the drop of the frame), which borrow the graph mutably on one thread.
unsafe impl Send for RenderGraph {}

impl core::fmt::Debug for RenderGraph {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RenderGraph")
            .field("stats", &self.stats)
            .finish()
    }
}

impl RenderGraph {
    /// A graph for `device` with `frames_in_flight` frame slots (each has its own
    /// transient heaps, as frames in flight run concurrently on the GPU).
    pub fn new(device: &Device, frames_in_flight: usize) -> Self {
        let debug_utils = device
            .debug_utils_enabled()
            .then(|| ash::ext::debug_utils::Device::new(device.instance_raw(), device.raw()));
        let slots = frames_in_flight.max(1);
        let compute = device.async_compute;
        let families = [
            device.graphics.family,
            compute.map_or(device.graphics.family, |q| q.family),
        ];
        Self {
            device: device.raw().clone(),
            debug_utils,
            records: Records::default(),
            attachments: Vec::new(),
            textures: Vec::new(),
            buffers: Vec::new(),
            imports: Vec::new(),
            callbacks: Vec::new(),
            arena: Arena::new(64 * 1024),
            physical: PhysicalResources::new(slots, families),
            vk_image_barriers: Vec::new(),
            vk_buffer_barriers: Vec::new(),
            color_attachments: Vec::new(),
            timing: [
                timing::Timing::new(device, slots, device.graphics, QueueKind::Graphics),
                compute
                    .and_then(|q| timing::Timing::new(device, slots, q, QueueKind::AsyncCompute)),
            ],
            times: Vec::new(),
            frame_us: None,
            queues: [Some(device.graphics), compute],
            families,
            async_compute: async_compute_default(device),
            sharing: sharing_default(),
            multi_queue: None,
            breadcrumbs: breadcrumb_tracks(device, frames_in_flight.max(1)),
            frames_recorded: 0,
            serial: 0,
            stats: GraphStats::default(),
            scopes: Vec::new(),
            current_scope: compile::NO_SCOPE,
        }
    }

    /// Starts declaring a frame recorded into `frame`'s command buffer. `frame` comes
    /// from `engine_rhi::Frames::begin`, which has waited until the GPU finished the
    /// frame that last used the same slot, so its transient resources can be reused.
    pub fn begin<'g>(&'g mut self, frame: &engine_rhi::Frame) -> FrameGraph<'g> {
        // SAFETY: `Frames::begin` waited for the slot's previous submission.
        unsafe { self.begin_slot(frame.slot(), frame.cmd) }
    }

    /// Like [`Self::begin`] for callers that manage frames themselves (tests, tools).
    ///
    /// # Safety
    ///
    /// The GPU has finished every submission that used transient resources of `slot`
    /// (they may be destroyed or overwritten while recording this frame).
    pub unsafe fn begin_slot<'g>(
        &'g mut self,
        slot: usize,
        cmd: vk::CommandBuffer,
    ) -> FrameGraph<'g> {
        self.records.clear();
        self.attachments.clear();
        self.textures.clear();
        self.buffers.clear();
        self.imports.clear();
        self.callbacks.clear();
        self.arena.reset();
        self.scopes.clear();
        self.current_scope = compile::NO_SCOPE;
        self.serial = FRAME_SERIAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        FrameGraph {
            slot: slot % self.physical.slots(),
            cmd,
            graph: self,
        }
    }

    /// Counts from the last executed frame.
    pub fn stats(&self) -> GraphStats {
        self.stats
    }

    /// Runs passes declared with [`PassBuilder::async_compute`] on the device's async
    /// compute queue (ADR 0065), or everything on the graphics queue. The default is
    /// [`async_compute_default`]; without an async compute queue this does nothing.
    ///
    /// With async compute, [`FrameGraph::execute`] submits the frame's earlier segments
    /// itself, to the device's graphics and async compute queues, and records the last
    /// graphics segment into the frame's command buffer, which the caller submits to the
    /// device's graphics queue afterwards as before. Commands the caller records into it
    /// after `execute` run after the whole frame; the queues must not be used from other
    /// threads during `execute`.
    pub fn set_async_compute(&mut self, enabled: bool) {
        self.async_compute = enabled && self.queues[1].is_some();
    }

    /// Whether async passes run on the async compute queue.
    pub fn async_compute(&self) -> bool {
        self.async_compute
    }

    /// How transients used on both queues are shared (default
    /// [`AsyncSharing::Exclusive`], or `ENGINE_ASYNC_SHARING`).
    pub fn set_async_sharing(&mut self, sharing: AsyncSharing) {
        self.sharing = sharing;
    }

    /// GPU time of every pass of the most recently completed frame (read when its frame
    /// slot comes around again): the graphics queue's passes, then the async compute
    /// queue's. Empty until then, or if the queue has no timestamps.
    pub fn gpu_pass_times(&self) -> &[PassTime] {
        &self.times
    }

    /// GPU time of the same frame from its first pass to its last, on the graphics queue
    /// (which waits for the async compute work): with async compute the frame is shorter
    /// than the sum of its passes.
    pub fn gpu_frame_time(&self) -> Option<f32> {
        self.frame_us
    }

    /// How far the GPU got in the frame last recorded into `slot`, from the breadcrumbs:
    /// the index (among the passes recorded, culled ones not counted) of the last pass it
    /// started and of the last it finished. `None` without breadcrumbs.
    pub fn gpu_progress(&self, slot: usize) -> Option<(Option<u32>, Option<u32>)> {
        let slots = self.physical.slots();
        self.breadcrumbs
            .get(slot % slots)
            .map(engine_rhi::Track::progress)
    }

    /// Frees the transient heaps and Vulkan objects, and returns their bindless indices to
    /// `bindless` (the set used with [`FrameGraph::execute_with_bindless`], if any).
    ///
    /// # Safety
    ///
    /// The GPU no longer uses any of them (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: Option<&Bindless>) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.physical.destroy_all(&self.device, bindless, allocator);
            for timing in self.timing.iter_mut().flatten() {
                timing.destroy(&self.device);
            }
            if let Some(mut multi) = self.multi_queue.take() {
                multi.destroy(&self.device);
            }
        }
    }
}

/// One frame's declarations, from [`RenderGraph::begin`] to [`FrameGraph::execute`].
pub struct FrameGraph<'g> {
    graph: &'g mut RenderGraph,
    slot: usize,
    cmd: vk::CommandBuffer,
}

impl core::fmt::Debug for FrameGraph<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FrameGraph")
            .field("slot", &self.slot)
            .field("passes", &self.graph.records.passes.len())
            .finish()
    }
}

/// Source of [`FrameGraph::serial`]: unique across every graph of the process.
static FRAME_SERIAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

impl<'g> FrameGraph<'g> {
    /// A number no other frame of any graph has (never 0): owners of persistent resources
    /// that several views of one frame use import them once per frame by it (an imported
    /// resource leaves the graph in its final state only at the end of the frame).
    pub fn serial(&self) -> u64 {
        self.graph.serial
    }

    fn add_resource(&mut self, name: &'static str, kind: Kind) -> u32 {
        let records = &mut self.graph.records;
        records.resources.push(ResourceRecord::new(name, kind));
        self.graph.imports.push(Physical::default());
        self.graph
            .textures
            .push(TextureDesc::new_2d(vk::Format::UNDEFINED, 0, 0));
        self.graph.buffers.push(BufferDesc { size: 0 });
        (records.resources.len() - 1) as u32
    }

    /// A texture that lives only during this frame; its memory may be shared with other
    /// transient resources whose lifetimes do not overlap.
    pub fn create_texture(&mut self, name: &'static str, desc: TextureDesc) -> Texture {
        let id = self.add_resource(name, Kind::Texture);
        self.graph.textures[id as usize] = desc;
        Texture(id)
    }

    /// A buffer that lives only during this frame (device-local, with a device address).
    pub fn create_buffer(&mut self, name: &'static str, desc: BufferDesc) -> Buffer {
        let id = self.add_resource(name, Kind::Buffer);
        self.graph.buffers[id as usize] = desc;
        Buffer(id)
    }

    /// Size and format of a texture of this frame.
    pub fn texture_desc(&self, texture: Texture) -> TextureDesc {
        self.graph.textures[texture.0 as usize]
    }

    /// Uses a texture owned outside the graph. Passes writing it are never culled.
    pub fn import_texture(&mut self, name: &'static str, texture: ImportedTexture) -> Texture {
        let id = self.add_resource(name, Kind::Texture);
        let r = &mut self.graph.records.resources[id as usize];
        r.initial = Some(Boundary::from(texture.initial));
        r.final_state = texture.final_state.map(Boundary::from);
        r.caller_waits = texture.initial == TextureState::acquired_swapchain_image();
        self.graph.textures[id as usize] = texture.desc;
        self.graph.imports[id as usize] = Physical {
            image: texture.image,
            view: texture.view,
            extent: texture.desc.extent(),
            sampled_index: texture.sampled_index,
            storage_index: texture.storage_index,
            ..Physical::default()
        };
        Texture(id)
    }

    /// Uses a buffer owned outside the graph. Passes writing it are never culled.
    pub fn import_buffer(&mut self, name: &'static str, buffer: ImportedBuffer) -> Buffer {
        let id = self.add_resource(name, Kind::Buffer);
        let r = &mut self.graph.records.resources[id as usize];
        r.initial = Some(Boundary::from(buffer.initial));
        r.final_state = buffer.final_state.map(Boundary::from);
        self.graph.buffers[id as usize] = BufferDesc { size: buffer.size };
        self.graph.imports[id as usize] = Physical {
            buffer: buffer.buffer,
            address: buffer.address,
            ..Physical::default()
        };
        Buffer(id)
    }

    /// Puts the passes declared from now on into a scope with a caller-chosen `id` (for
    /// example a viewport's): GPU captures show them under `label` (up to 63 bytes), and
    /// their [`PassTime::scope`] is `Some(id)`. Scopes do not nest; a new one ends the
    /// previous one.
    pub fn begin_scope(&mut self, id: u32, label: &str) {
        let mut bytes = [0u8; 64];
        let len = label.len().min(63);
        bytes[..len].copy_from_slice(&label.as_bytes()[..len]);
        self.graph.scopes.push(ScopeRecord { id, label: bytes });
        self.graph.current_scope = (self.graph.scopes.len() - 1) as u32;
    }

    /// Ends the current scope: later passes belong to none.
    pub fn end_scope(&mut self) {
        self.graph.current_scope = compile::NO_SCOPE;
    }

    /// Declares a pass. Passes run in declaration order.
    pub fn add_pass(&mut self, name: &'static str) -> PassBuilder<'_, 'g> {
        let records = &mut self.graph.records;
        records.passes.push(PassRecord::new(
            name,
            self.graph.current_scope,
            records.accesses.len() as u32,
            self.graph.attachments.len() as u32,
        ));
        self.graph.callbacks.push(None);
        PassBuilder { frame: self }
    }

    /// Compiles the frame, creates or reuses transient resources, and records every
    /// needed pass into the frame's command buffer (with async compute, see
    /// [`RenderGraph::set_async_compute`]: the earlier segments are submitted here, the
    /// last graphics one goes into the frame's command buffer).
    pub fn execute(self, allocator: &mut Allocator) -> Result<(), RhiError> {
        self.execute_with_bindless(allocator, None)
    }

    /// [`Self::execute`], also registering transient textures that are sampled or used as
    /// storage images in `bindless` (see [`PassContext::sampled_index`]). Use the same
    /// set for every frame, and pass it to [`RenderGraph::destroy`].
    pub fn execute_with_bindless(
        mut self,
        allocator: &mut Allocator,
        bindless: Option<&Bindless>,
    ) -> Result<(), RhiError> {
        engine_profiler::zone!("render::graph::execute");
        let result = self.execute_inner(allocator, bindless);
        self.drop_callbacks();
        result
    }

    #[allow(clippy::too_many_lines)]
    fn execute_inner(
        &mut self,
        allocator: &mut Allocator,
        bindless: Option<&Bindless>,
    ) -> Result<(), RhiError> {
        let graph = &mut *self.graph;
        let used_async = {
            engine_profiler::zone!("render::graph::compile");
            graph.records.cull();
            let used_async = graph.records.schedule(ScheduleOptions {
                async_compute: graph.async_compute,
                concurrent: graph.sharing == AsyncSharing::Concurrent,
            });
            graph.records.resolve_lifetimes();
            used_async
        };
        let realized = {
            engine_profiler::zone!("render::graph::realize");
            // SAFETY: `begin`/`begin_slot` guaranteed that the slot's previous work is done.
            unsafe {
                graph.physical.realize(
                    self.slot,
                    &graph.device,
                    graph.debug_utils.as_ref(),
                    bindless,
                    allocator,
                    &mut graph.records,
                    &graph.textures,
                    &graph.buffers,
                    &graph.imports,
                )
            }?
        };
        graph.records.build_barriers();

        let slot = self.slot;
        let slot_count = graph.physical.slots();
        let RenderGraph {
            device: d,
            debug_utils,
            records,
            attachments,
            textures,
            callbacks,
            physical,
            vk_image_barriers,
            vk_buffer_barriers,
            color_attachments,
            timing,
            times,
            frame_us,
            queues,
            families,
            multi_queue,
            breadcrumbs,
            frames_recorded,
            stats: graph_stats,
            scopes,
            ..
        } = graph;

        // GPU times of the slot's previous frame, from both queues.
        for t in timing.iter_mut().flatten() {
            // SAFETY: the slot's previous frame is done (`begin`).
            unsafe { t.collect(d, slot) };
        }
        times.clear();
        for t in timing.iter().flatten() {
            times.extend_from_slice(t.times());
        }
        *frame_us = timing[0].as_ref().and_then(timing::Timing::frame_us);

        *frames_recorded += 1;
        let crumbs = breadcrumbs.get(slot);
        if let Some(track) = crumbs {
            track.begin_frame(*frames_recorded);
        }
        let mut stats = GraphStats {
            passes: records.passes.len() as u32,
            culled_passes: records.passes.iter().filter(|p| !p.needed).count() as u32,
            transient_resources: realized.transient_resources,
            transient_heap_bytes: realized.heap_bytes,
            transient_placed_bytes: realized.placed_bytes,
            transient_unaliased_bytes: realized.unaliased_bytes,
            created_objects: realized.created_objects,
            ..GraphStats::default()
        };
        let mut queries = [2u32, 0];
        for &p in &records.order {
            let pass = &records.passes[p as usize];
            queries[pass.queue.index()] += 2;
            stats.async_passes += u32::from(pass.queue == QueueKind::AsyncCompute);
        }
        let last_graphics = records
            .segments
            .iter()
            .rposition(|s| s.queue == QueueKind::Graphics)
            .unwrap_or(0);
        if used_async && multi_queue.is_none() {
            *multi_queue = Some(MultiQueue::new(d, *families, slot_count)?);
        }
        let mut multi = if used_async {
            multi_queue.as_mut()
        } else {
            None
        };
        if let Some(multi) = multi.as_deref_mut() {
            // SAFETY: the slot's previous frame is done (`begin`).
            unsafe { multi.reset(d, slot) }?;
            multi.segment_cmds.clear();
            multi.segment_values.clear();
        }

        let mut recorder = Recorder {
            d,
            utils: debug_utils.as_ref(),
            records,
            attachments,
            textures,
            physical: physical.current(),
            callbacks,
            scopes,
            families,
            image_scratch: vk_image_barriers,
            buffer_scratch: vk_buffer_barriers,
            colors: color_attachments,
            timing,
            crumbs,
            slot,
            recorded: 0,
            stats: &mut stats,
        };
        let mut started = [false; 2];
        for (index, segment) in records.segments.iter().enumerate() {
            let queue = segment.queue;
            let cmd = match multi.as_deref_mut() {
                Some(multi) if index != last_graphics => multi.begin(d, slot, queue)?,
                _ => self.cmd,
            };
            if !started[queue.index()] {
                started[queue.index()] = true;
                if let Some(t) = &mut recorder.timing[queue.index()] {
                    // SAFETY: the slot's previous frame is done; this is the queue's first
                    // command buffer of the frame, outside a render pass.
                    unsafe { t.reset(d, cmd, slot, queries[queue.index()]) };
                    if queue == QueueKind::Graphics {
                        // SAFETY: just reset.
                        unsafe { t.frame_mark(d, cmd, slot, false) };
                    }
                }
            }
            recorder.segment(cmd, index);
            if index == last_graphics {
                recorder.finals(cmd);
                if let Some(t) = &mut recorder.timing[0] {
                    // SAFETY: recording on the graphics queue, after the reset.
                    unsafe { t.frame_mark(d, cmd, slot, true) };
                }
            } else {
                // SAFETY: begun by `MultiQueue::begin`, recorded above.
                unsafe { d.end_command_buffer(cmd) }.map_err(RhiError::vk("vkEndCommandBuffer"))?;
            }
            if let Some(multi) = multi.as_deref_mut() {
                multi.segment_cmds.push(cmd);
            }
        }
        let recorded = recorder.recorded;
        if let Some(track) = crumbs {
            track.end_frame(recorded);
        }

        // Submit every segment but the caller's, in order; each signals its queue's
        // timeline and waits for the segment of the other queue it depends on.
        if let Some(multi) = multi {
            for (index, segment) in records.segments.iter().enumerate() {
                let q = segment.queue.index();
                let waits = [vk::SemaphoreSubmitInfo::default()
                    .semaphore(multi.timelines[1 - q])
                    .value(
                        multi
                            .segment_values
                            .get(segment.wait as usize)
                            .copied()
                            .unwrap_or(0),
                    )
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
                let waits: &[vk::SemaphoreSubmitInfo<'_>] = if segment.wait == NO_SEGMENT {
                    &[]
                } else {
                    &waits
                };
                let Some(queue) = queues[q] else {
                    continue;
                };
                if index == last_graphics {
                    // The caller submits its command buffer next: an empty batch before it
                    // waits for the compute work (a wait also holds back the batches
                    // after it on the queue).
                    multi.segment_values.push(0);
                    if !waits.is_empty() {
                        let submit = vk::SubmitInfo2::default().wait_semaphore_infos(waits);
                        // SAFETY: the semaphore and its value were signaled by an earlier
                        // submission; the queue is not used concurrently (caller).
                        unsafe { d.queue_submit2(queue.raw, &[submit], vk::Fence::null()) }
                            .map_err(RhiError::vk("vkQueueSubmit2"))?;
                        stats.submits += 1;
                    }
                    continue;
                }
                multi.values[q] += 1;
                let value = multi.values[q];
                multi.segment_values.push(value);
                let signals = [vk::SemaphoreSubmitInfo::default()
                    .semaphore(multi.timelines[q])
                    .value(value)
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
                let cmds = [vk::CommandBufferSubmitInfo::default()
                    .command_buffer(multi.segment_cmds[index])];
                let submit = vk::SubmitInfo2::default()
                    .wait_semaphore_infos(waits)
                    .command_buffer_infos(&cmds)
                    .signal_semaphore_infos(&signals);
                // SAFETY: the command buffer is recorded and ended; the waited value was
                // signaled by an earlier submission; the queue is not used concurrently
                // (caller).
                unsafe { d.queue_submit2(queue.raw, &[submit], vk::Fence::null()) }
                    .map_err(RhiError::vk("vkQueueSubmit2"))?;
                stats.submits += 1;
            }
        }
        *graph_stats = stats;
        Ok(())
    }

    fn drop_callbacks(&mut self) {
        for callback in self.graph.callbacks.drain(..).flatten() {
            // SAFETY: each callback was written into the arena by `PassBuilder::execute`
            // and is dropped exactly once, here; the arena is reset only after this.
            unsafe { core::ptr::drop_in_place(callback.as_ptr()) };
        }
    }
}

impl Drop for FrameGraph<'_> {
    fn drop(&mut self) {
        // Frames abandoned before `execute` still release what their callbacks captured.
        self.drop_callbacks();
    }
}

/// Declares one pass: its attachments, other accesses and the recording callback.
pub struct PassBuilder<'f, 'g> {
    frame: &'f mut FrameGraph<'g>,
}

impl core::fmt::Debug for PassBuilder<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PassBuilder").finish_non_exhaustive()
    }
}

impl<'g> PassBuilder<'_, 'g> {
    fn access(&mut self, resource: u32, info: AccessInfo) {
        self.frame.graph.records.add_access(resource, info);
    }

    fn attachment(
        &mut self,
        texture: Texture,
        depth: bool,
        read_only: bool,
        load: LoadOp,
        store: StoreOp,
    ) {
        let graph = &mut *self.frame.graph;
        graph.attachments.push(AttachmentRecord {
            resource: texture.0,
            depth,
            read_only,
            load,
            store,
            resolve: None,
        });
        if let Some(pass) = graph.records.passes.last_mut() {
            pass.attachments.end = graph.attachments.len() as u32;
        }
    }

    /// Renders to `texture` as a color attachment (in declaration order: location 0, 1…).
    pub fn color(mut self, texture: Texture, load: LoadOp, store: StoreOp) -> Self {
        self.access(
            texture.0,
            AccessInfo::color_attachment(load == LoadOp::Load),
        );
        self.attachment(texture, false, false, load, store);
        self
    }

    /// Uses `texture` as the depth attachment, with depth writes.
    pub fn depth(mut self, texture: Texture, load: LoadOp, store: StoreOp) -> Self {
        self.access(
            texture.0,
            AccessInfo::depth_attachment(load == LoadOp::Load, true),
        );
        self.attachment(texture, true, false, load, store);
        self
    }

    /// Uses `texture` as a read-only depth attachment (depth test without writes).
    pub fn depth_read_only(mut self, texture: Texture) -> Self {
        self.access(texture.0, AccessInfo::depth_attachment(true, false));
        self.attachment(texture, true, true, LoadOp::Load, StoreOp::Store);
        self
    }

    /// Resolves the attachment declared last (multisampled) into `target` (single-sampled,
    /// the same size and format) at the end of the pass, with `mode` (ADR 0075). The
    /// device must support the mode for the format (`Adapter::depth_resolve_modes` for
    /// depth).
    pub fn resolve(mut self, target: Texture, mode: ResolveMode) -> Self {
        let graph = &mut *self.frame.graph;
        let Some(source) = graph.attachments.last_mut() else {
            return self;
        };
        source.resolve = Some((target.0, mode.flags()));
        let (resource, depth) = (source.resource, source.depth);
        self.access(resource, AccessInfo::resolve_source());
        self.access(target.0, AccessInfo::resolve_target(depth));
        self
    }

    /// Declares another use of a texture or buffer.
    pub fn read(mut self, resource: impl GraphResource, access: Access) -> Self {
        self.access(resource.id(), AccessInfo::of(access));
        self
    }

    /// Same as [`Self::read`]; reads more naturally for write accesses.
    pub fn write(self, resource: impl GraphResource, access: Access) -> Self {
        self.read(resource, access)
    }

    /// [`Self::read`] when `resource` is `Some`.
    pub fn maybe_read(self, resource: Option<impl GraphResource>, access: Access) -> Self {
        match resource {
            Some(resource) => self.read(resource, access),
            None => self,
        }
    }

    /// Runs this pass on the async compute queue when the graph has async compute on
    /// (ADR 0065), overlapping graphics work that does not lead to it. The pass must
    /// declare every resource it touches; one with attachments, or with accesses in
    /// stages other than compute, transfer and indirect arguments, stays on the graphics
    /// queue. Results are the same on either queue.
    pub fn async_compute(self) -> Self {
        if let Some(pass) = self.frame.graph.records.passes.last_mut() {
            pass.async_requested = true;
        }
        self
    }

    /// Never cull this pass (it writes outside the graph: readbacks, queries, UI state).
    pub fn side_effects(self) -> Self {
        if let Some(pass) = self.frame.graph.records.passes.last_mut() {
            pass.side_effects = true;
        }
        self
    }

    /// Sets the callback that records the pass's commands. Without one, the pass only
    /// performs its attachments' load and store operations (for example a clear).
    pub fn execute(self, callback: impl FnOnce(&mut PassContext<'_>) + 'g) {
        let mut callback = Some(callback);
        let wrapper = move |ctx: &mut PassContext<'_>| {
            if let Some(callback) = callback.take() {
                callback(ctx);
            }
        };
        let graph = &mut *self.frame.graph;
        let stored: &mut (dyn FnMut(&mut PassContext<'_>) + 'g) = graph.arena.alloc(wrapper);
        let raw: *mut (dyn FnMut(&mut PassContext<'_>) + 'g) = stored;
        // SAFETY: erases the `'g` bound for storage in the graph. The callback is only
        // called and dropped while the `FrameGraph<'g>` exists (`execute` or its `Drop`).
        let raw: *mut Callback = unsafe { core::mem::transmute(raw) };
        if let Some(slot) = graph.callbacks.last_mut() {
            *slot = NonNull::new(raw);
        }
    }
}

/// A graph texture or buffer.
pub trait GraphResource: Copy {
    /// Index in the frame's resource table.
    fn id(self) -> u32;
}

impl GraphResource for Texture {
    fn id(self) -> u32 {
        self.0
    }
}

impl GraphResource for Buffer {
    fn id(self) -> u32 {
        self.0
    }
}

/// A breadcrumb track per frame slot, if the device has breadcrumbs and enough tracks
/// are free.
fn breadcrumb_tracks(device: &Device, slots: usize) -> Vec<engine_rhi::Track> {
    const LABELS: [&str; 4] = [
        "render graph, frame slot 0",
        "render graph, frame slot 1",
        "render graph, frame slot 2",
        "render graph",
    ];
    let Some(breadcrumbs) = device.breadcrumbs() else {
        return Vec::new();
    };
    (0..slots)
        .map(|slot| breadcrumbs.track(LABELS[slot.min(3)]))
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}

pub(crate) fn aspect_of(format: vk::Format) -> vk::ImageAspectFlags {
    match format {
        vk::Format::D16_UNORM | vk::Format::X8_D24_UNORM_PACK32 | vk::Format::D32_SFLOAT => {
            vk::ImageAspectFlags::DEPTH
        }
        vk::Format::D16_UNORM_S8_UINT
        | vk::Format::D24_UNORM_S8_UINT
        | vk::Format::D32_SFLOAT_S8_UINT => {
            vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL
        }
        vk::Format::S8_UINT => vk::ImageAspectFlags::STENCIL,
        _ => vk::ImageAspectFlags::COLOR,
    }
}

/// Records a frame's segments: each pass with its barriers, rendering scope, labels,
/// timestamps and breadcrumbs.
struct Recorder<'a> {
    d: &'a ash::Device,
    utils: Option<&'a ash::ext::debug_utils::Device>,
    records: &'a Records,
    attachments: &'a [AttachmentRecord],
    textures: &'a [TextureDesc],
    physical: &'a [Physical],
    callbacks: &'a [Option<NonNull<Callback>>],
    scopes: &'a [ScopeRecord],
    families: &'a [u32; 2],
    image_scratch: &'a mut Vec<vk::ImageMemoryBarrier2<'static>>,
    buffer_scratch: &'a mut Vec<vk::BufferMemoryBarrier2<'static>>,
    colors: &'a mut Vec<vk::RenderingAttachmentInfo<'static>>,
    timing: &'a mut [Option<timing::Timing>; 2],
    crumbs: Option<&'a engine_rhi::Track>,
    slot: usize,
    /// Graphics passes recorded (breadcrumb indices).
    recorded: u32,
    stats: &'a mut GraphStats,
}

impl Recorder<'_> {
    /// Records segment `index` into `cmd`: its passes, then its releases.
    fn segment(&mut self, cmd: vk::CommandBuffer, index: usize) {
        let records = self.records;
        let segment = &records.segments[index];
        let mut open_scope = compile::NO_SCOPE;
        for &p in
            &records.segment_passes[segment.passes.start as usize..segment.passes.end as usize]
        {
            let pass = &records.passes[p as usize];
            if pass.scope != open_scope {
                if let Some(utils) = self.utils {
                    if open_scope != compile::NO_SCOPE {
                        // SAFETY: closes the scope label opened below.
                        unsafe { utils.cmd_end_debug_utils_label(cmd) };
                    }
                    if let Some(scope) = self.scopes.get(pass.scope as usize) {
                        let label = vk::DebugUtilsLabelEXT::default().label_name(
                            core::ffi::CStr::from_bytes_until_nul(&scope.label).unwrap_or(c"scope"),
                        );
                        // SAFETY: recording command buffer; the label is copied.
                        unsafe { utils.cmd_begin_debug_utils_label(cmd, &label) };
                    }
                }
                open_scope = pass.scope;
            }
            self.pass(cmd, p as usize);
        }
        if open_scope != compile::NO_SCOPE
            && let Some(utils) = self.utils
        {
            // SAFETY: closes the last scope label.
            unsafe { utils.cmd_end_debug_utils_label(cmd) };
        }
        let images = &records.release_images
            [segment.release_images.start as usize..segment.release_images.end as usize];
        let buffers = &records.release_buffers
            [segment.release_buffers.start as usize..segment.release_buffers.end as usize];
        self.stats.image_barriers += images.len() as u32;
        record_barriers(
            self.d,
            cmd,
            self.image_scratch,
            self.buffer_scratch,
            images,
            buffers,
            &MemoryBarrier::default(),
            self.physical,
            self.textures,
            self.families,
        );
    }

    fn pass(&mut self, cmd: vk::CommandBuffer, index: usize) {
        let (d, records, slot) = (self.d, self.records, self.slot);
        let pass = &records.passes[index];
        let queue = pass.queue;
        let scope_id = self.scopes.get(pass.scope as usize).map(|s| s.id);
        if let Some(utils) = self.utils {
            let mut name = [0u8; 64];
            let len = pass.name.len().min(63);
            name[..len].copy_from_slice(&pass.name.as_bytes()[..len]);
            let label = vk::DebugUtilsLabelEXT::default()
                .label_name(core::ffi::CStr::from_bytes_until_nul(&name).unwrap_or(c"pass"));
            // SAFETY: recording command buffer; the label is copied by the call.
            unsafe { utils.cmd_begin_debug_utils_label(cmd, &label) };
        }
        let barriers = &records.image_barriers
            [pass.image_barriers.start as usize..pass.image_barriers.end as usize];
        let buffers = &records.buffer_barriers
            [pass.buffer_barriers.start as usize..pass.buffer_barriers.end as usize];
        self.stats.image_barriers += barriers.len() as u32;
        self.stats.memory_barriers += u32::from(!pass.memory_barrier.is_empty());
        self.stats.ownership_transfers +=
            (barriers.iter().filter(|b| b.transfer.is_some()).count() + buffers.len()) as u32;
        record_barriers(
            d,
            cmd,
            self.image_scratch,
            self.buffer_scratch,
            barriers,
            buffers,
            &pass.memory_barrier,
            self.physical,
            self.textures,
            self.families,
        );
        if let Some(timing) = &mut self.timing[queue.index()] {
            // SAFETY: recording; queries were prepared by `reset`.
            unsafe { timing.pass_begin(d, cmd, slot, pass.name, scope_id) };
        }
        let crumbs = self.crumbs.filter(|_| queue == QueueKind::Graphics);
        if let Some(track) = crumbs {
            // SAFETY: recording on the graphics queue, outside a render pass, between the
            // track's `begin_frame` and `end_frame`.
            unsafe { track.pass_begin(cmd, self.recorded, pass.name) };
        }
        let attachments =
            &self.attachments[pass.attachments.start as usize..pass.attachments.end as usize];
        let mut render_area = vk::Rect2D::default();
        if !attachments.is_empty() {
            render_area = begin_rendering(
                d,
                cmd,
                attachments,
                self.physical,
                self.textures,
                self.colors,
            );
        }
        if let Some(callback) = self.callbacks[index] {
            let mut ctx = PassContext {
                cmd,
                device: d,
                physical: self.physical,
                render_area,
            };
            // SAFETY: the callback lives in the frame arena until `drop_callbacks`, and
            // nothing else references it; each pass is recorded once.
            unsafe { (*callback.as_ptr())(&mut ctx) };
        }
        if !attachments.is_empty() {
            // SAFETY: matches the `cmd_begin_rendering` above.
            unsafe { d.cmd_end_rendering(cmd) };
        }
        if let Some(timing) = &mut self.timing[queue.index()] {
            // SAFETY: after `pass_begin` for this pass.
            unsafe { timing.pass_end(d, cmd, slot) };
        }
        if let Some(track) = crumbs {
            // SAFETY: as above, after `pass_begin` for this pass.
            unsafe { track.pass_end(cmd, self.recorded) };
            self.recorded += 1;
        }
        if let Some(utils) = self.utils {
            // SAFETY: matches the label begun above.
            unsafe { utils.cmd_end_debug_utils_label(cmd) };
        }
    }

    /// The transitions of imported resources to their final states.
    fn finals(&mut self, cmd: vk::CommandBuffer) {
        let records = self.records;
        let finals = &records.image_barriers[records.final_image_barriers.start as usize
            ..records.final_image_barriers.end as usize];
        let buffers = &records.buffer_barriers[records.final_buffer_barriers.start as usize
            ..records.final_buffer_barriers.end as usize];
        self.stats.image_barriers += finals.len() as u32;
        self.stats.memory_barriers += u32::from(!records.final_memory_barrier.is_empty());
        self.stats.ownership_transfers +=
            (finals.iter().filter(|b| b.transfer.is_some()).count() + buffers.len()) as u32;
        record_barriers(
            self.d,
            cmd,
            self.image_scratch,
            self.buffer_scratch,
            finals,
            buffers,
            &records.final_memory_barrier,
            self.physical,
            self.textures,
            self.families,
        );
    }
}

/// Queue family indices of an ownership transfer, or `IGNORED` for none.
fn transfer_families(transfer: Option<(QueueKind, QueueKind)>, families: &[u32; 2]) -> (u32, u32) {
    match transfer {
        Some((from, to)) if families[from.index()] != families[to.index()] => {
            (families[from.index()], families[to.index()])
        }
        _ => (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_barriers(
    d: &ash::Device,
    cmd: vk::CommandBuffer,
    scratch: &mut Vec<vk::ImageMemoryBarrier2<'static>>,
    buffer_scratch: &mut Vec<vk::BufferMemoryBarrier2<'static>>,
    barriers: &[ImageBarrier],
    buffer_barriers: &[BufferBarrier],
    memory: &MemoryBarrier,
    physical: &[Physical],
    textures: &[TextureDesc],
    families: &[u32; 2],
) {
    if barriers.is_empty() && buffer_barriers.is_empty() && memory.is_empty() {
        return;
    }
    scratch.clear();
    scratch.extend(barriers.iter().map(|b| {
        let desc = &textures[b.resource as usize];
        let (src_family, dst_family) = transfer_families(b.transfer, families);
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(b.src_stages)
            .src_access_mask(b.src_access)
            .dst_stage_mask(b.dst_stages)
            .dst_access_mask(b.dst_access)
            .old_layout(b.old_layout)
            .new_layout(b.new_layout)
            .src_queue_family_index(src_family)
            .dst_queue_family_index(dst_family)
            .image(physical[b.resource as usize].image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect_of(desc.format),
                base_mip_level: 0,
                level_count: vk::REMAINING_MIP_LEVELS,
                base_array_layer: 0,
                layer_count: vk::REMAINING_ARRAY_LAYERS,
            })
    }));
    buffer_scratch.clear();
    buffer_scratch.extend(buffer_barriers.iter().map(|b| {
        let (src_family, dst_family) = transfer_families(Some(b.transfer), families);
        vk::BufferMemoryBarrier2::default()
            .src_stage_mask(b.src_stages)
            .src_access_mask(b.src_access)
            .dst_stage_mask(b.dst_stages)
            .dst_access_mask(b.dst_access)
            .src_queue_family_index(src_family)
            .dst_queue_family_index(dst_family)
            .buffer(physical[b.resource as usize].buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE)
    }));
    let memory_barrier = [vk::MemoryBarrier2::default()
        .src_stage_mask(memory.src_stages)
        .src_access_mask(memory.src_access)
        .dst_stage_mask(memory.dst_stages)
        .dst_access_mask(memory.dst_access)];
    let mut dependency = vk::DependencyInfo::default()
        .image_memory_barriers(scratch)
        .buffer_memory_barriers(buffer_scratch);
    if !memory.is_empty() {
        dependency = dependency.memory_barriers(&memory_barrier);
    }
    // SAFETY: recording command buffer; every image and buffer belongs to the graph's
    // frame.
    unsafe { d.cmd_pipeline_barrier2(cmd, &dependency) };
}

fn begin_rendering(
    d: &ash::Device,
    cmd: vk::CommandBuffer,
    attachments: &[AttachmentRecord],
    physical: &[Physical],
    textures: &[TextureDesc],
    colors: &mut Vec<vk::RenderingAttachmentInfo<'static>>,
) -> vk::Rect2D {
    colors.clear();
    let mut depth = None;
    let mut extent = vk::Extent2D {
        width: u32::MAX,
        height: u32::MAX,
    };
    for a in attachments {
        let desc = &textures[a.resource as usize];
        extent.width = extent.width.min(desc.width);
        extent.height = extent.height.min(desc.height);
        let (load, clear) = match a.load {
            LoadOp::Load => (vk::AttachmentLoadOp::LOAD, [0.0; 4]),
            LoadOp::Clear(value) => (vk::AttachmentLoadOp::CLEAR, value),
            LoadOp::DontCare => (vk::AttachmentLoadOp::DONT_CARE, [0.0; 4]),
        };
        // A read-only attachment keeps its contents without a store, which would count as
        // a write (and race with a layout transition that only waited for reads, such as
        // a compute pass sampling the depth between the prepass and the shading).
        let store = match a.store {
            _ if a.read_only => vk::AttachmentStoreOp::NONE,
            StoreOp::Store => vk::AttachmentStoreOp::STORE,
            StoreOp::DontCare => vk::AttachmentStoreOp::DONT_CARE,
        };
        let clear_value = if a.depth {
            vk::ClearValue {
                depth_stencil: vk::ClearDepthStencilValue {
                    depth: clear[0],
                    stencil: clear[1] as u32,
                },
            }
        } else {
            vk::ClearValue {
                color: vk::ClearColorValue { float32: clear },
            }
        };
        let mut info = vk::RenderingAttachmentInfo::default()
            .image_view(physical[a.resource as usize].view)
            .image_layout(if a.read_only {
                vk::ImageLayout::READ_ONLY_OPTIMAL
            } else {
                vk::ImageLayout::ATTACHMENT_OPTIMAL
            })
            .load_op(load)
            .store_op(store)
            .clear_value(clear_value);
        if let Some((target, mode)) = a.resolve {
            info = info
                .resolve_mode(mode)
                .resolve_image_view(physical[target as usize].view)
                .resolve_image_layout(vk::ImageLayout::ATTACHMENT_OPTIMAL);
        }
        if a.depth {
            depth = Some(info);
        } else {
            colors.push(info);
        }
    }
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    };
    let mut rendering = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(colors);
    let depth_info;
    if let Some(info) = depth {
        depth_info = info;
        rendering = rendering.depth_attachment(&depth_info);
    }
    // SAFETY: recording command buffer; the views belong to this frame's graph.
    unsafe { d.cmd_begin_rendering(cmd, &rendering) };
    area
}
