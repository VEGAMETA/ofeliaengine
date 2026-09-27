//! Textures of materials (ADR 0040): images with full mip chains, uploaded through a
//! staging buffer within a budget of bytes per frame, and registered in the bindless set
//! once uploaded. Until then materials shade without them (their factors alone), so a
//! scene appears at once and its textures fill in over the next frames.
//!
//! Uploads record their own layout transitions: materials sample textures through
//! bindless indices without declaring them to the graph, so a texture is either being
//! uploaded (inside the upload pass) or read-only.

use crate::graph::{Access, BufferState, FrameGraph, ImportedBuffer};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};
use std::collections::VecDeque;

/// A texture in a [`TexturePool`]. Ids of removed textures are stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId {
    index: u32,
    generation: u32,
}

/// A texture to add: RGBA8 levels from the full size down, each `max(1, size >> level)`
/// on a side, rows tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureLevels {
    /// Width of level 0.
    pub width: u32,
    /// Height of level 0.
    pub height: u32,
    /// sRGB-encoded color (the view decodes it); linear data otherwise.
    pub srgb: bool,
    /// The levels, at least one.
    pub levels: Vec<Vec<u8>>,
}

/// Why a texture was not added.
#[derive(Debug)]
pub enum TextureError {
    /// No levels, or a level whose size does not match its dimensions.
    BadLevels,
    /// A Vulkan call failed.
    Rhi(RhiError),
}

impl From<RhiError> for TextureError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

impl core::fmt::Display for TextureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadLevels => write!(f, "texture levels do not match its size"),
            Self::Rhi(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TextureError {}

#[derive(Debug)]
struct PoolTexture {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Option<Allocation>,
    width: u32,
    height: u32,
    /// Bindless index once uploaded.
    bindless: Option<u32>,
    /// Level data waiting for upload.
    pending: Option<Vec<Vec<u8>>>,
    bytes: u64,
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    texture: Option<PoolTexture>,
}

#[derive(Debug)]
enum Retired {
    Texture(PoolTexture),
    Staging(vk::Buffer, Allocation),
}

/// Material textures (see the module documentation).
pub struct TexturePool {
    device: engine_rhi::ash::Device,
    slots: Vec<Slot>,
    free_slots: Vec<u32>,
    queue: VecDeque<TextureId>,
    budget: u64,
    retired: Vec<(u64, Retired)>,
    frames_in_flight: u64,
    /// Changes whenever textures become ready or are removed: materials resolve their
    /// textures again when it does.
    generation: u64,
    uploaded_bytes: u64,
}

impl core::fmt::Debug for TexturePool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TexturePool")
            .field("textures", &(self.slots.len() - self.free_slots.len()))
            .field("queued", &self.queue.len())
            .field("budget", &self.budget)
            .field("uploaded_bytes", &self.uploaded_bytes)
            .finish()
    }
}

/// Size of level `level` of a `size`-texel side.
fn level_size(size: u32, level: usize) -> u32 {
    (size >> level).max(1)
}

impl TexturePool {
    /// An empty pool uploading at most `budget` bytes per frame (at least one texture,
    /// however large); frames in flight bound how long removed textures are kept.
    pub fn new(device: &Device, budget: u64, frames_in_flight: usize) -> Self {
        Self {
            device: device.raw().clone(),
            slots: Vec::new(),
            free_slots: Vec::new(),
            queue: VecDeque::new(),
            budget,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            generation: 0,
            uploaded_bytes: 0,
        }
    }

