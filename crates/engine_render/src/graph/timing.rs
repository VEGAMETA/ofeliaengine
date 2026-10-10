//! GPU time per pass (CLAUDE.md §14: GPU passes per viewport in the editor profiler,
//! Tracy GPU zones), on each queue the graph uses, and the frame's GPU time end to end.
//!
//! Each queue has a timestamp query pool per frame slot. Every needed pass is bracketed by
//! two timestamps; when the slot comes around again (its previous frame has finished), the
//! results are read without waiting, turned into per-pass GPU times and, in profiling
//! builds, uploaded to Tracy as GPU zones. On the graphics queue two more timestamps open
//! and close the graph's frame: with async compute, passes overlap, and only the frame's
//! own span tells what the overlap saved.
//!
//! A pass's opening timestamp is taken at the top of the pipe: with no barrier before the
//! pass, the GPU writes it while the previous pass still runs, and the span to the closing
//! timestamp holds that pass's tail (a 49 µs pass after the tone mapper read 2.4 ms on the
//! Radeon, core review 2). A pass's time therefore starts at its opening timestamp or at
//! the previous pass's closing one on the same queue, whichever is later: the times
//! partition the queue's busy time, and overlap is counted once, on the earlier pass.

use super::compile::QueueKind;
use engine_rhi::ash::vk::Handle as _;
use engine_rhi::ash::{self, vk};
use engine_rhi::{Device, Queue};

/// GPU time of one pass of a completed frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PassTime {
    /// Pass name.
    pub name: &'static str,
    /// The scope the pass was declared in ([`crate::graph::FrameGraph::begin_scope`]).
    pub scope: Option<u32>,
    /// The queue it ran on. Async compute passes overlap graphics work: their times do
    /// not add up to the frame's.
    pub queue: QueueKind,
    /// The pass's own time on its queue, in microseconds: from its start timestamp, or
    /// from the end of the pass before it on the queue if that came later, to its end
    /// (see the module's documentation).
    pub gpu_us: f32,
    /// Time between the pass's own start and end timestamps, in microseconds: includes
    /// the tail of the pass before it when the two overlapped.
    pub span_us: f32,
}

#[derive(Default)]
struct SlotQueries {
    pool: vk::QueryPool,
    capacity: u32,
    /// Passes recorded in this slot's last frame: name, scope and first query index.
    pending: Vec<(&'static str, Option<u32>, u32)>,
    /// The queries opening and closing the frame, if written.
    frame: [Option<u32>; 2],
    /// Queries written in that frame.
    written: u32,
}

pub(crate) struct Timing {
    queue: QueueKind,
    slots: Vec<SlotQueries>,
    period_ns: f32,
    valid_mask: u64,
    results: Vec<u64>,
    times: Vec<PassTime>,
    frame_us: Option<f32>,
    tracy: Option<engine_profiler::GpuContext>,
}

impl Timing {
    /// `None` when `queue`'s family has no timestamps.
    pub(crate) fn new(
        device: &Device,
        slots: usize,
        queue: Queue,
        kind: QueueKind,
    ) -> Option<Self> {
        let adapter = device.adapter();
        let bits = adapter.queue_family(queue.family)?.timestamp_valid_bits;
        let period_ns = adapter.limits.timestamp_period;
        if bits == 0 || period_ns <= 0.0 {
            return None;
        }
        let valid_mask = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let tracy = if engine_profiler::ENABLED {
            let name = match kind {
                QueueKind::Graphics => "graphics queue",
                QueueKind::AsyncCompute => "async compute queue",
            };
            // SAFETY: one-off submission on the queue, waited for.
            unsafe { calibrate(device, queue) }.and_then(|timestamp| {
                engine_profiler::GpuContext::new_vulkan(
                    name,
                    (timestamp & valid_mask) as i64,
                    period_ns,
                )
            })
        } else {
            None
        };
        Some(Self {
            queue: kind,
            slots: (0..slots).map(|_| SlotQueries::default()).collect(),
            period_ns,
            valid_mask,
            results: Vec::new(),
            times: Vec::new(),
            frame_us: None,
            tracy,
        })
    }

    /// GPU times of the most recently completed frame.
    pub(crate) fn times(&self) -> &[PassTime] {
        &self.times
    }

    /// The span between the frame's opening and closing timestamps, µs.
    pub(crate) fn frame_us(&self) -> Option<f32> {
        self.frame_us
    }

    fn tracy_query(&self, slot: usize, query: u32) -> u16 {
        let capacity = self.slots[slot].capacity;
        ((slot as u32 * capacity + query) & 0xFFFF) as u16
    }

