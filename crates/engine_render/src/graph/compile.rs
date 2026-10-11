//! Graph compilation without Vulkan calls: pass culling, resource lifetimes and usage,
//! and the barriers each pass needs (synchronization2), computed from declared accesses.
//!
//! Everything works on flat vectors owned by [`Records`], cleared every frame and reused,
//! so steady-state frames do not allocate.

use super::access::{AccessInfo, BufferState, TextureState};
use engine_rhi::ash::vk;
use std::ops::Range;

/// Access bits that write memory; only these belong in a barrier's source access mask.
const WRITE_ACCESS: vk::AccessFlags2 = vk::AccessFlags2::from_raw(
    vk::AccessFlags2::SHADER_WRITE.as_raw()
        | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw()
        | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE.as_raw()
        | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE.as_raw()
        | vk::AccessFlags2::TRANSFER_WRITE.as_raw()
        | vk::AccessFlags2::HOST_WRITE.as_raw()
        | vk::AccessFlags2::MEMORY_WRITE.as_raw(),
);

pub(crate) const UNUSED: u32 = u32::MAX;

/// The queue a pass runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum QueueKind {
    /// The graphics queue (graphics, compute and transfer).
    #[default]
    Graphics,
    /// The async compute queue: a compute queue of a family without graphics, running
    /// alongside the graphics queue (ADR 0065).
    AsyncCompute,
}

impl QueueKind {
    pub(crate) fn other(self) -> Self {
        match self {
            Self::Graphics => Self::AsyncCompute,
            Self::AsyncCompute => Self::Graphics,
        }
    }

    pub(crate) fn index(self) -> usize {
        match self {
            Self::Graphics => 0,
            Self::AsyncCompute => 1,
        }
    }
}

/// What a graph resource is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Texture,
    Buffer,
}

/// State of an imported resource at the graph's boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Boundary {
    pub(crate) layout: vk::ImageLayout,
    pub(crate) stages: vk::PipelineStageFlags2,
    pub(crate) access: vk::AccessFlags2,
}

impl From<TextureState> for Boundary {
    fn from(s: TextureState) -> Self {
        Self {
            layout: s.layout,
            stages: s.stages,
            access: s.access,
        }
    }
}

