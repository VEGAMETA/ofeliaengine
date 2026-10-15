//! The AV1 decode session and its pictures (the Vulkan specification's video coding,
//! video decode and AV1 decode chapters).

use engine_rhi::ash::khr::{video_decode_queue, video_queue};
use engine_rhi::ash::vk;
use engine_rhi::ash::vk::native::StdVideoDecodeAV1ReferenceInfo;
use engine_rhi::{Allocation, AllocationRequest, Allocator, Device, MemoryLocation, ResourceKind};

use super::caps::{GpuCaps, Profile};
use super::std_av1::{StdPicture, StdSequence, reference_info, tile_starts_quirk};
use super::{GpuError, vk_error};
use crate::av1::frame::{FrameHeader, FrameType};
use crate::av1::{NUM_REF_FRAMES, REFS_PER_FRAME, SequenceHeader, TemporalUnit};

/// Command buffers, each with its bitstream buffer, that may be in flight.
const IN_FLIGHT: usize = 4;
/// Reference picture slots: AV1's eight references and the frame being decoded.
const DPB_SLOTS: u32 = NUM_REF_FRAMES as u32 + 1;
/// The smallest bitstream buffer: larger units grow it.
const MIN_BITSTREAM: u64 = 1 << 20;

/// How a [`GpuDecoder`] is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuDecoderDesc {
    /// Output images: pictures handed out and not released yet, and hidden frames kept
    /// until they are shown.
    pub outputs: u32,
    /// Whether the decoder applies the streams' film grain.
    pub film_grain: bool,
}

impl Default for GpuDecoderDesc {
    fn default() -> Self {
        Self {
            outputs: 12,
            film_grain: true,
        }
    }
}

/// A decoded picture, in one of the decoder's output images (NV12 or P010, single
/// layer, shared by the decode and graphics queues). Read it once the decoder's timeline
/// semaphore reaches [`Self::ready`], from [`Self::layout`]; give it back with
/// [`GpuDecoder::release`] when nothing reads it any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuPicture {
    /// The image.
    pub image: vk::Image,
    /// Its format.
    pub format: vk::Format,
    /// The decoded size in luma samples (after super-resolution).
    pub width: u32,
    /// The decoded height.
    pub height: u32,
    /// The size to show it at (the stream's render size).
    pub render_width: u32,
    /// The height to show it at.
    pub render_height: u32,
    /// The image's layout when ready.
    pub layout: vk::ImageLayout,
    /// The value of [`GpuDecoder::semaphore`] after which it is decoded.
    pub ready: u64,
    /// The timestamp given with its temporal unit.
    pub timestamp: i64,
    /// Its output image, for [`GpuDecoder::release`].
    output: u32,
}

/// A reference picture slot (a layer of the reference picture image).
#[derive(Clone, Copy)]
struct Slot {
    /// AV1's reference slots that hold this picture (bits 0..8).
    held: u16,
    /// The reference information it was decoded with.
    info: StdVideoDecodeAV1ReferenceInfo,
    /// Its decoded size.
    extent: vk::Extent2D,
    /// The output image of a hidden frame that may be shown later.
    output: Option<u32>,
}

/// An output image.
struct Output {
    image: vk::Image,
    allocation: Option<Allocation>,
    view: vk::ImageView,
    /// Holders: pictures handed out and a slot keeping a hidden frame's picture.
    users: u32,
    /// The picture it holds, as handed out.
    picture: GpuPicture,
}

/// A command buffer and its bitstream buffer.
struct Submission {
    commands: vk::CommandBuffer,
    bitstream: Option<(vk::Buffer, Allocation)>,
    capacity: u64,
    /// The timeline value its last use signals.
    done: u64,
}

/// An AV1 decoder on the device's video engine.
pub struct GpuDecoder {
    caps: GpuCaps,
    profile: Box<Profile>,
    queue: vk::Queue,
    graphics_family: u32,
    decode_family: u32,
    queue_fns: video_queue::Device,
    decode_fns: video_decode_queue::Device,
    session: vk::VideoSessionKHR,
    session_memory: Vec<Allocation>,
    parameters: vk::VideoSessionParametersKHR,
    /// The largest frame the parameters' sequence header allows.
    parameters_max: (u32, u32),
    sequence_serial: u32,
    film_grain: bool,
    /// The tiles' starts in superblocks (`log2` of a superblock in mode info units) for a
    /// driver that reads them so ([`tile_starts_quirk`]).
    starts_in_superblocks: Option<u32>,
    extent: vk::Extent2D,
    dpb: vk::Image,
    dpb_allocation: Option<Allocation>,
    dpb_view: vk::ImageView,
    slots: [Slot; DPB_SLOTS as usize],
    /// The reference picture slot of each of AV1's reference slots.
    references: [Option<u8>; NUM_REF_FRAMES],
    outputs: Vec<Output>,
    pool: vk::CommandPool,
    submissions: [Submission; IN_FLIGHT],
    next: usize,
    semaphore: vk::Semaphore,
    value: u64,
    /// The session's slots need a reset before the next decode.
    reset: bool,
    /// The reference picture image is still in its initial layout.
    fresh: bool,
    picture: Box<StdPicture>,
}

impl core::fmt::Debug for GpuDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuDecoder")
            .field("extent", &self.extent)
            .field("coincide", &self.coincides())
            .field("outputs", &self.outputs.len())
            .finish_non_exhaustive()
    }
}