    /// Whether timestamp `a` comes after `b` (the counter may wrap within its valid bits;
    /// one frame is far shorter than half its period).
    fn later(&self, a: u64, b: u64) -> bool {
        let ahead = (a & self.valid_mask).wrapping_sub(b & self.valid_mask) & self.valid_mask;
        ahead != 0 && ahead <= self.valid_mask >> 1
    }

    fn ticks_us(&self, start: u64, end: u64) -> f32 {
        let ticks = (end & self.valid_mask).wrapping_sub(start & self.valid_mask) & self.valid_mask;
        ticks as f32 * self.period_ns / 1000.0
    }

    /// Reads the results of `slot`'s previous frame (if it used this queue) and forgets
    /// them; call once per frame, before [`Self::reset`].
    ///
    /// # Safety
    ///
    /// `slot`'s previous frame has finished on the GPU.
    pub(crate) unsafe fn collect(&mut self, device: &ash::Device, slot: usize) {
        let state = &mut self.slots[slot];
        self.times.clear();
        self.frame_us = None;
        if state.written > 0 && !state.pool.is_null() {
            self.results.clear();
            self.results.resize(state.written as usize, 0);
            // SAFETY: the queries were written by a finished frame; no WAIT flag, so a
            // query that is somehow not ready yields NOT_READY instead of blocking.
            let read = unsafe {
                device.get_query_pool_results(
                    state.pool,
                    0,
                    &mut self.results,
                    vk::QueryResultFlags::TYPE_64,
                )
            };
            if read.is_ok() {
                let state = &self.slots[slot];
                // Passes in the order they were recorded on this queue.
                let mut previous_end: Option<u64> = None;
                for &(name, scope, first) in &state.pending {
                    let start = self.results[first as usize];
                    let end = self.results[first as usize + 1];
                    let from = previous_end
                        .filter(|&p| self.later(p, start))
                        .unwrap_or(start);
                    let gpu_us = if self.later(from, end) {
                        0.0
                    } else {
                        self.ticks_us(from, end)
                    };
                    previous_end =
                        Some(previous_end.filter(|&p| self.later(p, end)).unwrap_or(end));
                    self.times.push(PassTime {
                        name,
                        scope,
                        queue: self.queue,
                        gpu_us,
                        span_us: self.ticks_us(start, end),
                    });
                }
                if let [Some(begin), Some(end)] = state.frame {
                    self.frame_us = Some(
                        self.ticks_us(self.results[begin as usize], self.results[end as usize]),
                    );
                }
                if let Some(tracy) = &self.tracy {
                    let capacity = state.capacity;
                    for (query, &timestamp) in self.results.iter().enumerate() {
                        let id = ((slot as u32 * capacity + query as u32) & 0xFFFF) as u16;
                        tracy.upload(id, (timestamp & self.valid_mask) as i64);
                    }
                }
            }
        }
        let state = &mut self.slots[slot];
        state.pending.clear();
        state.frame = [None; 2];
        state.written = 0;
    }

    /// Prepares `queries` queries of `slot` for this frame, recording the reset into
    /// `cmd` (the first command buffer of this queue in the frame).
    ///
    /// # Safety
    ///
    /// `slot`'s previous frame has finished on the GPU; `cmd` is recording, outside a
    /// render pass, and is submitted to this queue before the others of the frame.
    pub(crate) unsafe fn reset(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        slot: usize,
        queries: u32,
    ) {
        let state = &mut self.slots[slot];
        if state.capacity < queries {
            if !state.pool.is_null() {
                // SAFETY: the slot's previous frame has finished.
                unsafe { device.destroy_query_pool(state.pool, None) };
                state.pool = vk::QueryPool::null();
            }
            let capacity = queries.next_power_of_two().max(64);
            // SAFETY: valid create info.
            match unsafe {
                device.create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count(capacity),
                    None,
                )
            } {
                Ok(pool) => {
                    state.pool = pool;
                    state.capacity = capacity;
                }
                Err(_) => {
                    state.capacity = 0;
                    return;
                }
            }
        }
        if queries > 0 {
            // SAFETY: recording, outside a render pass (caller).
            unsafe { device.cmd_reset_query_pool(cmd, state.pool, 0, queries) };
        }
    }