impl From<BufferState> for Boundary {
    fn from(s: BufferState) -> Self {
        Self {
            layout: vk::ImageLayout::UNDEFINED,
            stages: s.stages,
            access: s.access,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceRecord {
    pub(crate) name: &'static str,
    pub(crate) kind: Kind,
    /// `Some` for imported resources: the state before the graph.
    pub(crate) initial: Option<Boundary>,
    /// For imported resources: the state required after the graph, if any.
    pub(crate) final_state: Option<Boundary>,
    pub(crate) image_usage: vk::ImageUsageFlags,
    pub(crate) buffer_usage: vk::BufferUsageFlags,
    /// First and last needed pass using it, or `UNUSED`.
    pub(crate) first: u32,
    pub(crate) last: u32,
    /// Transient resources whose memory this one reuses (into `Records::aliases`).
    pub(crate) aliases: Range<u32>,
    /// Created with `VK_SHARING_MODE_CONCURRENT` for both queue families (a transient
    /// used on both queues, with concurrent sharing chosen): no ownership transfers.
    pub(crate) concurrent: bool,
    /// An acquired swapchain image: the caller's submission waits for it, so only the
    /// graph's last graphics segment (the caller's command buffer) may touch it.
    pub(crate) caller_waits: bool,
}

impl ResourceRecord {
    pub(crate) fn new(name: &'static str, kind: Kind) -> Self {
        Self {
            name,
            kind,
            initial: None,
            final_state: None,
            image_usage: vk::ImageUsageFlags::empty(),
            buffer_usage: vk::BufferUsageFlags::empty(),
            first: UNUSED,
            last: UNUSED,
            aliases: 0..0,
            concurrent: false,
            caller_waits: false,
        }
    }

    pub(crate) fn is_transient(&self) -> bool {
        self.initial.is_none()
    }

    pub(crate) fn is_used(&self) -> bool {
        self.first != UNUSED
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AccessRecord {
    pub(crate) resource: u32,
    pub(crate) info: AccessInfo,
}

/// An image barrier in resource terms (the image handle is looked up at execution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageBarrier {
    pub(crate) resource: u32,
    pub(crate) src_stages: vk::PipelineStageFlags2,
    pub(crate) src_access: vk::AccessFlags2,
    pub(crate) dst_stages: vk::PipelineStageFlags2,
    pub(crate) dst_access: vk::AccessFlags2,
    pub(crate) old_layout: vk::ImageLayout,
    pub(crate) new_layout: vk::ImageLayout,
    /// A queue family ownership transfer from the first queue to the second (the release
    /// half on the first queue, the acquire half on the second).
    pub(crate) transfer: Option<(QueueKind, QueueKind)>,
}

/// A buffer barrier: only for queue family ownership transfers (other buffer
/// dependencies use one global memory barrier per pass).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BufferBarrier {
    pub(crate) resource: u32,
    pub(crate) src_stages: vk::PipelineStageFlags2,
    pub(crate) src_access: vk::AccessFlags2,
    pub(crate) dst_stages: vk::PipelineStageFlags2,
    pub(crate) dst_access: vk::AccessFlags2,
    pub(crate) transfer: (QueueKind, QueueKind),
}

/// A global memory barrier (buffers need no per-resource barriers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemoryBarrier {
    pub(crate) src_stages: vk::PipelineStageFlags2,
    pub(crate) src_access: vk::AccessFlags2,
    pub(crate) dst_stages: vk::PipelineStageFlags2,
    pub(crate) dst_access: vk::AccessFlags2,
}

impl MemoryBarrier {
    fn add(&mut self, src: (vk::PipelineStageFlags2, vk::AccessFlags2), dst: &AccessInfo) {
        self.src_stages |= src.0;
        self.src_access |= src.1;
        self.dst_stages |= dst.stages;
        self.dst_access |= dst.access;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.src_stages.is_empty() && self.dst_stages.is_empty()
    }
}

#[derive(Debug)]
pub(crate) struct PassRecord {
    pub(crate) name: &'static str,
    /// Index into the frame's scopes, or [`NO_SCOPE`].
    pub(crate) scope: u32,
    pub(crate) accesses: Range<u32>,
    pub(crate) attachments: Range<u32>,
    pub(crate) side_effects: bool,
    pub(crate) needed: bool,
    /// Declared with `.async_compute()`.
    pub(crate) async_requested: bool,
    /// Declared with `.fence()`: runs after every pass declared before it and before
    /// every pass declared after it.
    pub(crate) fence: bool,
    /// Where it runs this frame (after scheduling).
    pub(crate) queue: QueueKind,
    /// Its segment (`Records::segments`) this frame.
    pub(crate) segment: u32,
    /// Passes it depends on (into `Records::edges`; with async compute only).
    pub(crate) preds: Range<u32>,
    pub(crate) image_barriers: Range<u32>,
    /// Ownership acquires of buffers (into `Records::buffer_barriers`).
    pub(crate) buffer_barriers: Range<u32>,
    pub(crate) memory_barrier: MemoryBarrier,
}

impl PassRecord {
    pub(crate) fn new(
        name: &'static str,
        scope: u32,
        accesses_start: u32,
        attachments_start: u32,
    ) -> Self {
        Self {
            name,
            scope,
            accesses: accesses_start..accesses_start,
            attachments: attachments_start..attachments_start,
            side_effects: false,
            needed: false,
            async_requested: false,
            fence: false,
            queue: QueueKind::Graphics,
            segment: 0,
            preds: 0..0,
            image_barriers: 0..0,
            buffer_barriers: 0..0,
            memory_barrier: MemoryBarrier::default(),
        }
    }
}

/// [`PassRecord::scope`] of passes declared outside any scope.
pub(crate) const NO_SCOPE: u32 = u32::MAX;

/// Synchronization state of one resource while walking the passes.
#[derive(Debug, Clone, Copy, Default)]
struct State {
    touched: bool,
    layout: vk::ImageLayout,
    /// Stages and write accesses of the last write (or layout transition).
    write_stages: vk::PipelineStageFlags2,
    write_access: vk::AccessFlags2,
    /// Stages that read since the last write (sources for write-after-read).
    read_stages: vk::PipelineStageFlags2,
    /// Stages and accesses the last write is already visible to.
    visible_stages: vk::PipelineStageFlags2,
    visible_access: vk::AccessFlags2,
    /// The queue that accessed it last, and the segment of that access (or
    /// [`NO_SEGMENT`] for an imported resource not used yet: the graphics queue owns it).
    queue: QueueKind,
    segment: u32,
}

/// [`Segment::wait`] of a segment that waits for nothing, and the segment of resources
/// not used yet.
pub(crate) const NO_SEGMENT: u32 = u32::MAX;

/// A run of passes on one queue submitted as one batch: it waits for a segment of the
/// other queue (a cross-queue dependency) before it starts, and its end is signaled.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub(crate) queue: QueueKind,
    /// Its passes, in execution order (into `Records::segment_passes`).
    pub(crate) passes: Range<u32>,
    /// The segment of the other queue it waits for, or [`NO_SEGMENT`].
    pub(crate) wait: u32,
    /// Position (in `Records::order`) where it starts.
    pub(crate) first_position: u32,
    /// For async compute segments: the position of the first graphics pass that waits
    /// for it (or the end of the frame). Graphics passes before it may run concurrently.
    pub(crate) join_position: u32,
    /// Barriers recorded after its last pass: releases of ownership and layout
    /// transitions for the other queue (into `Records::release_images` / `_buffers`).
    pub(crate) release_images: Range<u32>,
    pub(crate) release_buffers: Range<u32>,
}

impl Segment {
    pub(crate) fn new(queue: QueueKind, wait: u32, position: u32) -> Self {
        Self {
            queue,
            passes: 0..0,
            wait,
            first_position: position,
            join_position: UNUSED,
            release_images: 0..0,
            release_buffers: 0..0,
        }
    }
}

/// Per-frame graph data, reused across frames.
#[derive(Debug, Default)]
pub(crate) struct Records {
    pub(crate) resources: Vec<ResourceRecord>,
    pub(crate) passes: Vec<PassRecord>,
    pub(crate) accesses: Vec<AccessRecord>,
    pub(crate) image_barriers: Vec<ImageBarrier>,
    /// Ownership acquires of buffers (async compute only).
    pub(crate) buffer_barriers: Vec<BufferBarrier>,
    /// Transitions of imported textures to their final states, after the last pass.
    pub(crate) final_image_barriers: Range<u32>,
    pub(crate) final_buffer_barriers: Range<u32>,
    pub(crate) final_memory_barrier: MemoryBarrier,
    pub(crate) aliases: Vec<u32>,
    /// Needed passes in execution order (declaration order without async compute).
    pub(crate) order: Vec<u32>,
    /// Batches per queue (one graphics segment without async compute).
    pub(crate) segments: Vec<Segment>,
    pub(crate) segment_passes: Vec<u32>,
    /// Barriers at the ends of segments, grouped by segment.
    pub(crate) release_images: Vec<ImageBarrier>,
    pub(crate) release_buffers: Vec<BufferBarrier>,
    /// Dependencies between passes (async compute scheduling): each pass's are
    /// `PassRecord::preds`.
    pub(crate) edges: Vec<u32>,
    pub(crate) schedule_scratch: super::schedule::Scratch,
    needed: Vec<bool>,
    states: Vec<State>,
    pending_images: Vec<(u32, u32, ImageBarrier)>,
    pending_buffers: Vec<(u32, u32, BufferBarrier)>,
}

impl Records {
    pub(crate) fn clear(&mut self) {
        self.resources.clear();
        self.passes.clear();
        self.accesses.clear();
        self.image_barriers.clear();
        self.buffer_barriers.clear();
        self.final_image_barriers = 0..0;
        self.final_buffer_barriers = 0..0;
        self.final_memory_barrier = MemoryBarrier::default();
        self.aliases.clear();
        self.order.clear();
        self.segments.clear();
        self.segment_passes.clear();
        self.release_images.clear();
        self.release_buffers.clear();
        self.edges.clear();
    }

