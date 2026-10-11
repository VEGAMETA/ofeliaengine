//! The atlas local lights' shadows are drawn into (ADR 0071), owned by the forward renderer
//! while they are on.

use super::*;

/// The atlas of local lights' shadows: a depth image that lives across frames.
#[derive(Debug)]
pub(super) struct AtlasImage {
    pub(super) image: vk::Image,
    pub(super) allocation: Option<Allocation>,
    pub(super) view: vk::ImageView,
    pub(super) sampled: u32,
    pub(super) desc: TextureDesc,
    pub(super) state: Cell<crate::graph::TextureState>,
}

impl AtlasImage {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        settings: &LocalShadowSettings,
    ) -> Result<Self, RhiError> {
        let size = settings.atlas_size;
        let (image, allocation) = allocator.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(settings.format)
                .extent(vk::Extent3D {
                    width: size,
                    height: size,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            MemoryLocation::Device,
        )?;
        // SAFETY: a view of the image just created.
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(settings.format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::DEPTH,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        };
        let view = match view {
            Ok(view) => view,
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { allocator.destroy_image(image, allocation) };
                return Err(RhiError::vk("vkCreateImageView")(e));
            }
        };
        let Some(sampled) = bindless.add_sampled_image(view) else {
            // SAFETY: never used by the GPU.
            unsafe {
                device.raw().destroy_image_view(view, None);
                allocator.destroy_image(image, allocation);
            }
            return Err(RhiError::Vulkan {
                call: "bindless set full",
                result: vk::Result::ERROR_OUT_OF_POOL_MEMORY,
            });
        };
        Ok(Self {
            image,
            allocation: Some(allocation),
            view,
            sampled,
            desc: TextureDesc::new_2d(settings.format, size, size),
            state: Cell::new(crate::graph::TextureState::undefined()),
        })
    }

    /// # Safety
    ///
    /// The GPU no longer uses the image.
    pub(super) unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) {
        if let Some(allocation) = self.allocation.take() {
            bindless.remove_sampled_image(self.sampled, 0);
            // SAFETY: guaranteed by the caller.
            unsafe {
                device.raw().destroy_image_view(self.view, None);
                allocator.destroy_image(self.image, allocation);
            }
        }
    }
}

/// Local lights' shadows (ADR 0071): the atlas, its cache and the pipelines, made when the
/// feature is first turned on.
#[derive(Debug)]
pub(super) struct LocalShadowState {
    pub(super) atlas: AtlasImage,
    pub(super) pipelines: ShadowedPipelines,
    pub(super) cache: RefCell<ShadowCache>,
    pub(super) plan: RefCell<Plan>,
    /// The frame (graph serial) the atlas was last imported into, and its handle there.
    pub(super) imported: Cell<Option<(u64, Texture)>>,
    /// The tiles drawn in that frame, by every view (each view's pass records its range).
    pub(super) rects: RefCell<Vec<vk::Rect2D>>,
}