    /// Records the timestamp opening (`end == false`) or closing the frame.
    ///
    /// # Safety
    ///
    /// `cmd` is recording on this queue; [`Self::reset`] prepared enough queries.
    pub(crate) unsafe fn frame_mark(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        slot: usize,
        end: bool,
    ) {
        let state = &mut self.slots[slot];
        let query = state.written;
        if query >= state.capacity {
            return;
        }
        let stage = if end {
            vk::PipelineStageFlags2::BOTTOM_OF_PIPE
        } else {
            vk::PipelineStageFlags2::TOP_OF_PIPE
        };
        // SAFETY: the query was reset in this frame's first command buffer of the queue.
        unsafe { device.cmd_write_timestamp2(cmd, stage, state.pool, query) };
        state.frame[usize::from(end)] = Some(query);
        state.written += 1;
    }

    /// Records the timestamp opening a pass.
    ///
    /// # Safety
    ///
    /// `cmd` is recording on this queue; [`Self::reset`] prepared enough queries for
    /// `slot`.
    pub(crate) unsafe fn pass_begin(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        slot: usize,
        name: &'static str,
        scope: Option<u32>,
    ) {
        let query = self.slots[slot].written;
        if query + 2 > self.slots[slot].capacity {
            return;
        }
        let id = self.tracy_query(slot, query);
        let state = &mut self.slots[slot];
        // SAFETY: the query was reset in this frame.
        unsafe {
            device.cmd_write_timestamp2(
                cmd,
                vk::PipelineStageFlags2::TOP_OF_PIPE,
                state.pool,
                query,
            )
        };
        state.pending.push((name, scope, query));
        state.written += 1;
        if let Some(tracy) = &self.tracy {
            tracy.begin_zone(name, id);
        }
    }

    /// Records the timestamp closing the pass opened last.
    ///
    /// # Safety
    ///
    /// As for [`Self::pass_begin`], which must have been called for this pass.
    pub(crate) unsafe fn pass_end(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        slot: usize,
    ) {
        let state = &self.slots[slot];
        let opened = state
            .pending
            .last()
            .is_some_and(|&(_, _, first)| first + 1 == state.written);
        if !opened || state.written >= state.capacity {
            return;
        }
        let query = state.written;
        let id = self.tracy_query(slot, query);
        let state = &mut self.slots[slot];
        // SAFETY: as above.
        unsafe {
            device.cmd_write_timestamp2(
                cmd,
                vk::PipelineStageFlags2::BOTTOM_OF_PIPE,
                state.pool,
                query,
            )
        };
        state.written += 1;
        if let Some(tracy) = &self.tracy {
            tracy.end_zone(id);
        }
    }

    /// Destroys the query pools.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub(crate) unsafe fn destroy(&mut self, device: &ash::Device) {
        for slot in &mut self.slots {
            if !slot.pool.is_null() {
                // SAFETY: guaranteed by the caller.
                unsafe { device.destroy_query_pool(slot.pool, None) };
                slot.pool = vk::QueryPool::null();
                slot.capacity = 0;
            }
        }
    }
}

/// A GPU timestamp taken now on `queue`, for correlating the GPU and CPU timelines.
///
/// # Safety
///
/// `queue` is not used concurrently by another thread.
unsafe fn calibrate(device: &Device, queue: Queue) -> Option<u64> {
    let d = device.raw();
    // SAFETY: plain object creation and a one-off, waited-for submission.
    unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(queue.family),
                None,
            )
            .ok()?;
        let queries = d
            .create_query_pool(
                &vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::TIMESTAMP)
                    .query_count(1),
                None,
            )
            .ok();
        let fence = d.create_fence(&vk::FenceCreateInfo::default(), None).ok();
        let mut result = None;
        if let (Some(queries), Some(fence)) = (queries, fence)
            && let Ok(cmds) = d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )
        {
            let cmd = cmds[0];
            let recorded = d
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .is_ok();
            if recorded {
                d.cmd_reset_query_pool(cmd, queries, 0, 1);
                d.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::BOTTOM_OF_PIPE, queries, 0);
                let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
                if d.end_command_buffer(cmd).is_ok()
                    && d.queue_submit2(
                        queue.raw,
                        &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                        fence,
                    )
                    .is_ok()
                    && d.wait_for_fences(&[fence], true, 1_000_000_000).is_ok()
                {
                    let mut value = [0u64];
                    if d.get_query_pool_results(
                        queries,
                        0,
                        &mut value,
                        vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
                    )
                    .is_ok()
                    {
                        result = Some(value[0]);
                    }
                }
            }
        }
        if let Some(fence) = fence {
            d.destroy_fence(fence, None);
        }
        if let Some(queries) = queries {
            d.destroy_query_pool(queries, None);
        }
        d.destroy_command_pool(pool, None);
        result
    }
}
