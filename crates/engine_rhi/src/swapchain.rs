//! Surfaces, swapchains and presentation.

use crate::device::{Device, Queue};
use crate::error::RhiError;
use crate::instance::{Instance, InstanceShared};
use ash::{ext, khr, vk};
use std::sync::Arc;

/// A presentation surface: a window, or headless for tests.
pub struct Surface {
    pub(crate) raw: vk::SurfaceKHR,
    pub(crate) loader: khr::surface::Instance,
    _instance: Arc<InstanceShared>,
}

impl core::fmt::Debug for Surface {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Surface").field("raw", &self.raw).finish()
    }
}

impl Surface {
    /// Creates a surface for a Win32 window (`HINSTANCE`, `HWND` as integers, as in
    /// `engine_platform::window::WindowHandle`).
    #[cfg(windows)]
    pub fn from_win32(
        instance: &Instance,
        hinstance: isize,
        hwnd: isize,
    ) -> Result<Self, RhiError> {
        if !instance.shared.win32_surface_enabled {
            return Err(RhiError::Vulkan {
                call: "VK_KHR_win32_surface not enabled",
                result: vk::Result::ERROR_EXTENSION_NOT_PRESENT,
            });
        }
        let loader = khr::win32_surface::Instance::new(instance.entry(), instance.raw());
        let info = vk::Win32SurfaceCreateInfoKHR::default()
            .hinstance(hinstance)
            .hwnd(hwnd);
        // SAFETY: the caller passes handles of a live window; the surface is destroyed
        // before the instance (it holds an `Arc` to it).
        let raw = unsafe { loader.create_win32_surface(&info, None) }
            .map_err(RhiError::vk("vkCreateWin32SurfaceKHR"))?;
        Ok(Self {
            raw,
            loader: khr::surface::Instance::new(instance.entry(), instance.raw()),
            _instance: instance.shared.clone(),
        })
    }

    /// Creates a headless surface (`VK_EXT_headless_surface`; enable it with
    /// `InstanceDesc::headless_surface`). Used by tests to exercise swapchains without a
    /// window.
    pub fn headless(instance: &Instance) -> Result<Self, RhiError> {
        if !instance.shared.headless_surface_enabled {
            return Err(RhiError::Vulkan {
                call: "VK_EXT_headless_surface not enabled",
                result: vk::Result::ERROR_EXTENSION_NOT_PRESENT,
            });
        }
        let loader = ext::headless_surface::Instance::new(instance.entry(), instance.raw());
        // SAFETY: valid create info; destroyed before the instance.
        let raw = unsafe {
            loader.create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None)
        }
        .map_err(RhiError::vk("vkCreateHeadlessSurfaceEXT"))?;
        Ok(Self {
            raw,
            loader: khr::surface::Instance::new(instance.entry(), instance.raw()),
            _instance: instance.shared.clone(),
        })
    }

    /// Whether `queue` can present to this surface.
    pub fn supports_queue(&self, device: &Device, queue: Queue) -> Result<bool, RhiError> {
        // SAFETY: the physical device and surface belong to the same instance.
        unsafe {
            self.loader.get_physical_device_surface_support(
                device.physical(),
                queue.family,
                self.raw,
            )
        }
        .map_err(RhiError::vk("vkGetPhysicalDeviceSurfaceSupportKHR"))
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: swapchains borrow the surface for creation only and must be dropped
        // first (documented on `Swapchain`); the instance is kept alive by `_instance`.
        unsafe { self.loader.destroy_surface(self.raw, None) };
    }
}

/// How presented images reach the screen (user-selectable, CLAUDE.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    /// V-sync; always available. Lowest power.
    Fifo,
    /// V-sync, but late frames tear instead of waiting a whole refresh.
    FifoRelaxed,
    /// No tearing, newest frame wins; lowest latency without tearing.
    Mailbox,
    /// No v-sync; tearing; lowest latency.
    Immediate,
}

impl PresentMode {
    fn raw(self) -> vk::PresentModeKHR {
        match self {
            Self::Fifo => vk::PresentModeKHR::FIFO,
            Self::FifoRelaxed => vk::PresentModeKHR::FIFO_RELAXED,
            Self::Mailbox => vk::PresentModeKHR::MAILBOX,
            Self::Immediate => vk::PresentModeKHR::IMMEDIATE,
        }
    }