fn align(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment) * alignment
}

fn extent_of(h: &FrameHeader) -> vk::Extent2D {
    vk::Extent2D {
        width: h.upscaled_width,
        height: h.frame_height,
    }
}

impl GpuDecoder {
    /// A decoder for a stream with `sequence` on `device`'s decode queue.
    ///
    /// # Errors
    ///
    /// No decode queue, a stream the device's decoder does not take
    /// ([`GpuError::Unsupported`]: decode it with the software decoder), or a failed
    /// Vulkan call or allocation.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        sequence: &SequenceHeader,
        sequence_serial: u32,
        desc: &GpuDecoderDesc,
    ) -> Result<Self, GpuError> {
        let queue = device.video_decode.ok_or(GpuError::NoDecodeQueue)?;
        let film_grain = desc.film_grain && sequence.film_grain_params_present;
        let profile = Profile::new(sequence.color.bit_depth, film_grain);
        let caps = GpuCaps::for_profile(device, &profile)?;
        if let Some(why) = caps.refuses(sequence) {
            return Err(GpuError::Unsupported(why));
        }
        if sequence.max_width() < caps.min_extent.width
            || sequence.max_height() < caps.min_extent.height
        {
            return Err(GpuError::Unsupported(
                "pictures smaller than the decoder takes",
            ));
        }
        let raw = device.raw();
        let queue_fns = video_queue::Device::new(device.instance_raw(), raw);
        let decode_fns = video_decode_queue::Device::new(device.instance_raw(), raw);
        let granularity = caps.granularity;
        let extent = vk::Extent2D {
            width: align(
                u64::from(sequence.max_width()),
                u64::from(granularity.width.max(1)),
            ) as u32,
            height: align(
                u64::from(sequence.max_height()),
                u64::from(granularity.height.max(1)),
            ) as u32,
        }
        .min_with(caps.max_extent);
        if caps.max_dpb_slots < DPB_SLOTS {
            return Err(GpuError::Unsupported(
                "fewer than nine reference picture slots",
            ));
        }
        // Coinciding, pictures are copied out on the decode queue: its family must copy.
        let copies = device
            .adapter()
            .queue_family(queue.family)
            .is_some_and(|f| f.queue_flags.contains(vk::QueueFlags::TRANSFER));
        if caps.coincide && !caps.distinct && !copies {
            return Err(GpuError::Unsupported(
                "a decode queue that cannot copy its pictures out",
            ));
        }

        let mut this = Self {
            caps,
            profile,
            queue: queue.raw,
            graphics_family: device.graphics.family,
            decode_family: queue.family,
            queue_fns,
            decode_fns,
            session: vk::VideoSessionKHR::null(),
            session_memory: Vec::new(),
            parameters: vk::VideoSessionParametersKHR::null(),
            parameters_max: (0, 0),
            sequence_serial,
            film_grain,
            starts_in_superblocks: tile_starts_quirk(device.adapter().driver_id).then_some(
                if sequence.use_128x128_superblock {
                    5
                } else {
                    4
                },
            ),
            extent,
            dpb: vk::Image::null(),
            dpb_allocation: None,
            dpb_view: vk::ImageView::null(),
            slots: [Slot {
                held: 0,
                info: reference_info(&FrameHeader::default()),
                extent: vk::Extent2D::default(),
                output: None,
            }; DPB_SLOTS as usize],
            references: [None; NUM_REF_FRAMES],
            outputs: Vec::new(),
            pool: vk::CommandPool::null(),
            submissions: core::array::from_fn(|_| Submission {
                commands: vk::CommandBuffer::null(),
                bitstream: None,
                capacity: 0,
                done: 0,
            }),
            next: 0,
            semaphore: vk::Semaphore::null(),
            value: 0,
            reset: true,
            fresh: true,
            picture: StdPicture::new(),
        };
        // Everything made so far is destroyed if a later step fails.
        match this.create(device, allocator, sequence, desc) {
            Ok(()) => Ok(this),
            Err(error) => {
                // SAFETY: nothing was submitted; the objects made are this device's.
                unsafe { this.destroy(device, allocator) };
                Err(error)
            }
        }
    }

    fn coincides(&self) -> bool {
        self.caps.coincide && !self.caps.distinct
    }

    fn create(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        sequence: &SequenceHeader,
        desc: &GpuDecoderDesc,
    ) -> Result<(), GpuError> {
        let raw = device.raw();
        let format = self.caps.format;
        let std_version = self.caps.std_header_version;
        let session_info = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(self.decode_family)
            .video_profile(self.profile.info())
            .picture_format(format)
            .max_coded_extent(self.extent)
            .reference_picture_format(format)
            .max_dpb_slots(DPB_SLOTS)
            .max_active_reference_pictures(
                (REFS_PER_FRAME as u32).min(self.caps.max_active_references),
            )
            .std_header_version(&std_version);
        // SAFETY: valid create info; the profile outlives the call.
        let result = unsafe {
            (self.queue_fns.fp().create_video_session_khr)(
                raw.handle(),
                &session_info,
                core::ptr::null(),
                &mut self.session,
            )
        };
        if result != vk::Result::SUCCESS {
            return Err(vk_error("vkCreateVideoSessionKHR")(result));
        }
        self.bind_session_memory(device, allocator)?;
        self.create_parameters(device, sequence)?;

        // The reference pictures: one image, a layer per slot, only for the decode queue.
        let coincide = self.coincides();
        let profiles = [*self.profile.info()];
        let mut dpb_usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
        if coincide {
            // Decoded into, and copied from into the output images.
            dpb_usage |=
                vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC;
        }
        if !self.caps.dpb_usage.contains(dpb_usage) {
            return Err(GpuError::Unsupported(
                "reference pictures of the usage needed",
            ));
        }
        let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: self.extent.width,
                height: self.extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(DPB_SLOTS)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(dpb_usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list);
        let (image, allocation) = allocator.create_image(&image_info, MemoryLocation::Device)?;
        self.dpb = image;
        self.dpb_allocation = Some(allocation);
        let video_usage = dpb_usage
            & (vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
                | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR);
        self.dpb_view = picture_view(
            raw,
            image,
            format,
            vk::ImageViewType::TYPE_2D_ARRAY,
            DPB_SLOTS,
            video_usage,
        )?;

        // The output images: decoded into (distinct) or copied into (coinciding), then read
        // by the graphics queue.
        let mut output_usage = vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::SAMPLED;
        if coincide {
            output_usage |= vk::ImageUsageFlags::TRANSFER_DST;
        }
        if !self.caps.output_usage.contains(output_usage) {
            return Err(GpuError::Unsupported("output pictures of the usage needed"));
        }
        // Views of single planes, later, for sampling (the conversion pass, 8b-3).
        let output_flags = self.caps.output_flags
            & (vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE);
        let families = [self.decode_family, self.graphics_family];
        for _ in 0..desc.outputs.max(2) {
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let info = vk::ImageCreateInfo::default()
                .flags(output_flags)
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width: self.extent.width,
                    height: self.extent.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(output_usage)
                .sharing_mode(vk::SharingMode::CONCURRENT)
                .queue_family_indices(&families)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut list);
            let (image, allocation) = allocator.create_image(&info, MemoryLocation::Device)?;
            let index = self.outputs.len() as u32;
            self.outputs.push(Output {
                image,
                allocation: Some(allocation),
                view: vk::ImageView::null(),
                users: 0,
                picture: GpuPicture {
                    image,
                    format,
                    width: 0,
                    height: 0,
                    render_width: 0,
                    render_height: 0,
                    layout: vk::ImageLayout::UNDEFINED,
                    ready: 0,
                    timestamp: 0,
                    output: index,
                },
            });
            let view = picture_view(
                raw,
                image,
                format,
                vk::ImageViewType::TYPE_2D,
                1,
                vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
            )?;
            if let Some(output) = self.outputs.last_mut() {
                output.view = view;
            }
        }

        // The command buffers and the timeline semaphore.
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(self.decode_family);
        // SAFETY: valid create info.
        self.pool = unsafe { raw.create_command_pool(&pool_info, None) }
            .map_err(vk_error("vkCreateCommandPool"))?;
        let buffers_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(IN_FLIGHT as u32);
        // SAFETY: the pool was just made.
        let buffers = unsafe { raw.allocate_command_buffers(&buffers_info) }
            .map_err(vk_error("vkAllocateCommandBuffers"))?;
        for (submission, buffer) in self.submissions.iter_mut().zip(buffers) {
            submission.commands = buffer;
        }
        let mut timeline = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let semaphore_info = vk::SemaphoreCreateInfo::default().push_next(&mut timeline);
        // SAFETY: valid create info.
        self.semaphore = unsafe { raw.create_semaphore(&semaphore_info, None) }
            .map_err(vk_error("vkCreateSemaphore"))?;
        Ok(())
    }

    fn bind_session_memory(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
    ) -> Result<(), GpuError> {
        let raw = device.raw().handle();
        let get = self
            .queue_fns
            .fp()
            .get_video_session_memory_requirements_khr;
        let mut count = 0u32;
        // SAFETY: the session was just made; a null array asks for the count.
        let result = unsafe { get(raw, self.session, &mut count, core::ptr::null_mut()) };
        if result != vk::Result::SUCCESS {
            return Err(vk_error("vkGetVideoSessionMemoryRequirementsKHR")(result));
        }
        let mut requirements =
            vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
        // SAFETY: `requirements` holds `count` structures.
        let result = unsafe { get(raw, self.session, &mut count, requirements.as_mut_ptr()) };
        if result != vk::Result::SUCCESS {
            return Err(vk_error("vkGetVideoSessionMemoryRequirementsKHR")(result));
        }
        let mut binds = Vec::with_capacity(requirements.len());
        for requirement in &requirements[..count as usize] {
            let allocation = allocator.allocate(&AllocationRequest {
                requirements: requirement.memory_requirements,
                location: MemoryLocation::Device,
                kind: ResourceKind::Image,
                prefers_dedicated: false,
                requires_dedicated: false,
                dedicated_to: None,
            })?;
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(requirement.memory_bind_index)
                    .memory(allocation.memory())
                    .memory_offset(allocation.offset())
                    .memory_size(requirement.memory_requirements.size),
            );
            self.session_memory.push(allocation);
        }
        // SAFETY: each binding has memory of a type its requirements allow, aligned.
        let result = unsafe {
            (self.queue_fns.fp().bind_video_session_memory_khr)(
                raw,
                self.session,
                binds.len() as u32,
                binds.as_ptr(),
            )
        };
        if result != vk::Result::SUCCESS {
            return Err(vk_error("vkBindVideoSessionMemoryKHR")(result));
        }
        Ok(())
    }

    /// New session parameters for `sequence`, its largest frame widened to `frames` when
    /// the stream's frames exceed it.
    ///
    /// SVT-AV1 1.8 pads heights that are not a multiple of 8 and codes 320 × 184 frames
    /// under a 320 × 180 maximum, against the specification. NVIDIA decodes them as they
    /// are; AMD's decoder keeps to the sequence's maximum (super-resolution loses the last
    /// rows, scaled references the right scale). The widened maximum stays inside the
    /// session's pictures, and a conformant stream never needs it.
    fn create_parameters_for(
        &mut self,
        device: &Device,
        sequence: &SequenceHeader,
        frames: (u32, u32),
    ) -> Result<(), GpuError> {
        let mut widened = *sequence;
        let width = sequence.max_width().max(frames.0).min(self.extent.width);
        let height = sequence.max_height().max(frames.1).min(self.extent.height);
        widened.max_frame_width_minus_1 = width - 1;
        widened.max_frame_height_minus_1 = height - 1;
        let bits = |value: u32| 32 - value.leading_zeros();
        widened.frame_width_bits_minus_1 = widened
            .frame_width_bits_minus_1
            .max(bits(width - 1).saturating_sub(1) as u8);
        widened.frame_height_bits_minus_1 = widened
            .frame_height_bits_minus_1
            .max(bits(height - 1).saturating_sub(1) as u8);
        self.create_parameters(device, &widened)?;
        self.parameters_max = (width, height);
        Ok(())
    }

    fn create_parameters(
        &mut self,
        device: &Device,
        sequence: &SequenceHeader,
    ) -> Result<(), GpuError> {
        self.parameters_max = (sequence.max_width(), sequence.max_height());
        let std = StdSequence::new(sequence);
        let mut av1 = vk::VideoDecodeAV1SessionParametersCreateInfoKHR::default()
            .std_sequence_header(&std.header);
        let info = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(self.session)
            .push_next(&mut av1);
        let mut parameters = vk::VideoSessionParametersKHR::null();
        // SAFETY: valid create info; the sequence header and its parts outlive the call.
        let result = unsafe {
            (self.queue_fns.fp().create_video_session_parameters_khr)(
                device.raw().handle(),
                &info,
                core::ptr::null(),
                &mut parameters,
            )
        };
        if result != vk::Result::SUCCESS {
            return Err(vk_error("vkCreateVideoSessionParametersKHR")(result));
        }
        if self.parameters != vk::VideoSessionParametersKHR::null() {
            // The previous parameters' decodes must be finished.
            self.wait(device, self.value)?;
            // SAFETY: no submitted work uses the old parameters any more.
            unsafe {
                (self.queue_fns.fp().destroy_video_session_parameters_khr)(
                    device.raw().handle(),
                    self.parameters,
                    core::ptr::null(),
                );
            }
        }
        self.parameters = parameters;
        Ok(())
    }

    /// The timeline semaphore decodes signal; a picture is decoded once it reaches the
    /// picture's [`GpuPicture::ready`].
    pub fn semaphore(&self) -> vk::Semaphore {
        self.semaphore
    }

    /// The value the last decode submitted signals on [`Self::semaphore`].
    pub fn submitted(&self) -> u64 {
        self.value
    }

    /// What the device's decoder does for this stream.
    pub fn caps(&self) -> &GpuCaps {
        &self.caps
    }

    /// Waits on the CPU until the semaphore reaches `value`.
    ///
    /// # Errors
    ///
    /// A failed wait (a lost device).
    pub fn wait(&self, device: &Device, value: u64) -> Result<(), GpuError> {
        if value == 0 {
            return Ok(());
        }
        let semaphores = [self.semaphore];
        let values = [value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&semaphores)
            .values(&values);
        // SAFETY: the semaphore is this device's.
        unsafe { device.raw().wait_semaphores(&info, u64::MAX) }
            .map_err(vk_error("vkWaitSemaphores"))
    }

    /// Forgets the stream's reference pictures (for a seek: decoding resumes at a key
    /// frame). Pictures handed out stay valid until released.
    pub fn reset(&mut self) {
        for slot in 0..self.slots.len() {
            self.drop_slot_output(slot);
            self.slots[slot].held = 0;
        }
        self.references = [None; NUM_REF_FRAMES];
        self.reset = true;
    }

    /// Gives back a picture handed out by [`Self::decode`], once nothing reads it any more
    /// (the reads' submissions finished).
    pub fn release(&mut self, picture: &GpuPicture) {
        if let Some(output) = self.outputs.get_mut(picture.output as usize) {
            output.users = output.users.saturating_sub(1);
        }
    }

    fn drop_slot_output(&mut self, slot: usize) {
        if let Some(output) = self.slots[slot].output.take()
            && let Some(output) = self.outputs.get_mut(output as usize)
        {
            output.users = output.users.saturating_sub(1);
        }
    }

    /// Points AV1's reference slots in `refresh` at reference picture slot `slot`; slots no
    /// longer held drop the hidden picture they kept.
    fn refresh(&mut self, refresh: u8, slot: usize) {
        for i in 0..NUM_REF_FRAMES {
            if refresh >> i & 1 == 0 {
                continue;
            }
            if let Some(old) = self.references[i] {
                let old = usize::from(old);
                self.slots[old].held &= !(1 << i);
                if self.slots[old].held == 0 && old != slot {
                    self.drop_slot_output(old);
                }
            }
            self.references[i] = Some(slot as u8);
            self.slots[slot].held |= 1 << i;
        }
    }

    fn free_output(&self) -> Option<u32> {
        self.outputs
            .iter()
            .position(|o| o.users == 0)
            .map(|i| i as u32)
    }

    /// Decodes a temporal unit: `unit` as [`crate::av1::Parser::parse`] made it from
    /// `data`, of a stream whose sequence header is `sequence` (the parser's). Hands `out`
    /// the pictures it shows, in order. A sequence header that changed (a new
    /// [`crate::av1::Frame::sequence_serial`]) gets new session parameters.
    ///
    /// # Errors
    ///
    /// A frame the decoder cannot take (its size), one needing a reference never decoded,
    /// no free output image (pictures not released), or a failed Vulkan call. The
    /// decoder's references are then unreliable: [`Self::reset`] it and resume at a key
    /// frame (or hand the stream to the software decoder).
    #[allow(clippy::too_many_arguments)]
    pub fn decode(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        data: &[u8],
        unit: &TemporalUnit,
        sequence: &SequenceHeader,
        timestamp: i64,
        out: &mut Vec<GpuPicture>,
    ) -> Result<(), GpuError> {
        let Some(first) = unit.frames.first() else {
            return Ok(());
        };
        if first.sequence_serial != self.sequence_serial {
            if let Some(why) = self.caps.refuses(sequence) {
                return Err(GpuError::Unsupported(why));
            }
            if sequence.max_width() > self.extent.width
                || sequence.max_height() > self.extent.height
            {
                return Err(GpuError::Unsupported(
                    "a new sequence larger than the session",
                ));
            }
            self.create_parameters(device, sequence)?;
            self.sequence_serial = first.sequence_serial;
        }
        // Frames larger than the sequence header allows (a stream against the
        // specification): parameters with a maximum that holds them.
        let frames = unit
            .frames
            .iter()
            .filter(|f| !f.header.show_existing_frame)
            .fold((0, 0), |(w, h), f| {
                (w.max(f.header.upscaled_width), h.max(f.header.frame_height))
            });
        if frames.0 > self.parameters_max.0 || frames.1 > self.parameters_max.1 {
            self.create_parameters_for(device, sequence, frames)?;
        }
        let raw = device.raw();
        let index = self.next;
        self.next = (self.next + 1) % IN_FLIGHT;
        self.wait(device, self.submissions[index].done)?;
        let profiles = profile_list(&self.profile);
        self.upload(allocator, index, data, profiles)?;
        let commands = self.submissions[index].commands;
        let Some((bitstream, _)) = &self.submissions[index].bitstream else {
            return Err(GpuError::Unsupported("no bitstream buffer"));
        };
        let bitstream = *bitstream;
        let range = align(data.len() as u64, self.caps.bitstream_size_alignment);

        // SAFETY: the command buffer's last submission finished (waited above).
        unsafe {
            raw.begin_command_buffer(
                commands,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .map_err(vk_error("vkBeginCommandBuffer"))?;
        if self.fresh {
            let barrier = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                .dst_access_mask(
                    vk::AccessFlags2::VIDEO_DECODE_READ_KHR
                        | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
                )
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
                .image(self.dpb)
                .subresource_range(color_range(0, DPB_SLOTS));
            // SAFETY: recording; the image is this decoder's.
            unsafe {
                raw.cmd_pipeline_barrier2(
                    commands,
                    &vk::DependencyInfo::default().image_memory_barriers(&[barrier]),
                );
            }
            self.fresh = false;
        }
        let ready = self.value + 1;
        let mut result = Ok(());
        for frame in &unit.frames {
            let h = &frame.header;
            if h.show_existing_frame {
                result = self.show_existing(h, out);
            } else {
                let offsets = unit.tile_offsets(frame);
                let sizes = unit.tile_sizes(frame);
                result = self.record_frame(
                    raw,
                    commands,
                    bitstream,
                    range,
                    frame.header_obu_offset,
                    offsets,
                    sizes,
                    h,
                    ready,
                    timestamp,
                    out,
                );
            }
            if result.is_err() {
                break;
            }
        }
        // SAFETY: recording.
        let ended =
            unsafe { raw.end_command_buffer(commands) }.map_err(vk_error("vkEndCommandBuffer"));
        result?;
        ended?;
        let command_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(commands)];
        let signals = [vk::SemaphoreSubmitInfo::default()
            .semaphore(self.semaphore)
            .value(ready)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let submit = vk::SubmitInfo2::default()
            .command_buffer_infos(&command_infos)
            .signal_semaphore_infos(&signals);
        // SAFETY: the decode queue is this decoder's alone; everything recorded is valid
        // until the semaphore reaches `ready`.
        unsafe { raw.queue_submit2(self.queue, &[submit], vk::Fence::null()) }
            .map_err(vk_error("vkQueueSubmit2"))?;
        self.value = ready;
        self.submissions[index].done = ready;
        Ok(())
    }

    /// A frame that shows a picture decoded before (`show_existing_frame`).
    fn show_existing(
        &mut self,
        h: &FrameHeader,
        out: &mut Vec<GpuPicture>,
    ) -> Result<(), GpuError> {
        let slot = self.references[usize::from(h.frame_to_show_map_idx)]
            .ok_or(GpuError::MissingReference)?;
        let slot = usize::from(slot);
        let output = self.slots[slot].output.ok_or(GpuError::MissingReference)?;
        let picture = &mut self.outputs[output as usize];
        picture.users += 1;
        out.push(picture.picture);
        if h.frame_type == FrameType::Key {
            // The frame loading process: every reference slot holds the shown key frame.
            self.refresh(0xFF, slot);
        }
        Ok(())
    }

    /// Records the decode of one frame (and, coinciding, the copy of its picture).
    #[allow(clippy::too_many_arguments)]
    fn record_frame(
        &mut self,
        raw: &engine_rhi::ash::Device,
        commands: vk::CommandBuffer,
        bitstream: vk::Buffer,
        range: u64,
        header_offset: u32,
        tile_offsets: &[u32],
        tile_sizes: &[u32],
        h: &FrameHeader,
        ready: u64,
        timestamp: i64,
        out: &mut Vec<GpuPicture>,
    ) -> Result<(), GpuError> {
        let extent = extent_of(h);
        let min = self.caps.min_extent;
        if extent.width < min.width
            || extent.height < min.height
            || extent.width > self.extent.width
            || extent.height > self.extent.height
        {
            return Err(GpuError::Unsupported(
                "a frame size outside the decoder's range",
            ));
        }
        let showable = h.show_frame || h.showable_frame;
        let grain = self.film_grain && h.film_grain.apply_grain;
        let coincide = self.coincides();
        // Decoded into its own image when distinct (or film grain applies), else into the
        // reference slot and copied when shown.
        let to_output = !coincide || grain;
        let needs_output = to_output || showable;
        let output = if needs_output {
            Some(self.free_output().ok_or(GpuError::Unsupported(
                "more pictures held than output images",
            ))?)
        } else {
            None
        };
        // A free reference slot: none of AV1's references hold it.
        let slot = self
            .slots
            .iter()
            .position(|s| s.held == 0)
            .ok_or(GpuError::MissingReference)?;

        // The references, by AV1 reference name, and the slots they are in.
        let mut names = [-1i32; REFS_PER_FRAME];
        let mut active = [0usize; REFS_PER_FRAME];
        let mut active_count = 0;
        if !h.is_intra() {
            for (name, &idx) in names.iter_mut().zip(&h.ref_frame_idx) {
                let reference =
                    self.references[usize::from(idx)].ok_or(GpuError::MissingReference)?;
                *name = i32::from(reference);
                let reference = usize::from(reference);
                if !active[..active_count].contains(&reference) {
                    active[active_count] = reference;
                    active_count += 1;
                }
            }
        }
        self.picture.fill(h, self.starts_in_superblocks, grain);
        let setup_info = reference_info(h);

        // Picture resources: the references, the reconstructed picture, the output.
        let resource = |view: vk::ImageView, layer: u32, extent: vk::Extent2D| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(extent)
                .base_array_layer(layer)
                .image_view_binding(view)
        };
        let mut resources = [vk::VideoPictureResourceInfoKHR::default(); REFS_PER_FRAME + 1];
        let mut infos = [StdVideoDecodeAV1ReferenceInfo::default_zeroed(); REFS_PER_FRAME + 1];
        for (i, &reference) in active[..active_count].iter().enumerate() {
            resources[i] = resource(
                self.dpb_view,
                reference as u32,
                self.slots[reference].extent,
            );
            infos[i] = self.slots[reference].info;
        }
        resources[active_count] = resource(self.dpb_view, slot as u32, extent);
        infos[active_count] = setup_info;
        let mut dpb_infos = [vk::VideoDecodeAV1DpbSlotInfoKHR::default(); REFS_PER_FRAME + 1];
        for (dpb_info, info) in dpb_infos.iter_mut().zip(&infos) {
            dpb_info.p_std_reference_info = info;
        }
        let mut slots = [vk::VideoReferenceSlotInfoKHR::default(); REFS_PER_FRAME + 1];
        for i in 0..=active_count {
            slots[i].slot_index = if i < active_count {
                active[i] as i32
            } else {
                -1
            };
            slots[i].p_picture_resource = &resources[i];
            slots[i].p_next = (&raw const dpb_infos[i]).cast();
        }
        let mut setup = slots[active_count];
        setup.slot_index = slot as i32;

        let output_resource = output.map(|o| resource(self.outputs[o as usize].view, 0, extent));
        if let Some(o) = output.filter(|_| to_output) {
            let barrier = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                .dst_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::VIDEO_DECODE_DST_KHR)
                .image(self.outputs[o as usize].image)
                .subresource_range(color_range(0, 1));
            // SAFETY: recording; the output image is free (no reader, released).
            unsafe {
                raw.cmd_pipeline_barrier2(
                    commands,
                    &vk::DependencyInfo::default().image_memory_barriers(&[barrier]),
                );
            }
        }

        let begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.parameters)
            .reference_slots(&slots[..=active_count]);
        // SAFETY: recording on the decode queue's family; the session, parameters and
        // pictures are this decoder's.
        unsafe { (self.queue_fns.fp().cmd_begin_video_coding_khr)(commands, &begin) };
        if self.reset {
            let control = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::RESET);
            // SAFETY: inside the coding scope begun above.
            unsafe { (self.queue_fns.fp().cmd_control_video_coding_khr)(commands, &control) };
            self.reset = false;
        }
        let mut av1 = vk::VideoDecodeAV1PictureInfoKHR::default()
            .std_picture_info(&self.picture.info)
            .reference_name_slot_indices(names)
            .frame_header_offset(header_offset)
            .tile_offsets(tile_offsets)
            .tile_sizes(tile_sizes);
        let dst = match (to_output, output_resource) {
            (true, Some(resource)) => resource,
            _ => resources[active_count],
        };
        let decode = vk::VideoDecodeInfoKHR::default()
            .src_buffer(bitstream)
            .src_buffer_offset(0)
            .src_buffer_range(range)
            .dst_picture_resource(dst)
            .setup_reference_slot(&setup)
            .reference_slots(&slots[..active_count])
            .push_next(&mut av1);
        // SAFETY: inside the coding scope; every structure lives until the call returns.
        unsafe { (self.decode_fns.fp().cmd_decode_video_khr)(commands, &decode) };
        // SAFETY: closes the coding scope.
        unsafe {
            (self.queue_fns.fp().cmd_end_video_coding_khr)(
                commands,
                &vk::VideoEndCodingInfoKHR::default(),
            )
        };

        let mut layout = vk::ImageLayout::VIDEO_DECODE_DST_KHR;
        if let Some(o) = output.filter(|_| !to_output) {
            // Coinciding: the picture is copied from its reference slot.
            self.copy_out(
                raw,
                commands,
                slot as u32,
                self.outputs[o as usize].image,
                extent,
            );
            layout = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
        }
        // The next frame's decode waits for this one's writes.
        let barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .dst_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            );
        // SAFETY: recording.
        unsafe {
            raw.cmd_pipeline_barrier2(
                commands,
                &vk::DependencyInfo::default().memory_barriers(&[barrier]),
            );
        }

        // The slot's new picture; AV1's references refreshed to it.
        self.drop_slot_output(slot);
        self.slots[slot].info = setup_info;
        self.slots[slot].extent = extent;
        self.refresh(h.refresh_frame_flags, slot);
        if let Some(o) = output {
            let picture = GpuPicture {
                image: self.outputs[o as usize].image,
                format: self.caps.format,
                width: extent.width,
                height: extent.height,
                render_width: h.render_width,
                render_height: h.render_height,
                layout,
                ready,
                timestamp,
                output: o,
            };
            self.outputs[o as usize].picture = picture;
            if h.show_frame {
                self.outputs[o as usize].users += 1;
                out.push(picture);
            } else if showable && self.slots[slot].held != 0 {
                // Kept with its slot until shown (show_existing_frame) or replaced.
                self.outputs[o as usize].users += 1;
                self.slots[slot].output = Some(o);
            }
        }
        Ok(())
    }

    /// Copies a decoded picture from reference slot `layer` into `output` (both planes).
    fn copy_out(
        &self,
        raw: &engine_rhi::ash::Device,
        commands: vk::CommandBuffer,
        layer: u32,
        output: vk::Image,
        extent: vk::Extent2D,
    ) {
        let barriers = [
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(self.dpb)
                .subresource_range(color_range(layer, 1)),
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(output)
                .subresource_range(color_range(0, 1)),
        ];
        // SAFETY: recording outside a coding scope; both images are this decoder's.
        unsafe {
            raw.cmd_pipeline_barrier2(
                commands,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            );
        }
        let plane = |aspect: vk::ImageAspectFlags, base_layer: u32| vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: base_layer,
            layer_count: 1,
        };
        let chroma = vk::Extent3D {
            width: extent.width.div_ceil(2),
            height: extent.height.div_ceil(2),
            depth: 1,
        };
        let regions = [
            vk::ImageCopy {
                src_subresource: plane(vk::ImageAspectFlags::PLANE_0, layer),
                src_offset: vk::Offset3D::default(),
                dst_subresource: plane(vk::ImageAspectFlags::PLANE_0, 0),
                dst_offset: vk::Offset3D::default(),
                extent: vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                },
            },
            vk::ImageCopy {
                src_subresource: plane(vk::ImageAspectFlags::PLANE_1, layer),
                src_offset: vk::Offset3D::default(),
                dst_subresource: plane(vk::ImageAspectFlags::PLANE_1, 0),
                dst_offset: vk::Offset3D::default(),
                extent: chroma,
            },
        ];
        // SAFETY: recording; the regions are inside both images.
        unsafe {
            raw.cmd_copy_image(
                commands,
                self.dpb,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                output,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
        }
        let back = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .dst_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            )
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
            .image(self.dpb)
            .subresource_range(color_range(layer, 1));
        // SAFETY: recording.
        unsafe {
            raw.cmd_pipeline_barrier2(
                commands,
                &vk::DependencyInfo::default().image_memory_barriers(&[back]),
            );
        }
    }

    /// Copies `data` into submission `index`'s bitstream buffer (grown when too small),
    /// zero-padded to the size alignment.
    fn upload(
        &mut self,
        allocator: &mut Allocator,
        index: usize,
        data: &[u8],
        profiles: [vk::VideoProfileInfoKHR<'static>; 1],
    ) -> Result<(), GpuError> {
        let needed = align(data.len() as u64, self.caps.bitstream_size_alignment);
        let submission = &mut self.submissions[index];
        if submission.capacity < needed {
            if let Some((buffer, allocation)) = submission.bitstream.take() {
                // SAFETY: its last submission finished (the caller waited for it).
                unsafe { allocator.destroy_buffer(buffer, allocation) };
            }
            let capacity = align(
                needed.max(MIN_BITSTREAM).next_power_of_two(),
                self.caps.bitstream_size_alignment,
            );
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let info = vk::BufferCreateInfo::default()
                .size(capacity)
                .usage(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .push_next(&mut list);
            let (buffer, allocation) = allocator.create_buffer(&info, MemoryLocation::Upload)?;
            submission.bitstream = Some((buffer, allocation));
            submission.capacity = capacity;
        }
        let Some((_, allocation)) = &mut submission.bitstream else {
            return Err(GpuError::Unsupported("no bitstream buffer"));
        };
        let mapped = allocation.mapped_slice_mut().ok_or(GpuError::Unsupported(
            "a bitstream buffer the CPU cannot write",
        ))?;
        mapped[..data.len()].copy_from_slice(data);
        mapped[data.len()..needed as usize].fill(0);
        Ok(())
    }

    /// Destroys the decoder: waits for its work, frees its images and memory.
    ///
    /// # Safety
    ///
    /// `device` and `allocator` are those it was made with; no picture it handed out is
    /// read any more.
    pub unsafe fn destroy(mut self, device: &Device, allocator: &mut Allocator) {
        let raw = device.raw();
        if self.semaphore != vk::Semaphore::null() {
            let _ = self.wait(device, self.value);
        }
        // SAFETY: the decoder's work finished (waited above); each object is destroyed
        // once, with the allocator that made it.
        unsafe {
            for submission in &mut self.submissions {
                if let Some((buffer, allocation)) = submission.bitstream.take() {
                    allocator.destroy_buffer(buffer, allocation);
                }
            }
            if self.pool != vk::CommandPool::null() {
                raw.destroy_command_pool(self.pool, None);
            }
            if self.semaphore != vk::Semaphore::null() {
                raw.destroy_semaphore(self.semaphore, None);
            }
            for output in &mut self.outputs {
                if output.view != vk::ImageView::null() {
                    raw.destroy_image_view(output.view, None);
                }
                if let Some(allocation) = output.allocation.take() {
                    allocator.destroy_image(output.image, allocation);
                }
            }
            if self.dpb_view != vk::ImageView::null() {
                raw.destroy_image_view(self.dpb_view, None);
            }
            if let Some(allocation) = self.dpb_allocation.take() {
                allocator.destroy_image(self.dpb, allocation);
            }
            if self.parameters != vk::VideoSessionParametersKHR::null() {
                (self.queue_fns.fp().destroy_video_session_parameters_khr)(
                    raw.handle(),
                    self.parameters,
                    core::ptr::null(),
                );
            }
            if self.session != vk::VideoSessionKHR::null() {
                (self.queue_fns.fp().destroy_video_session_khr)(
                    raw.handle(),
                    self.session,
                    core::ptr::null(),
                );
            }
        }
        for allocation in self.session_memory.drain(..) {
            allocator.free(allocation);
        }
    }
}

/// The profile as the one-element list buffers and images take.
fn profile_list(profile: &Profile) -> [vk::VideoProfileInfoKHR<'static>; 1] {
    [*profile.info()]
}

fn color_range(base_layer: u32, layers: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: base_layer,
        layer_count: layers,
    }
}