    /// Creates a texture's image now and queues its levels for upload.
    pub fn add(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        texture: TextureLevels,
    ) -> Result<TextureId, TextureError> {
        let (width, height) = (texture.width.max(1), texture.height.max(1));
        if texture.levels.is_empty()
            || texture.levels.len() > 32
            || texture.levels.iter().enumerate().any(|(k, level)| {
                level.len() != (level_size(width, k) * level_size(height, k) * 4) as usize
            })
        {
            return Err(TextureError::BadLevels);
        }
        let format = if texture.srgb {
            vk::Format::R8G8B8A8_SRGB
        } else {
            vk::Format::R8G8B8A8_UNORM
        };
        let level_count = texture.levels.len() as u32;
        let (image, allocation) = allocator.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(level_count)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            MemoryLocation::Device,
        )?;
        // SAFETY: a view of the image just created, over all its levels.
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        };
        let view = match view {
            Ok(view) => view,
            Err(e) => {
                // SAFETY: never used.
                unsafe { allocator.destroy_image(image, allocation) };
                return Err(RhiError::vk("vkCreateImageView")(e).into());
            }
        };
        let bytes = texture.levels.iter().map(|l| l.len() as u64).sum();
        let entry = PoolTexture {
            image,
            view,
            allocation: Some(allocation),
            width,
            height,
            bindless: None,
            pending: Some(texture.levels),
            bytes,
        };
        let index = match self.free_slots.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    texture: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.texture = Some(entry);
        let id = TextureId {
            index,
            generation: slot.generation,
        };
        self.queue.push_back(id);
        Ok(id)
    }

    /// Removes a texture: materials that use it shade without it from the next
    /// [`crate::material::MaterialTable::begin_frame`] on; its memory is freed once the
    /// frames up to `frame_number` (the current one) have finished.
    pub fn remove(&mut self, id: TextureId, frame_number: u64, bindless: &Bindless) {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return;
        };
        if slot.generation != id.generation {
            return;
        }
        if let Some(texture) = slot.texture.take() {
            if let Some(index) = texture.bindless {
                bindless.remove_sampled_image(index, frame_number);
                self.generation += 1;
            }
            self.retired.push((frame_number, Retired::Texture(texture)));
        }
        slot.generation = slot.generation.wrapping_add(1);
        self.free_slots.push(id.index);
    }

    /// The bindless index of a texture once uploaded; `None` before, and for stale ids.
    pub fn index(&self, id: TextureId) -> Option<u32> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.texture.as_ref()?.bindless
    }

    /// A counter that changes whenever textures become ready or are removed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Textures waiting for upload, and their bytes.
    pub fn pending(&self) -> (usize, u64) {
        let bytes = self
            .queue
            .iter()
            .filter_map(|id| self.slots[id.index as usize].texture.as_ref())
            .map(|t| t.bytes)
            .sum();
        (self.queue.len(), bytes)
    }

    /// Bytes uploaded so far.
    pub fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }

    /// Frees what finished frames no longer use and uploads queued textures, oldest
    /// first, up to the budget. Uploaded textures are readable (and have bindless indices)
    /// in every pass declared after this call. Allocates only on frames that upload.
    pub fn begin_frame(
        &mut self,
        fg: &mut FrameGraph<'_>,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame_number: u64,
    ) -> Result<(), RhiError> {
        engine_profiler::zone!("render::TexturePool::begin_frame");
        self.collect_retired(allocator, frame_number);
        if self.queue.is_empty() {
            return Ok(());
        }
        // This frame's batch: the oldest textures within the budget.
        let mut batch = Vec::new();
        let mut total = 0u64;
        while let Some(&id) = self.queue.front() {
            let bytes = match self.slots[id.index as usize].texture.as_ref() {
                Some(t) if self.slots[id.index as usize].generation == id.generation => t.bytes,
                _ => {
                    // Removed before its upload.
                    self.queue.pop_front();
                    continue;
                }
            };
            if !batch.is_empty() && total + bytes > self.budget {
                break;
            }
            self.queue.pop_front();
            batch.push(id);
            total += bytes;
        }
        if batch.is_empty() {
            return Ok(());
        }
        let (staging, mut staging_memory) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(total.max(16))
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )?;
        let mut images = Vec::with_capacity(batch.len());
        let mut regions: Vec<(vk::Image, Vec<vk::BufferImageCopy>)> =
            Vec::with_capacity(batch.len());
        let mut offset = 0u64;
        {
            let Some(target) = staging_memory.mapped_slice_mut() else {
                return Err(RhiError::Vulkan {
                    call: "staging buffer not mapped",
                    result: vk::Result::ERROR_MEMORY_MAP_FAILED,
                });
            };
            for id in &batch {
                let Some(texture) = self.slots[id.index as usize].texture.as_mut() else {
                    continue;
                };
                let levels = texture.pending.take().unwrap_or_default();
                let mut copies = Vec::with_capacity(levels.len());
                for (k, level) in levels.iter().enumerate() {
                    target[offset as usize..offset as usize + level.len()].copy_from_slice(level);
                    copies.push(vk::BufferImageCopy {
                        buffer_offset: offset,
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: k as u32,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_extent: vk::Extent3D {
                            width: level_size(texture.width, k),
                            height: level_size(texture.height, k),
                            depth: 1,
                        },
                        ..Default::default()
                    });
                    offset += level.len() as u64;
                }
                images.push((texture.image, levels.len() as u32));
                regions.push((texture.image, copies));
            }
        }
        let staging_handle = fg.import_buffer(
            "texture staging",
            ImportedBuffer {
                buffer: staging,
                size: total.max(16),
                address: 0,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        fg.add_pass("texture pool upload")
            .read(staging_handle, Access::TransferRead)
            .side_effects()
            .execute(move |ctx| {
                let range = |levels| vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: levels,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let to_transfer: Vec<_> = images
                    .iter()
                    .map(|&(image, levels)| {
                        vk::ImageMemoryBarrier2::default()
                            .src_stage_mask(vk::PipelineStageFlags2::NONE)
                            .src_access_mask(vk::AccessFlags2::NONE)
                            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                            .old_layout(vk::ImageLayout::UNDEFINED)
                            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(image)
                            .subresource_range(range(levels))
                    })
                    .collect();
                let to_read: Vec<_> = images
                    .iter()
                    .map(|&(image, levels)| {
                        vk::ImageMemoryBarrier2::default()
                            .src_stage_mask(vk::PipelineStageFlags2::COPY)
                            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .new_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(image)
                            .subresource_range(range(levels))
                    })
                    .collect();
                let source = ctx.buffer(staging_handle);
                // SAFETY: recording outside a rendering scope; the images are new (never
                // used before these barriers) and owned by the pool, which frees them only
                // after the frames that used them.
                unsafe {
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&to_transfer),
                    );
                    for (image, copies) in &regions {
                        ctx.device.cmd_copy_buffer_to_image(
                            ctx.cmd,
                            source,
                            *image,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            copies,
                        );
                    }
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&to_read),
                    );
                }
            });
        for id in batch {
            if let Some(texture) = self.slots[id.index as usize].texture.as_mut() {
                texture.bindless = bindless.add_sampled_image(texture.view);
                self.uploaded_bytes += texture.bytes;
            }
        }
        self.generation += 1;
        self.retired
            .push((frame_number, Retired::Staging(staging, staging_memory)));
        Ok(())
    }

    fn collect_retired(&mut self, allocator: &mut Allocator, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, retired) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it have finished.
            unsafe { self.destroy_retired(allocator, retired) };
        }
    }

    /// # Safety
    ///
    /// The GPU no longer uses it.
    unsafe fn destroy_retired(&self, allocator: &mut Allocator, retired: Retired) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            match retired {
                Retired::Texture(mut texture) => {
                    self.device.destroy_image_view(texture.view, None);
                    if let Some(allocation) = texture.allocation.take() {
                        allocator.destroy_image(texture.image, allocation);
                    }
                }
                Retired::Staging(buffer, allocation) => {
                    allocator.destroy_buffer(buffer, allocation)
                }
            }
        }
    }

    /// Frees every texture.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the pool (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        let slots = core::mem::take(&mut self.slots);
        for slot in slots {
            if let Some(texture) = slot.texture {
                if let Some(index) = texture.bindless {
                    bindless.remove_sampled_image(index, 0);
                }
                // SAFETY: guaranteed by the caller.
                unsafe { self.destroy_retired(allocator, Retired::Texture(texture)) };
            }
        }
        for (_, retired) in core::mem::take(&mut self.retired) {
            // SAFETY: guaranteed by the caller.
            unsafe { self.destroy_retired(allocator, retired) };
        }
    }
}