    /// Adds an access by the last pass, merging with an earlier access of the same
    /// resource by that pass.
    pub(crate) fn add_access(&mut self, resource: u32, info: AccessInfo) {
        let Some(pass) = self.passes.last_mut() else {
            return;
        };
        let range = pass.accesses.start as usize..self.accesses.len();
        if let Some(existing) = self.accesses[range]
            .iter_mut()
            .find(|a| a.resource == resource)
        {
            existing.info = existing.info.merge(info);
            return;
        }
        self.accesses.push(AccessRecord { resource, info });
        pass.accesses.end = self.accesses.len() as u32;
    }

    /// Marks the passes that contribute to an imported resource or have side effects.
    /// Walking backwards: a pass is needed if it writes something a later needed pass
    /// reads (or an imported resource); a write that discards the old contents ends the
    /// need for earlier writers.
    pub(crate) fn cull(&mut self) {
        let Self {
            resources,
            passes,
            accesses,
            needed,
            ..
        } = self;
        needed.clear();
        needed.extend(resources.iter().map(|r| !r.is_transient()));
        for pass in passes.iter_mut().rev() {
            let range = pass.accesses.start as usize..pass.accesses.end as usize;
            pass.needed = pass.side_effects
                || accesses[range.clone()]
                    .iter()
                    .any(|a| a.info.writes && needed[a.resource as usize]);
            if !pass.needed {
                continue;
            }
            for a in &accesses[range.clone()] {
                let r = a.resource as usize;
                if a.info.discards && !a.info.reads && resources[r].is_transient() {
                    needed[r] = false;
                }
            }
            for a in &accesses[range] {
                if a.info.reads {
                    needed[a.resource as usize] = true;
                }
            }
        }
    }

    /// First/last position (in [`Self::order`]) and usage flags of every resource. Call
    /// after scheduling. A resource used by an async compute segment lives over that
    /// segment's whole window (from its start to the graphics pass that waits for it):
    /// graphics passes in the window may run at the same time, so its memory must not be
    /// shared with theirs.
    pub(crate) fn resolve_lifetimes(&mut self) {
        for r in &mut self.resources {
            r.first = UNUSED;
            r.last = UNUSED;
            r.image_usage = vk::ImageUsageFlags::empty();
            r.buffer_usage = vk::BufferUsageFlags::empty();
        }
        for (position, &p) in self.order.iter().enumerate() {
            let pass = &self.passes[p as usize];
            for a in &self.accesses[pass.accesses.start as usize..pass.accesses.end as usize] {
                let r = &mut self.resources[a.resource as usize];
                if r.first == UNUSED {
                    r.first = position as u32;
                }
                r.last = position as u32;
                r.image_usage |= a.info.image_usage;
                r.buffer_usage |= a.info.buffer_usage;
            }
        }
        for &p in &self.order {
            let pass = &self.passes[p as usize];
            if pass.queue != QueueKind::AsyncCompute {
                continue;
            }
            let segment = &self.segments[pass.segment as usize];
            let (start, end) = (
                segment.first_position,
                segment.join_position.saturating_sub(1),
            );
            for a in &self.accesses[pass.accesses.start as usize..pass.accesses.end as usize] {
                let r = &mut self.resources[a.resource as usize];
                r.first = r.first.min(start);
                r.last = r.last.max(end);
            }
        }
    }

    /// Computes the barriers before every needed pass, at the ends of segments, and the
    /// final transitions. Call after [`Self::resolve_lifetimes`] and after aliases are
    /// known.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn build_barriers(&mut self) {
        let Self {
            resources,
            passes,
            accesses,
            image_barriers,
            buffer_barriers,
            final_image_barriers,
            final_buffer_barriers,
            final_memory_barrier,
            aliases,
            states,
            order,
            segments,
            release_images,
            release_buffers,
            pending_images,
            pending_buffers,
            ..
        } = self;
        image_barriers.clear();
        buffer_barriers.clear();
        pending_images.clear();
        pending_buffers.clear();
        states.clear();
        states.resize(resources.len(), State::default());
        let mut sequence = 0u32;

