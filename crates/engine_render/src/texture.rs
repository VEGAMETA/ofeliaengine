//! Textures that live across frames: device-local images registered in the bindless set,
//! filled from the CPU through the upload ring.
//!
//! An upload is a graph pass that copies from the ring and leaves the image in
//! [`vk::ImageLayout::READ_ONLY_OPTIMAL`] for every shader stage. The pass records its
//! own barriers because renderers sample textures through bindless indices without
//! declaring them to the graph: a texture is either being uploaded (inside this pass) or
//! read-only, so the one release barrier at the end of the pass is all later passes
//! need. Large and streamed textures will go through the asset pipeline (CLAUDE.md §13).

use crate::graph::{FrameGraph, TextureDesc};
use crate::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};

/// Bytes per texel of the uncompressed formats textures can be uploaded in.
pub fn bytes_per_texel(format: vk::Format) -> Option<u32> {
    Some(match format {
        vk::Format::R8_UNORM | vk::Format::R8_SRGB => 1,
        vk::Format::R8G8_UNORM => 2,
        vk::Format::R8G8B8A8_UNORM
        | vk::Format::R8G8B8A8_SRGB
        | vk::Format::B8G8R8A8_UNORM
        | vk::Format::B8G8R8A8_SRGB
        | vk::Format::A2B10G10R10_UNORM_PACK32
        | vk::Format::R32_SFLOAT => 4,
        vk::Format::R16G16B16A16_SFLOAT => 8,
        vk::Format::R32G32B32A32_SFLOAT => 16,
        _ => return None,
    })
}

/// Why an upload was not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadError {
    /// The texture's format is not one [`bytes_per_texel`] knows.
    UnsupportedFormat,
    /// `pixels` is not `width × height × bytes_per_texel` long.
    WrongSize {
        /// Expected length.
        expected: usize,
        /// Given length.
        actual: usize,
    },
    /// The upload ring's current slot has no room; retry next frame or grow the ring.
    RingFull,
    /// A region upload before the first whole-texture upload (the rest would be undefined).
    NotInitialized,
    /// The region is not inside the texture.
    OutOfBounds,
}

impl core::fmt::Display for UploadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedFormat => write!(f, "texture format not supported for uploads"),
            Self::WrongSize { expected, actual } => {
                write!(f, "texture data is {actual} bytes, expected {expected}")
            }
            Self::RingFull => write!(f, "upload ring is full"),
            Self::NotInitialized => write!(f, "region upload before the first full upload"),
            Self::OutOfBounds => write!(f, "upload region outside the texture"),
        }
    }
}

impl std::error::Error for UploadError {}

/// A sampled 2D texture without mips, in device-local memory, with a bindless index.
pub struct GpuTexture {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Option<Allocation>,
    desc: TextureDesc,
    sampled_index: u32,
    /// Set by the first whole upload: region uploads keep the rest of the contents.
    initialized: core::sync::atomic::AtomicBool,
}

impl core::fmt::Debug for GpuTexture {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuTexture")
            .field("format", &self.desc.format)
            .field("width", &self.desc.width)
            .field("height", &self.desc.height)
            .field("sampled_index", &self.sampled_index)
            .finish()
    }
}