    fn from_raw(raw: vk::PresentModeKHR) -> Self {
        match raw {
            vk::PresentModeKHR::FIFO_RELAXED => Self::FifoRelaxed,
            vk::PresentModeKHR::MAILBOX => Self::Mailbox,
            vk::PresentModeKHR::IMMEDIATE => Self::Immediate,
            _ => Self::Fifo,
        }
    }
}

/// Parameters for [`Swapchain::new`].
#[derive(Debug, Clone, Copy)]
pub struct SwapchainDesc {
    /// Width in pixels, used when the surface does not dictate its size.
    pub width: u32,
    /// Height in pixels, used when the surface does not dictate its size.
    pub height: u32,
    /// Requested present mode; falls back to [`PresentMode::Fifo`] if unsupported.
    pub present_mode: PresentMode,
    /// Prefer an sRGB format (the GPU encodes on write).
    pub srgb: bool,
    /// Create the swapchain for NVIDIA's low-latency mode (`VK_NV_low_latency2`, Reflex)
    /// when the device has it; `engine_latency` drives it.
    pub low_latency: bool,
}

/// Result of [`Swapchain::acquire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquiredImage {
    /// Index into [`Swapchain::images`].
    pub index: u32,
    /// The swapchain still works but no longer matches the surface; recreate soon.
    pub suboptimal: bool,
}

/// A swapchain. Drop it before its [`Surface`] and [`Device`].
pub struct Swapchain {
    loader: khr::swapchain::Device,
    raw: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    /// One "rendering finished" semaphore per image: presentation waits on it, and it can
    /// only be reused once that image is acquired again (the classic reuse hazard).
    render_finished: Vec<vk::Semaphore>,
    format: vk::SurfaceFormatKHR,
    extent: vk::Extent2D,
    present_mode: PresentMode,
    desc: SwapchainDesc,
    latency: crate::device::LatencyExtensions,
    present_wait: Option<ash::khr::present_wait::Device>,
    device: ash::Device,
}

impl core::fmt::Debug for Swapchain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Swapchain")
            .field("images", &self.images.len())
            .field("format", &self.format.format)
            .field("extent", &self.extent)
            .field("present_mode", &self.present_mode)
            .finish()
    }
}

impl Swapchain {
    /// Creates the swapchain.
    pub fn new(device: &Device, surface: &Surface, desc: &SwapchainDesc) -> Result<Self, RhiError> {
        let loader = khr::swapchain::Device::new(device.instance_raw(), device.raw());
        let mut swapchain = Self {
            loader,
            raw: vk::SwapchainKHR::null(),
            images: Vec::new(),
            views: Vec::new(),
            render_finished: Vec::new(),
            format: vk::SurfaceFormatKHR::default(),
            extent: vk::Extent2D::default(),
            present_mode: desc.present_mode,
            desc: *desc,
            latency: device.latency_extensions(),
            present_wait: device
                .latency_extensions()
                .present_wait
                .then(|| ash::khr::present_wait::Device::new(device.instance_raw(), device.raw())),
            device: device.raw().clone(),
        };
        swapchain.build(device, surface, desc.width, desc.height)?;
        Ok(swapchain)
    }

    /// Recreates the swapchain for a new size (after `Resized`, out-of-date or
    /// suboptimal). The caller must make sure no submitted work still uses the old images
    /// (for example `Frames::wait_idle`).
    pub fn recreate(
        &mut self,
        device: &Device,
        surface: &Surface,
        width: u32,
        height: u32,
    ) -> Result<(), RhiError> {
        self.build(device, surface, width, height)
    }