        for &p in order.iter() {
            let pass = &mut passes[p as usize];
            let (queue, segment) = (pass.queue, pass.segment);
            let start = image_barriers.len() as u32;
            let buffer_start = buffer_barriers.len() as u32;
            let mut memory = MemoryBarrier::default();
            for a in &accesses[pass.accesses.start as usize..pass.accesses.end as usize] {
                let r = a.resource as usize;
                let resource = &resources[r];
                let is_image = resource.kind == Kind::Texture;
                let info = &a.info;
                if !states[r].touched {
                    states[r].touched = true;
                    states[r].queue = QueueKind::Graphics;
                    states[r].segment = NO_SEGMENT;
                    if let Some(initial) = resource.initial {
                        let s = &mut states[r];
                        s.layout = initial.layout;
                        let writes = initial.access & WRITE_ACCESS;
                        if writes.is_empty() {
                            s.read_stages = initial.stages;
                        } else {
                            s.write_stages = initial.stages;
                            s.write_access = writes;
                        }
                    } else {
                        // Transient: nothing to keep, but the memory's previous
                        // occupants must be done with it. Those on the other queue are:
                        // a semaphore orders their segment before this one (their
                        // lifetimes cover their segments' windows), and the barrier
                        // chains on the wait.
                        let (mut src_stages, mut src_access) = Default::default();
                        for &p in
                            &aliases[resource.aliases.start as usize..resource.aliases.end as usize]
                        {
                            let p = &states[p as usize];
                            if p.queue == queue {
                                src_stages |= p.write_stages | p.read_stages;
                                src_access |= p.write_access;
                            } else {
                                src_stages |= info.stages;
                            }
                        }
                        if is_image {
                            image_barriers.push(ImageBarrier {
                                resource: a.resource,
                                src_stages,
                                src_access,
                                dst_stages: info.stages,
                                dst_access: info.access,
                                old_layout: vk::ImageLayout::UNDEFINED,
                                new_layout: info.layout,
                                transfer: None,
                            });
                        } else if !src_stages.is_empty() {
                            memory.add((src_stages, src_access), info);
                        }
                        let s = &mut states[r];
                        after_access(s, info);
                        s.queue = queue;
                        s.segment = segment;
                        continue;
                    }
                }
                if states[r].queue != queue {
                    let s = states[r];
                    let from = s.queue;
                    let keep = !info.discards;
                    let exclusive = !resource.concurrent;
                    // The other queue's segment that last used it (for an imported
                    // resource first used on the compute queue: the graphics segment this
                    // one waits for). The scheduler made this segment wait for it.
                    let releasing = if s.segment == NO_SEGMENT {
                        segments[segment as usize].wait
                    } else {
                        s.segment
                    };
                    let src = (s.write_stages | s.read_stages, s.write_access);
                    let mut transitioned_here = false;
                    if is_image {
                        if keep && exclusive {
                            // Queue family ownership transfer: released there, acquired
                            // here, the layout transition between.
                            let transfer = Some((from, queue));
                            pending_images.push((
                                releasing,
                                sequence,
                                ImageBarrier {
                                    resource: a.resource,
                                    src_stages: src.0,
                                    src_access: src.1,
                                    dst_stages: vk::PipelineStageFlags2::NONE,
                                    dst_access: vk::AccessFlags2::NONE,
                                    old_layout: s.layout,
                                    new_layout: info.layout,
                                    transfer,
                                },
                            ));
                            sequence += 1;
                            image_barriers.push(ImageBarrier {
                                resource: a.resource,
                                src_stages: info.stages,
                                src_access: vk::AccessFlags2::NONE,
                                dst_stages: info.stages,
                                dst_access: info.access,
                                old_layout: s.layout,
                                new_layout: info.layout,
                                transfer,
                            });
                            transitioned_here = s.layout != info.layout;
                        } else if keep && s.layout != info.layout {
                            // Concurrent sharing: the queue that used it last transitions
                            // it before signaling. Its own later passes may read it too
                            // without waiting for the other queue: visible to everything.
                            pending_images.push((
                                releasing,
                                sequence,
                                ImageBarrier {
                                    resource: a.resource,
                                    src_stages: src.0,
                                    src_access: src.1,
                                    dst_stages: vk::PipelineStageFlags2::ALL_COMMANDS,
                                    dst_access: vk::AccessFlags2::MEMORY_READ
                                        | vk::AccessFlags2::MEMORY_WRITE,
                                    old_layout: s.layout,
                                    new_layout: info.layout,
                                    transfer: None,
                                },
                            ));
                            sequence += 1;
                        } else if !keep {
                            // Contents do not matter: no ownership to transfer.
                            image_barriers.push(ImageBarrier {
                                resource: a.resource,
                                src_stages: info.stages,
                                src_access: vk::AccessFlags2::NONE,
                                dst_stages: info.stages,
                                dst_access: info.access,
                                old_layout: vk::ImageLayout::UNDEFINED,
                                new_layout: info.layout,
                                transfer: None,
                            });
                            transitioned_here = true;
                        }
                    } else if keep && exclusive {
                        let transfer = (from, queue);
                        pending_buffers.push((
                            releasing,
                            sequence,
                            BufferBarrier {
                                resource: a.resource,
                                src_stages: src.0,
                                src_access: src.1,
                                dst_stages: vk::PipelineStageFlags2::NONE,
                                dst_access: vk::AccessFlags2::NONE,
                                transfer,
                            },
                        ));
                        sequence += 1;
                        buffer_barriers.push(BufferBarrier {
                            resource: a.resource,
                            src_stages: info.stages,
                            src_access: vk::AccessFlags2::NONE,
                            dst_stages: info.stages,
                            dst_access: info.access,
                            transfer,
                        });
                    }
                    // The semaphore made the other queue's writes visible to everything
                    // after the wait: later reads here need no barrier, except after a
                    // layout transition recorded on this queue.
                    let s = &mut states[r];
                    if info.writes {
                        after_access(s, info);
                    } else {
                        s.layout = info.layout;
                        s.write_stages = if transitioned_here {
                            info.stages
                        } else {
                            vk::PipelineStageFlags2::empty()
                        };
                        s.write_access = vk::AccessFlags2::empty();
                        s.read_stages = info.stages;
                        s.visible_stages = info.stages;
                        s.visible_access = info.access;
                    }
                    s.queue = queue;
                    s.segment = segment;
                    continue;
                }
                let s = &mut states[r];
                s.segment = segment;
                let layout_change = is_image && s.layout != info.layout;
                if layout_change || info.writes {
                    let src = (s.write_stages | s.read_stages, s.write_access);
                    if is_image && (layout_change || !src.0.is_empty()) {
                        image_barriers.push(ImageBarrier {
                            resource: a.resource,
                            src_stages: src.0,
                            src_access: src.1,
                            dst_stages: info.stages,
                            dst_access: info.access,
                            old_layout: if info.discards {
                                vk::ImageLayout::UNDEFINED
                            } else {
                                s.layout
                            },
                            new_layout: info.layout,
                            transfer: None,
                        });
                    } else if !is_image && !src.0.is_empty() {
                        memory.add(src, info);
                    }
                    after_access(s, info);
                } else {
                    // Read in the current layout: wait for the last write unless it is
                    // already visible to these stages.
                    let covered = s.visible_stages.contains(info.stages)
                        && s.visible_access.contains(info.access);
                    if !s.write_stages.is_empty() && !covered {
                        let src = (s.write_stages, s.write_access);
                        if is_image {
                            image_barriers.push(ImageBarrier {
                                resource: a.resource,
                                src_stages: src.0,
                                src_access: src.1,
                                dst_stages: info.stages,
                                dst_access: info.access,
                                old_layout: s.layout,
                                new_layout: s.layout,
                                transfer: None,
                            });
                        } else {
                            memory.add(src, info);
                        }
                        s.visible_stages |= info.stages;
                        s.visible_access |= info.access;
                    }
                    s.read_stages |= info.stages;
                }
            }
            pass.image_barriers = start..image_barriers.len() as u32;
            pass.buffer_barriers = buffer_start..buffer_barriers.len() as u32;
            pass.memory_barrier = memory;
        }

