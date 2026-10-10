//! Where a frame is drawn before its swapchain shows it (core review 2 §3.4).
//!
//! A swapchain presents across adapters when its window's monitor is driven by another GPU
//! than the one rendering (a desktop's integrated GPU behind a monitor on the discrete one;
//! a laptop's discrete GPU behind its integrated one;
//! [`Swapchain::presents_across_adapters`]). Its images are then shared with that GPU, and
//! shading into them is slow: on ref-1's Radeon iGPU behind a monitor on the RTX, the tone
//! mapper took 2.56 ms into the swapchain image against 0.82 ms into an image of its own,
//! which a copy then brings to the swapchain image in 0.62 ms. There the frame is drawn
//! into an image of the swapchain's format and size and copied at the end; elsewhere
//! straight into the swapchain image (a copy would only cost: 19.5 µs on the RTX).
//!
//! ```ignore
//! let target = PresentTarget::import(&mut fg, &swapchain, acquired.index);
//! // ... every pass that draws the frame writes `target.texture` ...
//! target.finish(&mut fg);
//! ```

use crate::graph::{Access, FrameGraph, ImportedTexture, Texture, TextureDesc, TextureState};
use engine_rhi::Swapchain;
use engine_rhi::ash::vk;

/// The texture a frame draws into for presentation: the acquired swapchain image, or an
/// image of its own that [`Self::finish`] copies into it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[must_use = "`finish` copies the frame into the swapchain image when it was drawn apart"]
pub struct PresentTarget {
    /// What the frame's passes draw into.
    pub texture: Texture,
    /// The swapchain image, when `texture` is an image of its own.
    swapchain_image: Option<Texture>,
}

impl PresentTarget {
    /// Imports `swapchain`'s acquired image `index` into the frame and picks where the frame
    /// is drawn: an image of its own when the swapchain presents across adapters.
    pub fn import(fg: &mut FrameGraph<'_>, swapchain: &Swapchain, index: u32) -> Self {
        Self::import_with(fg, swapchain, index, swapchain.presents_across_adapters())
    }

    /// [`Self::import`] with the choice made by the caller (`apart`: draw into an image of
    /// its own and copy), for measurements.
    pub fn import_with(
        fg: &mut FrameGraph<'_>,
        swapchain: &Swapchain,
        index: u32,
        apart: bool,
    ) -> Self {
        let extent = swapchain.extent();
        let desc = TextureDesc::new_2d(swapchain.format(), extent.width, extent.height);
        let image = fg.import_texture(
            "backbuffer",
            ImportedTexture {
                image: swapchain.images()[index as usize],
                view: swapchain.views()[index as usize],
                desc,
                initial: TextureState::acquired_swapchain_image(),
                final_state: Some(TextureState::present()),
                sampled_index: None,
                storage_index: None,
            },
        );
        if apart {
            Self {
                texture: fg.create_texture("frame", desc),
                swapchain_image: Some(image),
            }
        } else {
            Self {
                texture: image,
                swapchain_image: None,
            }
        }
    }

    /// Whether the frame is drawn into an image of its own.
    pub fn apart(&self) -> bool {
        self.swapchain_image.is_some()
    }

    /// Copies the frame into the swapchain image when it was drawn apart: declare it after
    /// every pass that draws the frame.
    pub fn finish(self, fg: &mut FrameGraph<'_>) {
        let Some(image) = self.swapchain_image else {
            return;
        };
        let frame = self.texture;
        let desc = fg.texture_desc(frame);
        fg.add_pass("copy to swapchain")
            .read(frame, Access::TransferRead)
            .write(image, Access::TransferWrite)
            .execute(move |ctx| {
                let layers = vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let region = vk::ImageCopy {
                    src_subresource: layers,
                    dst_subresource: layers,
                    extent: vk::Extent3D {
                        width: desc.width,
                        height: desc.height,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording; the graph arranged both layouts, and the images have
                // the same format and size.
                unsafe {
                    ctx.device.cmd_copy_image(
                        ctx.cmd,
                        ctx.image(frame),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.image(image),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                }
            });
    }
}