    fn build(
        &mut self,
        device: &Device,
        surface: &Surface,
        width: u32,
        height: u32,
    ) -> Result<(), RhiError> {
        engine_profiler::zone!("rhi::Swapchain::build");
        let physical = device.physical();
        // SAFETY: the physical device and surface belong to the same instance.
        let caps = unsafe {
            surface
                .loader
                .get_physical_device_surface_capabilities(physical, surface.raw)
        }
        .map_err(RhiError::vk("vkGetPhysicalDeviceSurfaceCapabilitiesKHR"))?;
        // SAFETY: as above.
        let formats = unsafe {
            surface
                .loader
                .get_physical_device_surface_formats(physical, surface.raw)
        }
        .map_err(RhiError::vk("vkGetPhysicalDeviceSurfaceFormatsKHR"))?;
        // SAFETY: as above.
        let modes = unsafe {
            surface
                .loader
                .get_physical_device_surface_present_modes(physical, surface.raw)
        }
        .map_err(RhiError::vk("vkGetPhysicalDeviceSurfacePresentModesKHR"))?;

        let preferred = if self.desc.srgb {
            [vk::Format::B8G8R8A8_SRGB, vk::Format::R8G8B8A8_SRGB]
        } else {
            [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM]
        };
        let format = preferred
            .iter()
            .find_map(|want| {
                formats.iter().find(|f| {
                    f.format == *want && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                })
            })
            .or_else(|| formats.first())
            .copied()
            .ok_or(RhiError::Vulkan {
                call: "surface has no formats",
                result: vk::Result::ERROR_FORMAT_NOT_SUPPORTED,
            })?;
        let present_mode = if modes.contains(&self.desc.present_mode.raw()) {
            self.desc.present_mode
        } else {
            PresentMode::Fifo
        };
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: width.clamp(
                    caps.min_image_extent.width,
                    caps.max_image_extent.width.max(1),
                ),
                height: height.clamp(
                    caps.min_image_extent.height,
                    caps.max_image_extent.height.max(1),
                ),
            }
        };
        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            image_count = image_count.min(caps.max_image_count);
        }

        let old = self.raw;
        let mut latency_info =
            vk::SwapchainLatencyCreateInfoNV::default().latency_mode_enable(true);
        let mut info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface.raw)
            .min_image_count(image_count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(present_mode.raw())
            .clipped(true)
            .old_swapchain(old);
        if self.desc.low_latency && self.latency.nv_low_latency2 {
            info = info.push_next(&mut latency_info);
        }
        // SAFETY: valid create info for a surface of this device's instance; `old` is
        // retired by this call and destroyed below after its views.
        let raw = unsafe { self.loader.create_swapchain(&info, None) }
            .map_err(RhiError::vk("vkCreateSwapchainKHR"))?;
        self.destroy_views();
        if old != vk::SwapchainKHR::null() {
            // SAFETY: the caller guaranteed no work uses the old images.
            unsafe { self.loader.destroy_swapchain(old, None) };
        }
        self.raw = raw;
        // SAFETY: `raw` is the swapchain just created.
        self.images = unsafe { self.loader.get_swapchain_images(raw) }
            .map_err(RhiError::vk("vkGetSwapchainImagesKHR"))?;
        for &image in &self.images {
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format.format)
                .subresource_range(color_range());
            // SAFETY: the image belongs to this device's swapchain.
            let view = unsafe { self.device.create_image_view(&view_info, None) }
                .map_err(RhiError::vk("vkCreateImageView"))?;
            self.views.push(view);
            // SAFETY: plain binary semaphore creation.
            let semaphore = unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .map_err(RhiError::vk("vkCreateSemaphore"))?;
            self.render_finished.push(semaphore);
        }
        self.format = format;
        self.extent = extent;
        self.present_mode = present_mode;
        Ok(())
    }

    fn destroy_views(&mut self) {
        // SAFETY: callers ensure the views and semaphores are no longer in use.
        unsafe {
            for view in self.views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            for semaphore in self.render_finished.drain(..) {
                self.device.destroy_semaphore(semaphore, None);
            }
        }
    }

    /// Acquires the next image; `signal` is signaled when it is ready to be rendered to.
    /// Returns `ERROR_OUT_OF_DATE_KHR` (as [`RhiError::Vulkan`]) when the swapchain must be
    /// recreated first.
    pub fn acquire(
        &mut self,
        signal: vk::Semaphore,
        timeout_ns: u64,
    ) -> Result<AcquiredImage, RhiError> {
        // SAFETY: `signal` is an unsignaled binary semaphore owned by the caller's frame.
        let (index, suboptimal) = unsafe {
            self.loader
                .acquire_next_image(self.raw, timeout_ns, signal, vk::Fence::null())
        }
        .map_err(RhiError::vk("vkAcquireNextImageKHR"))?;
        Ok(AcquiredImage { index, suboptimal })
    }

    /// Presents image `index` on `queue` after the work that signaled
    /// [`render_finished`](Self::render_finished)`(index)`. Returns whether the swapchain
    /// is suboptimal; out-of-date is an error.
    pub fn present(&self, queue: Queue, index: u32) -> Result<bool, RhiError> {
        self.present_with_id(queue, index, None)
    }

    /// Like [`Swapchain::present`], tagging the present with `id` (increasing, non-zero)
    /// when `VK_KHR_present_id` is enabled: [`Swapchain::wait_for_present`] and the
    /// latency extensions refer to presents by it.
    pub fn present_with_id(
        &self,
        queue: Queue,
        index: u32,
        id: Option<u64>,
    ) -> Result<bool, RhiError> {
        let semaphores = [self.render_finished[index as usize]];
        let swapchains = [self.raw];
        let indices = [index];
        let ids = [id.unwrap_or(0)];
        let mut present_id = vk::PresentIdKHR::default().present_ids(&ids);
        let mut info = vk::PresentInfoKHR::default()
            .wait_semaphores(&semaphores)
            .swapchains(&swapchains)
            .image_indices(&indices);
        if id.is_some() && self.latency.present_id {
            info = info.push_next(&mut present_id);
        }
        // SAFETY: `index` was acquired and rendered; the semaphore is signaled by the
        // caller's submission.
        unsafe { self.loader.queue_present(queue.raw, &info) }
            .map_err(RhiError::vk("vkQueuePresentKHR"))
    }

    /// Waits until the present tagged `id` has reached the screen, up to `timeout_ns`.
    /// `Ok(true)` when it has, `Ok(false)` on timeout or without `VK_KHR_present_wait`.
    pub fn wait_for_present(&self, id: u64, timeout_ns: u64) -> Result<bool, RhiError> {
        let Some(loader) = &self.present_wait else {
            return Ok(false);
        };
        // SAFETY: the swapchain is alive; ids come from `present_with_id`. Host access to
        // the swapchain is synchronized by `&self` on the presenting thread.
        match unsafe { loader.wait_for_present(self.raw, id, timeout_ns) } {
            Ok(()) => Ok(true),
            Err(vk::Result::TIMEOUT) => Ok(false),
            Err(e) => Err(RhiError::Vulkan {
                call: "vkWaitForPresentKHR",
                result: e,
            }),
        }
    }

    /// The raw swapchain handle (for extensions such as `VK_NV_low_latency2`).
    pub fn raw(&self) -> vk::SwapchainKHR {
        self.raw
    }

    /// Whether the swapchain was created for NVIDIA's low-latency mode.
    pub fn low_latency(&self) -> bool {
        self.desc.low_latency && self.latency.nv_low_latency2
    }

    /// Swapchain images.
    pub fn images(&self) -> &[vk::Image] {
        &self.images
    }

    /// Image views, one per image.
    pub fn views(&self) -> &[vk::ImageView] {
        &self.views
    }

    /// Semaphore that the submission rendering image `index` must signal.
    pub fn render_finished(&self, index: u32) -> vk::Semaphore {
        self.render_finished[index as usize]
    }

    /// Image format.
    pub fn format(&self) -> vk::Format {
        self.format.format
    }

    /// Image size.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Present mode actually in use.
    pub fn present_mode(&self) -> PresentMode {
        self.present_mode
    }

    /// Supported present modes on this surface, for the settings menu.
    pub fn supported_present_modes(
        device: &Device,
        surface: &Surface,
    ) -> Result<Vec<PresentMode>, RhiError> {
        // SAFETY: same instance.
        let modes = unsafe {
            surface
                .loader
                .get_physical_device_surface_present_modes(device.physical(), surface.raw)
        }
        .map_err(RhiError::vk("vkGetPhysicalDeviceSurfacePresentModesKHR"))?;
        Ok(modes.into_iter().map(PresentMode::from_raw).collect())
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // SAFETY: wait for all work before destroying images still referenced by it.
        unsafe {
            let _ = self.device.device_wait_idle();
        }
        self.destroy_views();
        // SAFETY: created by this loader; nothing uses it any more.
        unsafe { self.loader.destroy_swapchain(self.raw, None) };
    }
}

/// Subresource range of a single-mip, single-layer color image.
pub fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}
