//! Per-view state kept across frames.

use crate::antialiasing::Antialiasing;
use crate::settings::ViewSettings;
use engine_render::forward::{Occlusion, Temporal};
use engine_render::graph::TextureState;
use engine_render::postfx::AutoExposure;
use engine_render::reflections::ScreenSpaceReflections;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};
use engine_upscale::Upscaler;

/// What [`crate::FrameRenderer::prepare`] settled for a view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prepared {
    /// The settings applied.
    pub settings: ViewSettings,
    /// The antialiasing used: the requested one, or TAAU when the requested upscaler is
    /// unavailable.
    pub antialiasing: Antialiasing,
    /// Output size in pixels.
    pub output: (u32, u32),
    /// Render size in pixels (the scene passes' targets).
    pub render: (u32, u32),
    /// The render size `quality` gives (the LOD bias follows it, not the dynamic scale).
    pub nominal_render: (u32, u32),
    /// Jitter phases (0: none).
    pub jitter_phases: u32,
    /// The texture LOD bias for this view.
    pub lod_bias: f32,
    /// With [`crate::Upscale::Integer`]: the whole factor each scene pixel is shown at,
    /// and where the scaled image starts in the output (the rest is black).
    pub integer: Option<(u32, (u32, u32))>,
}

/// Why a view's history started over in its last frame (for tests and debugging).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Resets {
    /// Camera cuts.
    pub cuts: u64,
    /// Output size, upscaler or antialiasing changes.
    pub changes: u64,
}

/// A persistent image of a view (its previous antialiased color).
#[derive(Debug)]
pub(crate) struct HistoryImage {
    pub(crate) image: vk::Image,
    pub(crate) view: vk::ImageView,
    allocation: Option<Allocation>,
    pub(crate) sampled: u32,
    pub(crate) state: TextureState,
    pub(crate) size: (u32, u32),
    pub(crate) format: vk::Format,
    /// Whether it holds last frame's color.
    pub(crate) valid: bool,
}

impl HistoryImage {
    pub(crate) fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        format: vk::Format,
        size: (u32, u32),
    ) -> Result<Self, RhiError> {
        let (image, allocation) = allocator.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width: size.0,
                    height: size.1,
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
        // SAFETY: a view of the image just created.
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
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
                // SAFETY: never used.
                unsafe { allocator.destroy_image(image, allocation) };
                return Err(RhiError::vk("vkCreateImageView")(e));
            }
        };
        let Some(sampled) = bindless.add_sampled_image(view) else {
            // SAFETY: never used.
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
            view,
            allocation: Some(allocation),
            sampled,
            state: TextureState::undefined(),
            size,
            format,
            valid: false,
        })
    }

    /// # Safety
    ///
    /// The GPU no longer uses the image after `frame`.
    pub(crate) unsafe fn destroy(
        mut self,
        device: &engine_rhi::ash::Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
    ) {
        bindless.remove_sampled_image(self.sampled, frame);
        // SAFETY: guaranteed by the caller.
        unsafe {
            device.destroy_image_view(self.view, None);
            if let Some(allocation) = self.allocation.take() {
                allocator.destroy_image(self.image, allocation);
            }
        }
    }
}

/// Something a view stopped using, freed once the frames that used it are done.
// Settings changes are rare and the list is short: no boxing for the size difference.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Retired {
    Upscaler(Box<dyn Upscaler>),
    Temporal(Temporal),
    Occlusion(Occlusion),
    Exposure(AutoExposure),
    History(HistoryImage),
    Reflections(ScreenSpaceReflections),
}

/// One view's state across frames (UE's `FSceneViewState`): its temporal state (jitter,
/// last frame's camera and instances), its upscaler and history, its occlusion-culling
/// state, its adapted exposure and its previous color. One per camera that renders:
/// the main view, each split-screen player, each render target.
///
/// Create it with [`View::new`], configure it every frame with
/// [`crate::FrameRenderer::prepare`] (cheap when nothing changed), draw it with
/// [`crate::FrameRenderer::add_view`], and free it with [`View::destroy`] when the GPU
/// is idle.
pub struct View {
    pub(crate) device: engine_rhi::ash::Device,
    /// Instances whose transforms and visibility are remembered (draw lists; a
    /// `GpuScene` keeps its own).
    pub(crate) capacity: u32,
    pub(crate) prepared: Option<Prepared>,
    pub(crate) temporal: Option<Temporal>,
    pub(crate) occlusion: Option<Occlusion>,
    pub(crate) upscaler: Option<Box<dyn Upscaler>>,
    pub(crate) exposure: Option<AutoExposure>,
    pub(crate) history: Option<HistoryImage>,
    /// Screen-space reflections' history (ADR 0073), at the render size.
    pub(crate) reflections: Option<ScreenSpaceReflections>,
    pub(crate) retired: Vec<(u64, Retired)>,
    pub(crate) resets: Resets,
    /// The next frame starts its history anew (a change found by `prepare`).
    pub(crate) reset_pending: bool,
}