        // Imported resources leave the graph in the state their owner asked for, owned by
        // the graphics queue (the last graphics segment waits for every compute segment).
        let start = image_barriers.len() as u32;
        let buffer_start = buffer_barriers.len() as u32;
        let mut memory = MemoryBarrier::default();
        for (r, resource) in resources.iter().enumerate() {
            let s = states[r];
            if !s.touched {
                continue;
            }
            if s.queue == QueueKind::AsyncCompute && resource.initial.is_some() {
                let target = resource.final_state.unwrap_or(Boundary {
                    layout: s.layout,
                    stages: vk::PipelineStageFlags2::ALL_COMMANDS,
                    access: vk::AccessFlags2::NONE,
                });
                let dst_stages = if target.stages.is_empty() {
                    vk::PipelineStageFlags2::ALL_COMMANDS
                } else {
                    target.stages
                };
                let src = (s.write_stages | s.read_stages, s.write_access);
                let transfer = (QueueKind::AsyncCompute, QueueKind::Graphics);
                if resource.kind == Kind::Texture {
                    pending_images.push((
                        s.segment,
                        sequence,
                        ImageBarrier {
                            resource: r as u32,
                            src_stages: src.0,
                            src_access: src.1,
                            dst_stages: vk::PipelineStageFlags2::NONE,
                            dst_access: vk::AccessFlags2::NONE,
                            old_layout: s.layout,
                            new_layout: target.layout,
                            transfer: Some(transfer),
                        },
                    ));
                    image_barriers.push(ImageBarrier {
                        resource: r as u32,
                        src_stages: vk::PipelineStageFlags2::ALL_COMMANDS,
                        src_access: vk::AccessFlags2::NONE,
                        dst_stages,
                        dst_access: target.access,
                        old_layout: s.layout,
                        new_layout: target.layout,
                        transfer: Some(transfer),
                    });
                } else {
                    pending_buffers.push((
                        s.segment,
                        sequence,
                        BufferBarrier {
                            resource: r as u32,
                            src_stages: src.0,
                            src_access: src.1,
                            dst_stages: vk::PipelineStageFlags2::NONE,
                            dst_access: vk::AccessFlags2::NONE,
                            transfer,
                        },
                    ));
                    buffer_barriers.push(BufferBarrier {
                        resource: r as u32,
                        src_stages: vk::PipelineStageFlags2::ALL_COMMANDS,
                        src_access: vk::AccessFlags2::NONE,
                        dst_stages,
                        dst_access: target.access,
                        transfer,
                    });
                }
                sequence += 1;
                continue;
            }
            let Some(target) = resource.final_state else {
                continue;
            };
            let src = (s.write_stages | s.read_stages, s.write_access);
            if resource.kind == Kind::Texture {
                if s.layout != target.layout || !s.write_access.is_empty() {
                    image_barriers.push(ImageBarrier {
                        resource: r as u32,
                        src_stages: src.0,
                        src_access: src.1,
                        dst_stages: target.stages,
                        dst_access: target.access,
                        old_layout: s.layout,
                        new_layout: target.layout,
                        transfer: None,
                    });
                }
            } else if !s.write_access.is_empty() {
                memory.src_stages |= src.0;
                memory.src_access |= src.1;
                memory.dst_stages |= target.stages;
                memory.dst_access |= target.access;
            }
        }
        *final_image_barriers = start..image_barriers.len() as u32;
        *final_buffer_barriers = buffer_start..buffer_barriers.len() as u32;
        *final_memory_barrier = memory;

