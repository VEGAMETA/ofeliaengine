//! Frames in flight (CLAUDE.md §7: configurable 1–3).
//!
//! Each frame slot owns a command pool and an "image available" semaphore. One timeline
//! semaphore counts submitted frames: before a slot is reused, the CPU waits until the
//! GPU has finished the frame that last used it. No fences, no per-frame allocation.

use crate::device::{Device, Queue};
use crate::error::RhiError;
use ash::vk;

struct Slot {
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    image_available: vk::Semaphore,
    /// Timeline value that marks this slot's last submission as complete.
    done_value: u64,
}

/// A frame being recorded, returned by [`Frames::begin`].
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    /// Command buffer in the recording state.
    pub cmd: vk::CommandBuffer,
    /// Pass to `Swapchain::acquire`; the submission waits on it.
    pub image_available: vk::Semaphore,
    /// Frame number (1-based, increases by one per submitted frame).
    pub number: u64,
    slot: usize,
}

/// Ring of frames in flight.
pub struct Frames {
    device: ash::Device,
    slots: Vec<Slot>,
    timeline: vk::Semaphore,
    submitted: u64,
    next: usize,
}

impl core::fmt::Debug for Frames {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frames")
            .field("in_flight", &self.slots.len())
            .field("submitted", &self.submitted)
            .finish()
    }
}

/// How long `begin` waits for the GPU before reporting a hang.
const GPU_TIMEOUT_NS: u64 = 5_000_000_000;

impl Frames {
    /// Creates `in_flight` (clamped to 1..=3) frame slots whose command buffers are
    /// submitted to queues of `queue_family`.
    pub fn new(device: &Device, queue_family: u32, in_flight: usize) -> Result<Self, RhiError> {
        let d = device.raw();
        let mut type_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        // SAFETY: valid create info.
        let timeline = unsafe {
            d.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut type_info),
                None,
            )
        }
        .map_err(RhiError::vk("vkCreateSemaphore(timeline)"))?;
        let mut frames = Self {
            device: d.clone(),
            slots: Vec::new(),
            timeline,
            submitted: 0,
            next: 0,
        };
        for _ in 0..in_flight.clamp(1, 3) {
            // SAFETY: valid create infos; objects are owned by `frames` and destroyed in
            // `Drop` after the GPU is idle.
            unsafe {
                let pool = d
                    .create_command_pool(
                        &vk::CommandPoolCreateInfo::default()
                            .queue_family_index(queue_family)
                            .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                        None,
                    )
                    .map_err(RhiError::vk("vkCreateCommandPool"))?;
                let cmd = d
                    .allocate_command_buffers(
                        &vk::CommandBufferAllocateInfo::default()
                            .command_pool(pool)
                            .level(vk::CommandBufferLevel::PRIMARY)
                            .command_buffer_count(1),
                    )
                    .map_err(RhiError::vk("vkAllocateCommandBuffers"))?[0];
                let image_available = d
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                    .map_err(RhiError::vk("vkCreateSemaphore"))?;
                frames.slots.push(Slot {
                    pool,
                    cmd,
                    image_available,
                    done_value: 0,
                });
            }
        }
        Ok(frames)
    }

    /// Number of frames in flight.
    pub fn in_flight(&self) -> usize {
        self.slots.len()
    }

    /// Waits until the next slot is free, resets it and begins recording.
    pub fn begin(&mut self) -> Result<Frame, RhiError> {
        engine_profiler::zone!("rhi::Frames::begin");
        let slot_index = self.next;
        let done_value = self.slots[slot_index].done_value;
        self.wait_for(done_value)?;
        let slot = &self.slots[slot_index];
        // SAFETY: the slot's previous work has completed (timeline wait above).
        unsafe {
            self.device
                .reset_command_pool(slot.pool, vk::CommandPoolResetFlags::empty())
                .map_err(RhiError::vk("vkResetCommandPool"))?;
            self.device
                .begin_command_buffer(
                    slot.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(RhiError::vk("vkBeginCommandBuffer"))?;
        }
        Ok(Frame {
            cmd: slot.cmd,
            image_available: slot.image_available,
            number: self.submitted + 1,
            slot: slot_index,
        })
    }

    /// Ends recording and submits the frame to `queue`. If `waits_for_image` the
    /// submission waits for `frame.image_available` before writing color attachments;
    /// `signal` (typically `Swapchain::render_finished`) is signaled when it completes.
    pub fn submit(
        &mut self,
        queue: Queue,
        frame: Frame,
        waits_for_image: bool,
        signal: Option<vk::Semaphore>,
    ) -> Result<(), RhiError> {
        engine_profiler::zone!("rhi::Frames::submit");
        let value = self.submitted + 1;
        let waits = [vk::SemaphoreSubmitInfo::default()
            .semaphore(frame.image_available)
            .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
        let mut signals = [
            vk::SemaphoreSubmitInfo::default()
                .semaphore(self.timeline)
                .value(value)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
            vk::SemaphoreSubmitInfo::default().stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
        ];
        let signal_count = match signal {
            Some(semaphore) => {
                signals[1] = signals[1].semaphore(semaphore);
                2
            }
            None => 1,
        };
        let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(frame.cmd)];
        let submit = vk::SubmitInfo2::default()
            .wait_semaphore_infos(if waits_for_image { &waits } else { &[] })
            .command_buffer_infos(&commands)
            .signal_semaphore_infos(&signals[..signal_count]);
        // SAFETY: the command buffer was recorded by `begin`/the caller and is ended here;
        // the semaphores are valid; external synchronization of `queue` is the caller's
        // (single submitting thread).
        unsafe {
            self.device
                .end_command_buffer(frame.cmd)
                .map_err(RhiError::vk("vkEndCommandBuffer"))?;
            self.device
                .queue_submit2(queue.raw, &[submit], vk::Fence::null())
                .map_err(RhiError::vk("vkQueueSubmit2"))?;
        }
        self.submitted = value;
        self.slots[frame.slot].done_value = value;
        self.next = (frame.slot + 1) % self.slots.len();
        Ok(())
    }

    /// Waits until every submitted frame has completed.
    pub fn wait_idle(&self) -> Result<(), RhiError> {
        self.wait_for(self.submitted)
    }

    /// Frames submitted so far.
    pub fn submitted(&self) -> u64 {
        self.submitted
    }

    fn wait_for(&self, value: u64) -> Result<(), RhiError> {
        if value == 0 {
            return Ok(());
        }
        let semaphores = [self.timeline];
        let values = [value];
        // SAFETY: waiting on our own timeline semaphore.
        unsafe {
            self.device.wait_semaphores(
                &vk::SemaphoreWaitInfo::default()
                    .semaphores(&semaphores)
                    .values(&values),
                GPU_TIMEOUT_NS,
            )
        }
        .map_err(RhiError::vk("vkWaitSemaphores"))
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        let _ = self.wait_idle();
        // SAFETY: all work using these objects has completed.
        unsafe {
            for slot in self.slots.drain(..) {
                self.device.destroy_semaphore(slot.image_available, None);
                self.device.destroy_command_pool(slot.pool, None);
            }
            self.device.destroy_semaphore(self.timeline, None);
        }
    }
}
