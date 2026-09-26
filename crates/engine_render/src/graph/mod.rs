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
//!   each inside a debug label (RenderDoc, Nsight).
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
mod timing;

pub use access::{Access, BufferState, ShaderStages, TextureState};
pub use timing::PassTime;

use access::AccessInfo;
use compile::{Boundary, ImageBarrier, Kind, MemoryBarrier, PassRecord, Records, ResourceRecord};
use engine_core::arena::Arena;
use engine_rhi::ash::{self, vk};
use engine_rhi::{Allocator, Bindless, Device, RhiError};
use physical::{Physical, PhysicalResources};
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
    color_attachments: Vec<vk::RenderingAttachmentInfo<'static>>,
    timing: Option<timing::Timing>,
    stats: GraphStats,
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
            physical: PhysicalResources::new(frames_in_flight.max(1)),
            vk_image_barriers: Vec::new(),
            color_attachments: Vec::new(),
            timing: timing::Timing::new(device, frames_in_flight.max(1)),
            stats: GraphStats::default(),
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

    /// GPU time of every pass of the most recently completed frame (read when its frame
    /// slot comes around again). Empty until then, or if the queue has no timestamps.
    pub fn gpu_pass_times(&self) -> &[PassTime] {
        self.timing.as_ref().map_or(&[], |t| t.times())
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
            if let Some(timing) = &mut self.timing {
                timing.destroy(&self.device);
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

impl<'g> FrameGraph<'g> {
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

    /// Uses a texture owned outside the graph. Passes writing it are never culled.
    pub fn import_texture(&mut self, name: &'static str, texture: ImportedTexture) -> Texture {
        let id = self.add_resource(name, Kind::Texture);
        let r = &mut self.graph.records.resources[id as usize];
        r.initial = Some(Boundary::from(texture.initial));
        r.final_state = texture.final_state.map(Boundary::from);
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

    /// Declares a pass. Passes run in declaration order.
    pub fn add_pass(&mut self, name: &'static str) -> PassBuilder<'_, 'g> {
        let records = &mut self.graph.records;
        records.passes.push(PassRecord::new(
            name,
            records.accesses.len() as u32,
            self.graph.attachments.len() as u32,
        ));
        self.graph.callbacks.push(None);
        PassBuilder { frame: self }
    }

    /// Compiles the frame, creates or reuses transient resources, and records every
    /// needed pass into the frame's command buffer.
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

    fn execute_inner(
        &mut self,
        allocator: &mut Allocator,
        bindless: Option<&Bindless>,
    ) -> Result<(), RhiError> {
        let graph = &mut *self.graph;
        {
            engine_profiler::zone!("render::graph::compile");
            graph.records.cull();
            graph.records.resolve_lifetimes();
        }
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

        let d = &graph.device;
        let cmd = self.cmd;
        let slot = self.slot;
        if let Some(timing) = &mut graph.timing {
            let needed = graph.records.passes.iter().filter(|p| p.needed).count() as u32;
            // SAFETY: the slot's previous frame is done (`begin`); nothing is recorded yet
            // inside a render pass.
            unsafe { timing.begin_frame(d, cmd, slot, needed * 2) };
        }
        let physical = graph.physical.current();
        let mut stats = GraphStats {
            passes: graph.records.passes.len() as u32,
            transient_resources: realized.transient_resources,
            transient_heap_bytes: realized.heap_bytes,
            transient_placed_bytes: realized.placed_bytes,
            transient_unaliased_bytes: realized.unaliased_bytes,
            created_objects: realized.created_objects,
            ..GraphStats::default()
        };
        for (index, pass) in graph.records.passes.iter().enumerate() {
            if !pass.needed {
                stats.culled_passes += 1;
                continue;
            }
            if let Some(utils) = &graph.debug_utils {
                let mut name = [0u8; 64];
                let len = pass.name.len().min(63);
                name[..len].copy_from_slice(&pass.name.as_bytes()[..len]);
                let label = vk::DebugUtilsLabelEXT::default()
                    .label_name(core::ffi::CStr::from_bytes_until_nul(&name).unwrap_or(c"pass"));
                // SAFETY: recording command buffer; the label is copied by the call.
                unsafe { utils.cmd_begin_debug_utils_label(cmd, &label) };
            }
            let barriers = &graph.records.image_barriers
                [pass.image_barriers.start as usize..pass.image_barriers.end as usize];
            stats.image_barriers += barriers.len() as u32;
            stats.memory_barriers += u32::from(!pass.memory_barrier.is_empty());
            record_barriers(
                d,
                cmd,
                &mut graph.vk_image_barriers,
                barriers,
                &pass.memory_barrier,
                physical,
                &graph.textures,
            );

            if let Some(timing) = &mut graph.timing {
                // SAFETY: recording; queries were prepared by `begin_frame`.
                unsafe { timing.pass_begin(d, cmd, slot, pass.name) };
            }
            let attachments =
                &graph.attachments[pass.attachments.start as usize..pass.attachments.end as usize];
            let mut render_area = vk::Rect2D::default();
            if !attachments.is_empty() {
                render_area = begin_rendering(
                    d,
                    cmd,
                    attachments,
                    physical,
                    &graph.textures,
                    &mut graph.color_attachments,
                );
            }
            if let Some(callback) = graph.callbacks[index] {
                let mut ctx = PassContext {
                    cmd,
                    device: d,
                    physical,
                    render_area,
                };
                // SAFETY: the callback lives in the frame arena until `drop_callbacks`, and
                // nothing else references it.
                unsafe { (*callback.as_ptr())(&mut ctx) };
            }
            if !attachments.is_empty() {
                // SAFETY: matches the `cmd_begin_rendering` above.
                unsafe { d.cmd_end_rendering(cmd) };
            }
            if let Some(timing) = &mut graph.timing {
                // SAFETY: after `pass_begin` for this pass.
                unsafe { timing.pass_end(d, cmd, slot) };
            }
            if let Some(utils) = &graph.debug_utils {
                // SAFETY: matches the label begun above.
                unsafe { utils.cmd_end_debug_utils_label(cmd) };
            }
        }
        let finals = &graph.records.image_barriers[graph.records.final_image_barriers.start as usize
            ..graph.records.final_image_barriers.end as usize];
        stats.image_barriers += finals.len() as u32;
        stats.memory_barriers += u32::from(!graph.records.final_memory_barrier.is_empty());
        record_barriers(
            d,
            cmd,
            &mut graph.vk_image_barriers,
            finals,
            &graph.records.final_memory_barrier,
            physical,
            &graph.textures,
        );
        graph.stats = stats;
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

    /// Declares another use of a texture or buffer.
    pub fn read(mut self, resource: impl GraphResource, access: Access) -> Self {
        self.access(resource.id(), AccessInfo::of(access));
        self
    }

    /// Same as [`Self::read`]; reads more naturally for write accesses.
    pub fn write(self, resource: impl GraphResource, access: Access) -> Self {
        self.read(resource, access)
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

fn record_barriers(
    d: &ash::Device,
    cmd: vk::CommandBuffer,
    scratch: &mut Vec<vk::ImageMemoryBarrier2<'static>>,
    barriers: &[ImageBarrier],
    memory: &MemoryBarrier,
    physical: &[Physical],
    textures: &[TextureDesc],
) {
    if barriers.is_empty() && memory.is_empty() {
        return;
    }
    scratch.clear();
    scratch.extend(barriers.iter().map(|b| {
        let desc = &textures[b.resource as usize];
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(b.src_stages)
            .src_access_mask(b.src_access)
            .dst_stage_mask(b.dst_stages)
            .dst_access_mask(b.dst_access)
            .old_layout(b.old_layout)
            .new_layout(b.new_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(physical[b.resource as usize].image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect_of(desc.format),
                base_mip_level: 0,
                level_count: vk::REMAINING_MIP_LEVELS,
                base_array_layer: 0,
                layer_count: vk::REMAINING_ARRAY_LAYERS,
            })
    }));
    let memory_barrier = [vk::MemoryBarrier2::default()
        .src_stage_mask(memory.src_stages)
        .src_access_mask(memory.src_access)
        .dst_stage_mask(memory.dst_stages)
        .dst_access_mask(memory.dst_access)];
    let mut dependency = vk::DependencyInfo::default().image_memory_barriers(scratch);
    if !memory.is_empty() {
        dependency = dependency.memory_barriers(&memory_barrier);
    }
    // SAFETY: recording command buffer; every image belongs to the graph's frame.
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
        let store = match a.store {
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
        let info = vk::RenderingAttachmentInfo::default()
            .image_view(physical[a.resource as usize].view)
            .image_layout(if a.read_only {
                vk::ImageLayout::READ_ONLY_OPTIMAL
            } else {
                vk::ImageLayout::ATTACHMENT_OPTIMAL
            })
            .load_op(load)
            .store_op(store)
            .clear_value(clear_value);
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