impl core::fmt::Debug for View {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("View")
            .field("prepared", &self.prepared)
            .field("resets", &self.resets)
            .finish_non_exhaustive()
    }
}

impl View {
    /// A view remembering up to `capacity` draw items (for motion vectors and occlusion
    /// culling of draw lists: 64 + 4 bytes each; a `GpuScene` view needs at least its
    /// `slot_count` for occlusion culling).
    pub fn new(device: &Device, capacity: u32) -> Self {
        Self {
            device: device.raw().clone(),
            capacity: capacity.max(1),
            prepared: None,
            temporal: None,
            occlusion: None,
            upscaler: None,
            exposure: None,
            history: None,
            reflections: None,
            retired: Vec::new(),
            resets: Resets::default(),
            reset_pending: false,
        }
    }

    /// What the last [`crate::FrameRenderer::prepare`] settled, if it ran.
    pub fn prepared(&self) -> Option<&Prepared> {
        self.prepared.as_ref()
    }

    /// The view's upscaler, when it is antialiased (to set its own options: FSR's
    /// sharpness, DLSS's preset; downcast through your own handle if you need them).
    pub fn upscaler_mut(&mut self) -> Option<&mut dyn Upscaler> {
        match &mut self.upscaler {
            Some(upscaler) => Some(&mut **upscaler),
            None => None,
        }
    }

    /// The view's temporal state (jitter, motion vectors), when it has one.
    pub fn temporal(&self) -> Option<&Temporal> {
        self.temporal.as_ref()
    }

    /// How often the history started over.
    pub fn resets(&self) -> Resets {
        self.resets
    }

    pub(crate) fn retire(&mut self, frame: u64, what: Retired) {
        self.retired.push((frame, what));
    }

    /// Frees what was retired before `completed`.
    pub(crate) fn collect(
        &mut self,
        allocator: &mut Allocator,
        bindless: &Bindless,
        completed: Option<u64>,
    ) {
        let mut i = 0;
        while i < self.retired.len() {
            if completed.is_some_and(|c| self.retired[i].0 > c) {
                i += 1;
                continue;
            }
            let (frame, what) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it are done (`completed`), or the device is
            // idle (`completed` is `None`, from `destroy`).
            unsafe { free(&self.device, what, allocator, bindless, frame) };
        }
    }

    /// Frees everything the view holds.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        let frame = self
            .retired
            .iter()
            .map(|r| r.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        for what in [
            self.upscaler.take().map(Retired::Upscaler),
            self.temporal.take().map(Retired::Temporal),
            self.occlusion.take().map(Retired::Occlusion),
            self.exposure.take().map(Retired::Exposure),
            self.history.take().map(Retired::History),
            self.reflections.take().map(Retired::Reflections),
        ]
        .into_iter()
        .flatten()
        {
            self.retired.push((frame, what));
        }
        self.collect(allocator, bindless, None);
        self.prepared = None;
    }
}

/// # Safety
///
/// The GPU no longer uses `what`.
unsafe fn free(
    device: &engine_rhi::ash::Device,
    what: Retired,
    allocator: &mut Allocator,
    bindless: &Bindless,
    frame: u64,
) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        match what {
            Retired::Upscaler(mut upscaler) => upscaler.destroy(allocator, bindless),
            Retired::Temporal(mut temporal) => temporal.destroy(allocator),
            Retired::Occlusion(mut occlusion) => occlusion.destroy(allocator),
            Retired::Exposure(mut exposure) => exposure.destroy(allocator),
            Retired::History(image) => image.destroy(device, allocator, bindless, frame),
            Retired::Reflections(mut ssr) => ssr.destroy(allocator, bindless),
        }
    }
}
