//! GPU time per pass (CLAUDE.md §14: GPU passes per viewport in the editor profiler,
//! Tracy GPU zones).
//!
//! Each frame slot owns a timestamp query pool. Every needed pass is bracketed by two
//! timestamps; when the slot comes around again (its previous frame has finished), the
//! results are read without waiting, turned into per-pass GPU times and, in profiling
//! builds, uploaded to Tracy as GPU zones.

use engine_rhi::Device;
use engine_rhi::ash::vk::Handle as _;
use engine_rhi::ash::{self, vk};

/// GPU time of one pass of a completed frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PassTime {
    /// Pass name.
    pub name: &'static str,
    /// Time between the pass's start and end timestamps, in microseconds.
    pub gpu_us: f32,
}

#[derive(Default)]
struct SlotQueries {
    pool: vk::QueryPool,
    capacity: u32,
    /// Passes recorded in this slot's last frame: name and first query index.
    pending: Vec<(&'static str, u32)>,
    /// Queries written in that frame.
    written: u32,
}

pub(crate) struct Timing {
    slots: Vec<SlotQueries>,
    period_ns: f32,
    valid_mask: u64,
    results: Vec<u64>,
    times: Vec<PassTime>,
    tracy: Option<engine_profiler::GpuContext>,
}

impl Timing {
    /// `None` when the graphics queue has no timestamps.
    pub(crate) fn new(device: &Device, slots: usize) -> Option<Self> {
        let adapter = device.adapter();
        let bits = adapter
            .queue_family(device.graphics.family)?
            .timestamp_valid_bits;
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
            // SAFETY: one-off submission on the graphics queue, waited for.
            unsafe { calibrate(device) }.and_then(|timestamp| {
                engine_profiler::GpuContext::new_vulkan(
                    "graphics queue",
                    (timestamp & valid_mask) as i64,
                    period_ns,
                )
            })
        } else {
            None
        };
        Some(Self {
            slots: (0..slots).map(|_| SlotQueries::default()).collect(),
            period_ns,
            valid_mask,
            results: Vec::new(),
            times: Vec::new(),
            tracy,
        })
    }

    /// GPU times of the most recently completed frame.
    pub(crate) fn times(&self) -> &[PassTime] {
        &self.times
    }

    fn tracy_query(&self, slot: usize, query: u32) -> u16 {
        let capacity = self.slots[slot].capacity;
        ((slot as u32 * capacity + query) & 0xFFFF) as u16
    }

    /// Reads the results of `slot`'s previous frame and prepares `queries` queries for
    /// this one (recording the reset into `cmd`).
    ///
    /// # Safety
    ///
    /// `slot`'s previous frame has finished on the GPU; `cmd` is recording, outside a
    /// render pass.
    pub(crate) unsafe fn begin_frame(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        slot: usize,
        queries: u32,
    ) {
        let state = &mut self.slots[slot];
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
                self.times.clear();
                for &(name, first) in &state.pending {
                    let start = self.results[first as usize] & self.valid_mask;
                    let end = self.results[first as usize + 1] & self.valid_mask;
                    let ticks = end.wrapping_sub(start) & self.valid_mask;
                    self.times.push(PassTime {
                        name,
                        gpu_us: ticks as f32 * self.period_ns / 1000.0,
                    });
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
        state.pending.clear();
        state.written = 0;
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

    /// Records the timestamp opening a pass.
    ///
    /// # Safety
    ///
    /// `cmd` is recording; [`Self::begin_frame`] prepared enough queries for `slot`.
    pub(crate) unsafe fn pass_begin(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        slot: usize,
        name: &'static str,
    ) {
        let query = self.slots[slot].written;
        if query + 2 > self.slots[slot].capacity {
            return;
        }
        let id = self.tracy_query(slot, query);
        let state = &mut self.slots[slot];
        // SAFETY: the query was reset in this command buffer.
        unsafe {
            device.cmd_write_timestamp2(
                cmd,
                vk::PipelineStageFlags2::TOP_OF_PIPE,
                state.pool,
                query,
            )
        };
        state.pending.push((name, query));
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
        if state.written.is_multiple_of(2) || state.written >= state.capacity {
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

/// A GPU timestamp taken now, for correlating the GPU and CPU timelines.
///
/// # Safety
///
/// The graphics queue is not used concurrently by another thread.
unsafe fn calibrate(device: &Device) -> Option<u64> {
    let d = device.raw();
    // SAFETY: plain object creation and a one-off, waited-for submission.
    unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(device.graphics.family),
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
                        device.graphics.raw,
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