        // The releases, grouped by segment in the order they were found.
        pending_images.sort_unstable_by_key(|&(segment, sequence, _)| (segment, sequence));
        pending_buffers.sort_unstable_by_key(|&(segment, sequence, _)| (segment, sequence));
        release_images.clear();
        release_buffers.clear();
        let (mut i, mut b) = (0, 0);
        for (index, segment) in segments.iter_mut().enumerate() {
            let index = index as u32;
            let start = release_images.len() as u32;
            while i < pending_images.len() && pending_images[i].0 == index {
                release_images.push(pending_images[i].2);
                i += 1;
            }
            segment.release_images = start..release_images.len() as u32;
            let start = release_buffers.len() as u32;
            while b < pending_buffers.len() && pending_buffers[b].0 == index {
                release_buffers.push(pending_buffers[b].2);
                b += 1;
            }
            segment.release_buffers = start..release_buffers.len() as u32;
        }
    }
}

/// State after an access that needed (or started with) a barrier.
fn after_access(s: &mut State, info: &AccessInfo) {
    s.layout = info.layout;
    // A layout transition is a write too: later accesses chain on this pass's stages.
    s.write_stages = info.stages;
    s.write_access = info.access & WRITE_ACCESS;
    s.read_stages = if info.reads && !info.writes {
        info.stages
    } else {
        vk::PipelineStageFlags2::empty()
    };
    // What this access reads is visible to it; what it writes is visible to no one yet,
    // so a later read (even by the same stages) waits for it. A read-write access used to
    // count its own writes as visible, and a read after it went without a barrier.
    if info.writes {
        s.visible_stages = vk::PipelineStageFlags2::empty();
        s.visible_access = vk::AccessFlags2::empty();
    } else {
        s.visible_stages = info.stages;
        s.visible_access = info.access;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::access::{Access, ShaderStages};
    use crate::graph::schedule::ScheduleOptions;

    fn texture(records: &mut Records, name: &'static str) -> u32 {
        records
            .resources
            .push(ResourceRecord::new(name, Kind::Texture));
        (records.resources.len() - 1) as u32
    }

    fn buffer(records: &mut Records, name: &'static str) -> u32 {
        records
            .resources
            .push(ResourceRecord::new(name, Kind::Buffer));
        (records.resources.len() - 1) as u32
    }

    fn import(records: &mut Records, r: u32, initial: TextureState, final_state: TextureState) {
        records.resources[r as usize].initial = Some(initial.into());
        records.resources[r as usize].final_state = Some(final_state.into());
    }

    fn pass(records: &mut Records, name: &'static str, accesses: &[(u32, AccessInfo)]) -> usize {
        let start = records.accesses.len() as u32;
        records
            .passes
            .push(PassRecord::new(name, NO_SCOPE, start, 0));
        for &(r, info) in accesses {
            records.add_access(r, info);
        }
        records.passes.len() - 1
    }

    fn compile(records: &mut Records) {
        records.cull();
        records.schedule(ScheduleOptions::default());
        records.resolve_lifetimes();
        records.build_barriers();
    }

    fn barriers(records: &Records, pass: usize) -> &[ImageBarrier] {
        let r = &records.passes[pass].image_barriers;
        &records.image_barriers[r.start as usize..r.end as usize]
    }

    fn fragment_read() -> AccessInfo {
        AccessInfo::of(Access::Sampled(ShaderStages::FRAGMENT))
    }

    #[test]
    fn unused_passes_are_culled_and_imports_keep_their_writers() {
        let mut rec = Records::default();
        let color = texture(&mut rec, "color");
        let unused = texture(&mut rec, "unused");
        let backbuffer = texture(&mut rec, "backbuffer");
        import(
            &mut rec,
            backbuffer,
            TextureState::acquired_swapchain_image(),
            TextureState::present(),
        );
        let scene = pass(
            &mut rec,
            "scene",
            &[(color, AccessInfo::color_attachment(false))],
        );
        let dead = pass(
            &mut rec,
            "dead",
            &[(unused, AccessInfo::color_attachment(false))],
        );
        let post = pass(
            &mut rec,
            "post",
            &[
                (color, fragment_read()),
                (backbuffer, AccessInfo::color_attachment(false)),
            ],
        );
        compile(&mut rec);
        assert!(rec.passes[scene].needed);
        assert!(!rec.passes[dead].needed);
        assert!(rec.passes[post].needed);
        assert!(!rec.resources[unused as usize].is_used());
        assert_eq!(
            (
                rec.resources[color as usize].first,
                rec.resources[color as usize].last
            ),
            // Positions in the execution order: the culled pass is not in it.
            (0, 1)
        );
        assert_eq!(
            rec.resources[color as usize].image_usage,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED
        );
    }

    #[test]
    fn overwritten_results_do_not_keep_their_writers_alive() {
        let mut rec = Records::default();
        let target = texture(&mut rec, "target");
        import(
            &mut rec,
            target,
            TextureState::acquired_swapchain_image(),
            TextureState::present(),
        );
        let t = texture(&mut rec, "t");
        let first = pass(
            &mut rec,
            "first",
            &[(t, AccessInfo::color_attachment(false))],
        );
        // Clears `t` again: whatever `first` wrote is never read.
        let second = pass(
            &mut rec,
            "second",
            &[(t, AccessInfo::color_attachment(false))],
        );
        let third = pass(
            &mut rec,
            "third",
            &[
                (t, fragment_read()),
                (target, AccessInfo::color_attachment(false)),
            ],
        );
        compile(&mut rec);
        assert!(!rec.passes[first].needed);
        assert!(rec.passes[second].needed && rec.passes[third].needed);
    }

    #[test]
    fn barriers_for_render_then_sample_then_present() {
        let mut rec = Records::default();
        let color = texture(&mut rec, "color");
        let backbuffer = texture(&mut rec, "backbuffer");
        import(
            &mut rec,
            backbuffer,
            TextureState::acquired_swapchain_image(),
            TextureState::present(),
        );
        let scene = pass(
            &mut rec,
            "scene",
            &[(color, AccessInfo::color_attachment(false))],
        );
        let post = pass(
            &mut rec,
            "post",
            &[
                (color, fragment_read()),
                (backbuffer, AccessInfo::color_attachment(false)),
            ],
        );
        compile(&mut rec);

        // Transient first use: UNDEFINED → attachment, nothing to wait for.
        let b = barriers(&rec, scene);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].old_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(b[0].new_layout, vk::ImageLayout::ATTACHMENT_OPTIMAL);
        assert!(b[0].src_stages.is_empty());

        // Render → sample: waits for color output, transitions to read-only.
        let b = barriers(&rec, post);
        let sample = b.iter().find(|b| b.resource == color).unwrap();
        assert_eq!(
            sample.src_stages,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT
        );
        assert_eq!(sample.src_access, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE);
        assert_eq!(sample.dst_stages, vk::PipelineStageFlags2::FRAGMENT_SHADER);
        assert_eq!(sample.new_layout, vk::ImageLayout::READ_ONLY_OPTIMAL);
        // The swapchain image chains on the acquire semaphore's stage and is discarded.
        let target = b.iter().find(|b| b.resource == backbuffer).unwrap();
        assert_eq!(
            target.src_stages,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT
        );
        assert_eq!(target.old_layout, vk::ImageLayout::UNDEFINED);

        // Then to PRESENT_SRC after the last pass.
        let f = &rec.image_barriers
            [rec.final_image_barriers.start as usize..rec.final_image_barriers.end as usize];
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].new_layout, vk::ImageLayout::PRESENT_SRC_KHR);
        assert_eq!(f[0].src_access, vk::AccessFlags2::COLOR_ATTACHMENT_WRITE);
    }

    #[test]
    fn repeated_reads_need_one_barrier_and_writes_wait_for_reads() {
        let mut rec = Records::default();
        let t = texture(&mut rec, "t");
        let out = texture(&mut rec, "out");
        import(
            &mut rec,
            out,
            TextureState::acquired_swapchain_image(),
            TextureState::present(),
        );
        let compute_write = AccessInfo::of(Access::StorageWrite(ShaderStages::COMPUTE));
        let compute_read = AccessInfo::of(Access::StorageRead(ShaderStages::COMPUTE));
        let w = pass(&mut rec, "write", &[(t, compute_write)]);
        let r1 = pass(
            &mut rec,
            "read1",
            &[(t, compute_read), (out, compute_write)],
        );
        let r2 = pass(
            &mut rec,
            "read2",
            &[(t, compute_read), (out, compute_write)],
        );
        let w2 = pass(
            &mut rec,
            "write again",
            &[(t, compute_write), (out, compute_write)],
        );
        compile(&mut rec);
        assert_eq!(barriers(&rec, w).len(), 1);
        // read1: RAW on t (GENERAL, no transition).
        let b1: Vec<_> = barriers(&rec, r1)
            .iter()
            .filter(|b| b.resource == t)
            .collect();
        assert_eq!(b1.len(), 1);
        assert_eq!(b1[0].src_access, vk::AccessFlags2::SHADER_STORAGE_WRITE);
        // read2: already visible to compute storage reads.
        assert!(barriers(&rec, r2).iter().all(|b| b.resource != t));
        // write again: WAR on the reads (execution dependency, no source access).
        let b3: Vec<_> = barriers(&rec, w2)
            .iter()
            .filter(|b| b.resource == t)
            .collect();
        assert_eq!(b3.len(), 1);
        assert!(
            b3[0]
                .src_stages
                .contains(vk::PipelineStageFlags2::COMPUTE_SHADER)
        );
    }

    #[test]
    fn buffers_use_global_memory_barriers_and_final_host_reads() {
        let mut rec = Records::default();
        let data = buffer(&mut rec, "data");
        let readback = buffer(&mut rec, "readback");
        rec.resources[readback as usize].initial = Some(BufferState::idle().into());
        rec.resources[readback as usize].final_state = Some(BufferState::host_read().into());
        let fill = pass(
            &mut rec,
            "fill",
            &[(
                data,
                AccessInfo::of(Access::StorageWrite(ShaderStages::COMPUTE)),
            )],
        );
        let copy = pass(
            &mut rec,
            "copy",
            &[
                (data, AccessInfo::of(Access::TransferRead)),
                (readback, AccessInfo::of(Access::TransferWrite)),
            ],
        );
        compile(&mut rec);
        assert!(rec.passes[fill].memory_barrier.is_empty());
        let m = rec.passes[copy].memory_barrier;
        assert_eq!(m.src_access, vk::AccessFlags2::SHADER_STORAGE_WRITE);
        assert_eq!(m.dst_stages, vk::PipelineStageFlags2::ALL_TRANSFER);
        assert_eq!(
            rec.final_memory_barrier.dst_access,
            vk::AccessFlags2::HOST_READ
        );
        assert_eq!(
            rec.final_memory_barrier.src_access,
            vk::AccessFlags2::TRANSFER_WRITE
        );
    }

    #[test]
    fn a_read_after_a_read_write_access_waits_for_its_writes() {
        // Atomics into a histogram, then a pass that reads it, both compute.
        let mut rec = Records::default();
        let data = buffer(&mut rec, "histogram");
        // Imported, so the passes are kept.
        rec.resources[data as usize].initial = Some(BufferState::idle().into());
        rec.resources[data as usize].final_state = Some(BufferState::host_read().into());
        let _count = pass(
            &mut rec,
            "count",
            &[(
                data,
                AccessInfo::of(Access::StorageReadWrite(ShaderStages::COMPUTE)),
            )],
        );
        let average = pass(
            &mut rec,
            "average",
            &[(
                data,
                AccessInfo::of(Access::StorageRead(ShaderStages::COMPUTE)),
            )],
        );
        let again = pass(
            &mut rec,
            "again",
            &[(
                data,
                AccessInfo::of(Access::StorageRead(ShaderStages::COMPUTE)),
            )],
        );
        // The readers write outside the graph (as the exposure pass does).
        rec.passes[average].side_effects = true;
        rec.passes[again].side_effects = true;
        compile(&mut rec);
        let m = rec.passes[average].memory_barrier;
        assert!(
            m.src_access
                .contains(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        );
        assert!(m.dst_access.contains(vk::AccessFlags2::SHADER_STORAGE_READ));
        // The second reader is covered by the first one's barrier.
        assert!(rec.passes[again].memory_barrier.is_empty());
    }

    #[test]
    fn aliased_memory_waits_for_the_previous_occupant() {
        let mut rec = Records::default();
        let a = texture(&mut rec, "a");
        let b = texture(&mut rec, "b");
        let out = texture(&mut rec, "out");
        import(
            &mut rec,
            out,
            TextureState::acquired_swapchain_image(),
            TextureState::present(),
        );
        let _pa = pass(
            &mut rec,
            "make a",
            &[(a, AccessInfo::color_attachment(false))],
        );
        let _ua = pass(
            &mut rec,
            "use a",
            &[
                (a, fragment_read()),
                (out, AccessInfo::color_attachment(false)),
            ],
        );
        let pb = pass(
            &mut rec,
            "make b",
            &[(b, AccessInfo::color_attachment(false))],
        );
        let _ub = pass(
            &mut rec,
            "use b",
            &[
                (b, fragment_read()),
                (out, AccessInfo::color_attachment(true)),
            ],
        );
        rec.cull();
        rec.schedule(ScheduleOptions::default());
        rec.resolve_lifetimes();
        // `b` reuses `a`'s memory.
        rec.aliases.push(a);
        rec.resources[b as usize].aliases = 0..1;
        rec.build_barriers();
        let first_b = barriers(&rec, pb).iter().find(|x| x.resource == b).unwrap();
        assert_eq!(first_b.old_layout, vk::ImageLayout::UNDEFINED);
        assert!(
            first_b
                .src_stages
                .contains(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        );
    }

    #[test]
    fn steady_state_compilation_reuses_memory() {
        let mut rec = Records::default();
        let build = |rec: &mut Records| {
            rec.clear();
            let backbuffer = texture(rec, "backbuffer");
            import(
                rec,
                backbuffer,
                TextureState::acquired_swapchain_image(),
                TextureState::present(),
            );
            let mut previous = texture(rec, "t0");
            pass(
                rec,
                "p0",
                &[(previous, AccessInfo::color_attachment(false))],
            );
            for _ in 1..64 {
                let next = texture(rec, "t");
                pass(
                    rec,
                    "p",
                    &[
                        (previous, fragment_read()),
                        (next, AccessInfo::color_attachment(false)),
                    ],
                );
                previous = next;
            }
            pass(
                rec,
                "final",
                &[
                    (previous, fragment_read()),
                    (backbuffer, AccessInfo::color_attachment(false)),
                ],
            );
            compile(rec);
        };
        build(&mut rec);
        let capacities = (
            rec.resources.capacity(),
            rec.passes.capacity(),
            rec.accesses.capacity(),
            rec.image_barriers.capacity(),
        );
        for _ in 0..10 {
            build(&mut rec);
        }
        assert_eq!(
            capacities,
            (
                rec.resources.capacity(),
                rec.passes.capacity(),
                rec.accesses.capacity(),
                rec.image_barriers.capacity(),
            )
        );
        assert!(rec.passes.iter().all(|p| p.needed));
    }
}