/// A view of a picture image with only its video usage (a view of a two-plane format
/// that may be sampled would need a Y'CbCr conversion).
fn picture_view(
    raw: &engine_rhi::ash::Device,
    image: vk::Image,
    format: vk::Format,
    kind: vk::ImageViewType,
    layers: u32,
    usage: vk::ImageUsageFlags,
) -> Result<vk::ImageView, GpuError> {
    let mut view_usage = vk::ImageViewUsageCreateInfo::default().usage(usage);
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(kind)
        .format(format)
        .subresource_range(color_range(0, layers))
        .push_next(&mut view_usage);
    // SAFETY: valid create info for an image of this device.
    unsafe { raw.create_image_view(&info, None) }.map_err(vk_error("vkCreateImageView"))
}

/// `Extent2D` clamped to another.
trait MinWith {
    fn min_with(self, other: Self) -> Self;
}

impl MinWith for vk::Extent2D {
    fn min_with(self, other: Self) -> Self {
        Self {
            width: self.width.min(other.width),
            height: self.height.min(other.height),
        }
    }
}

/// An all-zero reference information.
trait Zeroed {
    fn default_zeroed() -> Self;
}

impl Zeroed for StdVideoDecodeAV1ReferenceInfo {
    fn default_zeroed() -> Self {
        // SAFETY: a plain structure of integers and a bitfield unit.
        unsafe { core::mem::zeroed() }
    }
}