impl GpuTexture {
    /// Creates the image and registers it in `bindless`. Its contents are undefined until
    /// the first [`Self::upload`]; sampling it before then reads garbage (but is valid).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        format: vk::Format,
        width: u32,
        height: u32,
    ) -> Result<Self, RhiError> {
        Self::with_swizzle(
            device,
            allocator,
            bindless,
            format,
            width,
            height,
            vk::ComponentMapping::default(),
        )
    }

    /// Like [`Self::new`] with a component swizzle on the sampled view. For example an
    /// `R8_UNORM` coverage atlas viewed as `(1, 1, 1, R)` samples as white with coverage in
    /// alpha, so ordinary sprite shaders draw it tinted by the quad color.
    pub fn with_swizzle(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        format: vk::Format,
        width: u32,
        height: u32,
        components: vk::ComponentMapping,
    ) -> Result<Self, RhiError> {
        let desc = TextureDesc::new_2d(format, width, height);
        let (image, allocation) = allocator.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            MemoryLocation::Device,
        )?;
        let d = device.raw();
        // SAFETY: a view of the image just created.
        let view = unsafe {
            d.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .components(components)
                    .subresource_range(COLOR_RANGE),
                None,
            )
        };
        let view = match view {
            Ok(view) => view,
            Err(e) => {
                // SAFETY: never used.
                unsafe { allocator.destroy_image(image, allocation) };
                return Err(RhiError::vk("vkCreateImageView")(e));
            }
        };
        let Some(sampled_index) = bindless.add_sampled_image(view) else {
            // SAFETY: never used.
            unsafe {
                d.destroy_image_view(view, None);
                allocator.destroy_image(image, allocation);
            }
            return Err(RhiError::vk("bindless sampled-image slots")(
                vk::Result::ERROR_OUT_OF_POOL_MEMORY,
            ));
        };
        Ok(Self {
            image,
            view,
            allocation: Some(allocation),
            desc,
            sampled_index,
            initialized: core::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Index in the bindless sampled-image array.
    pub fn sampled_index(&self) -> u32 {
        self.sampled_index
    }

    /// Size and format.
    pub fn desc(&self) -> TextureDesc {
        self.desc
    }

    /// Whether a whole-texture upload was recorded.
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The image.
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// Declares a pass that replaces the whole texture with `pixels` (rows tightly
    /// packed). The data is copied into `ring` now; the texture is readable by every
    /// shader stage in passes declared after this one.
    pub fn upload<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        pixels: &[u8],
    ) -> Result<(), UploadError> {
        let texel = bytes_per_texel(self.desc.format).ok_or(UploadError::UnsupportedFormat)?;
        let expected = (self.desc.width * self.desc.height * texel) as usize;
        if pixels.len() != expected {
            return Err(UploadError::WrongSize {
                expected,
                actual: pixels.len(),
            });
        }
        let staged = ring.stage(pixels).ok_or(UploadError::RingFull)?;
        self.initialized
            .store(true, core::sync::atomic::Ordering::Relaxed);
        let (image, desc) = (self.image, self.desc);
        fg.add_pass("texture upload")
            .side_effects()
            .execute(move |ctx| {
                let to_transfer = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(COLOR_RANGE);
                let to_read = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COPY)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(COLOR_RANGE);
                let region = vk::BufferImageCopy {
                    buffer_offset: staged.offset,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: desc.width,
                        height: desc.height,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording outside a rendering scope; the image is owned by the
                // texture, which outlives the frame ('g), and not in use by earlier passes
                // (textures are only sampled, and samplers of the previous contents
                // finished before the ALL_COMMANDS source scope of the first barrier).
                unsafe {
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(core::slice::from_ref(&to_transfer)),
                    );
                    ctx.device.cmd_copy_buffer_to_image(
                        ctx.cmd,
                        staged.buffer,
                        image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(core::slice::from_ref(&to_read)),
                    );
                }
            });
        Ok(())
    }

    /// Declares a pass that replaces the `width × height` region at (`x`, `y`) with
    /// `pixels` (rows tightly packed) and keeps the rest of the texture. Requires one
    /// whole [`Self::upload`] first.
    pub fn upload_region<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        (x, y, width, height): (u32, u32, u32, u32),
        pixels: &[u8],
    ) -> Result<(), UploadError> {
        if !self.initialized.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(UploadError::NotInitialized);
        }
        if width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|r| r > self.desc.width)
            || y.checked_add(height).is_none_or(|b| b > self.desc.height)
        {
            return Err(UploadError::OutOfBounds);
        }
        let texel = bytes_per_texel(self.desc.format).ok_or(UploadError::UnsupportedFormat)?;
        let expected = (width * height * texel) as usize;
        if pixels.len() != expected {
            return Err(UploadError::WrongSize {
                expected,
                actual: pixels.len(),
            });
        }
        let staged = ring.stage(pixels).ok_or(UploadError::RingFull)?;
        let image = self.image;
        fg.add_pass("texture region upload")
            .side_effects()
            .execute(move |ctx| {
                let barrier = |old, new, src_stage, src_access, dst_stage, dst_access| {
                    vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(src_stage)
                        .src_access_mask(src_access)
                        .dst_stage_mask(dst_stage)
                        .dst_access_mask(dst_access)
                        .old_layout(old)
                        .new_layout(new)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(COLOR_RANGE)
                };
                let to_transfer = barrier(
                    vk::ImageLayout::READ_ONLY_OPTIMAL,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::PipelineStageFlags2::ALL_COMMANDS,
                    vk::AccessFlags2::NONE,
                    vk::PipelineStageFlags2::COPY,
                    vk::AccessFlags2::TRANSFER_WRITE,
                );
                let to_read = barrier(
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::COPY,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    vk::PipelineStageFlags2::ALL_COMMANDS,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                );
                let region = vk::BufferImageCopy {
                    buffer_offset: staged.offset,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: vk::Offset3D {
                        x: x as i32,
                        y: y as i32,
                        z: 0,
                    },
                    image_extent: vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: as in `upload`; the image is in READ_ONLY_OPTIMAL since the
                // first whole upload (every upload pass ends there), so its contents
                // outside the region are kept.
                unsafe {
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(core::slice::from_ref(&to_transfer)),
                    );
                    ctx.device.cmd_copy_buffer_to_image(
                        ctx.cmd,
                        staged.buffer,
                        image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(core::slice::from_ref(&to_read)),
                    );
                }
            });
        Ok(())
    }

    /// Unregisters and frees the texture.
    ///
    /// # Safety
    ///
    /// No submitted work that uses it is still pending except frames up to `last_frame`
    /// (its bindless slot is reused after that frame completes), and the image and view
    /// themselves are no longer used by the GPU.
    pub unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        last_frame: u64,
    ) {
        if let Some(allocation) = self.allocation.take() {
            bindless.remove_sampled_image(self.sampled_index, last_frame);
            // SAFETY: guaranteed by the caller.
            unsafe {
                device.raw().destroy_image_view(self.view, None);
                allocator.destroy_image(self.image, allocation);
            }
        }
    }
}

const COLOR_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};
