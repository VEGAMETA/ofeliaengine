//! Surfaces, swapchains and presentation.

use crate::device::{Device, Queue};
use crate::error::RhiError;
use crate::instance::{Instance, InstanceShared};
use ash::{ext, khr, vk};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Vulkan functions looked up through a library that intercepts some of them: the surface
/// and swapchain functions of Streamline's interposer for DLSS frame generation (ADR
/// 0091), which takes over presentation. Every other call goes to the driver as usual.
#[derive(Clone, Copy)]
pub struct VulkanHooks {
    /// The library's `vkGetInstanceProcAddr`.
    pub get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
}

impl core::fmt::Debug for VulkanHooks {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VulkanHooks").finish_non_exhaustive()
    }
}

impl VulkanHooks {
    fn static_fn(&self) -> ash::StaticFn {
        ash::StaticFn {
            get_instance_proc_addr: self.get_instance_proc_addr,
        }
    }

    /// An entry whose lookups go through the hook.
    fn entry(&self) -> ash::Entry {
        // SAFETY: the hook behaves as the loader's `vkGetInstanceProcAddr` (it forwards
        // what it does not intercept).
        unsafe { ash::Entry::from_static_fn(self.static_fn()) }
    }

    /// `instance`'s functions, looked up through the hook (device-level ones too: its
    /// `vkGetDeviceProcAddr` is the hook's).
    fn instance(&self, instance: vk::Instance) -> ash::Instance {
        // SAFETY: as above; `instance` is alive.
        unsafe { ash::Instance::load(&self.static_fn(), instance) }
    }

    /// `vkDeviceWaitIdle` through the hook: the library drains its own queues too.
    pub fn device_wait_idle(&self, device: &Device) -> Result<(), RhiError> {
        let instance = self.instance(device.instance_raw().handle());
        // SAFETY: looking up a core function of a live device.
        let f = unsafe {
            instance.get_device_proc_addr(device.raw().handle(), c"vkDeviceWaitIdle".as_ptr())
        };
        let Some(f) = f else {
            return device.wait_idle();
        };
        // SAFETY: `vkDeviceWaitIdle` has this signature; the device is alive.
        let wait = unsafe {
            core::mem::transmute::<unsafe extern "system" fn(), vk::PFN_vkDeviceWaitIdle>(f)
        };
        // SAFETY: as above.
        match unsafe { wait(device.raw().handle()) } {
            vk::Result::SUCCESS => Ok(()),
            result => Err(RhiError::Vulkan {
                call: "vkDeviceWaitIdle",
                result,
            }),
        }
    }
}

/// A presentation surface: a window, or headless for tests.
pub struct Surface {
    pub(crate) raw: vk::SurfaceKHR,
    pub(crate) loader: khr::surface::Instance,
    /// The window's `HWND` (0 for a headless surface).
    hwnd: isize,
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
            hwnd,
            _instance: instance.shared.clone(),
        })
    }

    /// [`Self::from_win32`] through `hooks` (Streamline's, for DLSS frame generation: it
    /// watches the surface it then presents to).
    #[cfg(windows)]
    pub fn from_win32_hooked(
        instance: &Instance,
        hinstance: isize,
        hwnd: isize,
        hooks: &VulkanHooks,
    ) -> Result<Self, RhiError> {
        if !instance.shared.win32_surface_enabled {
            return Err(RhiError::Vulkan {
                call: "VK_KHR_win32_surface not enabled",
                result: vk::Result::ERROR_EXTENSION_NOT_PRESENT,
            });
        }
        let entry = hooks.entry();
        let hooked = hooks.instance(instance.raw().handle());
        let loader = khr::win32_surface::Instance::new(&entry, &hooked);
        let info = vk::Win32SurfaceCreateInfoKHR::default()
            .hinstance(hinstance)
            .hwnd(hwnd);
        // SAFETY: as in `from_win32`.
        let raw = unsafe { loader.create_win32_surface(&info, None) }
            .map_err(RhiError::vk("vkCreateWin32SurfaceKHR"))?;
        Ok(Self {
            raw,
            loader: khr::surface::Instance::new(&entry, &hooked),
            hwnd,
            _instance: instance.shared.clone(),
        })
    }

    /// The LUID of the display adapter that drives the monitor showing (most of) this
    /// surface's window, low part first as [`crate::Adapter::luid`]; `None` for a headless
    /// surface or when Windows cannot tell.
    pub fn display_adapter_luid(&self) -> Option<[u8; 8]> {
        #[cfg(windows)]
        {
            use windows_sys::Wdk::Graphics::Direct3D::{
                D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMGDIDISPLAYNAME, D3DKMTCloseAdapter,
                D3DKMTOpenAdapterFromGdiDisplayName,
            };
            use windows_sys::Win32::Graphics::Gdi::{
                GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
            };
            if self.hwnd == 0 {
                return None;
            }
            // SAFETY: plain Win32 calls about the window this surface was created for (a
            // stale handle only makes them fail); the structures are sized and zeroed as
            // the calls require, and the adapter opened is closed again.
            unsafe {
                let monitor = MonitorFromWindow(self.hwnd as _, MONITOR_DEFAULTTONEAREST);
                if monitor.is_null() {
                    return None;
                }
                let mut info = MONITORINFOEXW::default();
                info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
                if GetMonitorInfoW(monitor, (&raw mut info).cast()) == 0 {
                    return None;
                }
                let mut open = D3DKMT_OPENADAPTERFROMGDIDISPLAYNAME {
                    DeviceName: info.szDevice,
                    ..Default::default()
                };
                if D3DKMTOpenAdapterFromGdiDisplayName(&mut open) != 0 {
                    return None;
                }
                D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER {
                    hAdapter: open.hAdapter,
                });
                let mut luid = [0u8; 8];
                luid[..4].copy_from_slice(&open.AdapterLuid.LowPart.to_le_bytes());
                luid[4..].copy_from_slice(&open.AdapterLuid.HighPart.to_le_bytes());
                Some(luid)
            }
        }
        #[cfg(not(windows))]
        None
    }

    /// Whether `adapter` presents to this surface across adapters: its window's monitor is
    /// driven by another GPU (a desktop's integrated GPU behind a monitor on the discrete
    /// one, a laptop's discrete GPU behind its integrated one). Swapchain images are then
    /// shared with that GPU, and drawing into them is slow ([`Swapchain::presents_across_adapters`]).
    /// `false` when either side cannot tell.
    pub fn presents_across_adapters(&self, adapter: &crate::Adapter) -> bool {
        match (self.display_adapter_luid(), adapter.luid) {
            (Some(display), Some(own)) => display != own,
            _ => false,
        }
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
            hwnd: 0,
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

/// What a swapchain's images hold and how the display reads them (ADR 0038).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputColor {
    /// SDR: 8-bit sRGB (`B8G8R8A8_SRGB`, or `_UNORM` without
    /// [`SwapchainDesc::srgb`]). Shaders write linear values in 0..1.
    Sdr,
    /// HDR as scRGB: linear FP16 with BT.709 primaries, 1.0 = 80 nits; values above SDR
    /// white are brighter, negative ones reach colors outside BT.709
    /// (`R16G16B16A16_SFLOAT`, `EXTENDED_SRGB_LINEAR`). Windows composes in it, and
    /// blending stays linear.
    ScRgb,
    /// HDR10: PQ-encoded BT.2020 in 10 bits (`A2B10G10R10_UNORM_PACK32`,
    /// `HDR10_ST2084`). Half the bytes of scRGB, but shaders encode it themselves and
    /// blending in it is not linear.
    Hdr10,
}

impl OutputColor {
    /// Whether this is an HDR output.
    pub fn is_hdr(self) -> bool {
        self != Self::Sdr
    }

    /// Formats (best first) and the color space of this output.
    fn candidates(self, srgb: bool) -> (&'static [vk::Format], vk::ColorSpaceKHR) {
        match self {
            Self::Sdr if srgb => (
                &[vk::Format::B8G8R8A8_SRGB, vk::Format::R8G8B8A8_SRGB],
                vk::ColorSpaceKHR::SRGB_NONLINEAR,
            ),
            Self::Sdr => (
                &[vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM],
                vk::ColorSpaceKHR::SRGB_NONLINEAR,
            ),
            Self::ScRgb => (
                &[vk::Format::R16G16B16A16_SFLOAT],
                vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT,
            ),
            Self::Hdr10 => (
                &[
                    vk::Format::A2B10G10R10_UNORM_PACK32,
                    vk::Format::A2R10G10B10_UNORM_PACK32,
                ],
                vk::ColorSpaceKHR::HDR10_ST2084_EXT,
            ),
        }
    }

    /// What to try when `self` is asked for: itself, the other HDR encoding, then SDR.
    fn fallbacks(self) -> &'static [Self] {
        match self {
            Self::Sdr => &[Self::Sdr],
            Self::ScRgb => &[Self::ScRgb, Self::Hdr10, Self::Sdr],
            Self::Hdr10 => &[Self::Hdr10, Self::ScRgb, Self::Sdr],
        }
    }

    /// The first of `formats` this output can use.
    fn pick(self, srgb: bool, formats: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
        let (wanted, space) = self.candidates(srgb);
        wanted.iter().find_map(|want| {
            formats
                .iter()
                .find(|f| f.format == *want && f.color_space == space)
                .copied()
        })
    }
}

/// What an HDR swapchain's content spans (`VK_EXT_hdr_metadata`: the SMPTE ST 2086
/// mastering display and CTA-861.3 content light levels). A display that tone maps by
/// itself decides from it how much to compress; content already tone mapped for the
/// display states the display's own range, so that it is left alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrMetadata {
    /// Peak luminance of the mastering display, nits.
    pub max_nits: f32,
    /// Black level of the mastering display, nits.
    pub min_nits: f32,
    /// Brightest pixel of the content (MaxCLL), nits.
    pub max_content_nits: f32,
    /// Brightest frame average of the content (MaxFALL), nits.
    pub max_frame_average_nits: f32,
}

/// Whether a swapchain may take its monitor for itself (`VK_EXT_full_screen_exclusive`,
/// ADR 0099). Without the extension ([`Device::full_screen_exclusive_enabled`]) every
/// choice is [`Self::Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullScreenExclusive {
    /// Nothing stated: the driver decides.
    #[default]
    Default,
    /// The driver may take the monitor for the swapchain when it sees fit (a window
    /// covering its monitor), with the switch that brings (the screen may go black for a
    /// moment).
    Allowed,
    /// The driver must not: the swapchain stays a window's, composed by Windows or
    /// flipped to the display when nothing covers it.
    Disallowed,
    /// The swapchain takes monitor `monitor` (its window covers it:
    /// `engine_platform::window::WindowMode::FullScreen`) at every creation. When the
    /// driver refuses, the swapchain is made as with [`Self::Disallowed`] until the next
    /// recreation; when the monitor is lost later (another window on top, Alt+Tab),
    /// acquiring and presenting fail with `ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT`,
    /// which [`RhiError::is_out_of_date`] counts: recreate it. [`Swapchain::exclusive`]
    /// tells whether it holds the monitor.
    Exclusive {
        /// The monitor (an `HMONITOR`).
        monitor: isize,
    },
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
    /// Prefer an sRGB format (the GPU encodes on write) for SDR output.
    pub srgb: bool,
    /// Requested output; falls back to the other HDR encoding, then to SDR, when the
    /// surface does not offer it ([`Swapchain::color`] tells what was used). Ask for HDR
    /// only while the display runs in HDR: Windows clips an HDR swapchain on an SDR
    /// display to SDR white.
    pub color: OutputColor,
    /// Create the swapchain for NVIDIA's low-latency mode (`VK_NV_low_latency2`, Reflex)
    /// when the device has it; `engine_latency` drives it.
    pub low_latency: bool,
    /// Whether the swapchain may take its monitor for itself.
    pub full_screen_exclusive: FullScreenExclusive,
}

impl Default for SwapchainDesc {
    /// The surface's size, vsync, sRGB, SDR, no low-latency mode, the driver's default
    /// full screen behavior.
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            present_mode: PresentMode::Fifo,
            srgb: true,
            color: OutputColor::Sdr,
            low_latency: false,
            full_screen_exclusive: FullScreenExclusive::Default,
        }
    }
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
    color: OutputColor,
    extent: vk::Extent2D,
    present_mode: PresentMode,
    desc: SwapchainDesc,
    /// Whether the device presents across adapters ([`Surface::presents_across_adapters`]),
    /// as of the last (re)creation.
    across_adapters: bool,
    /// `VK_EXT_hdr_metadata`, and the metadata last set (stated again for every new
    /// swapchain object).
    hdr_metadata: Option<(ash::ext::hdr_metadata::Device, Option<HdrMetadata>)>,
    latency: crate::device::LatencyExtensions,
    present_wait: Option<ash::khr::present_wait::Device>,
    /// Largest present id sent so far: ids must keep increasing for the swapchain's whole
    /// life, across every user of it (`VUID-VkPresentIdKHR-presentIds-04999`).
    last_present_id: AtomicU64,
    /// Presents through this function instead of the driver's (a library's hook, such as
    /// Streamline's for DLSS, ADR 0063).
    present_function: Option<vk::PFN_vkQueuePresentKHR>,
    /// `VK_EXT_full_screen_exclusive`, when the device enabled it.
    full_screen_exclusive: Option<ash::ext::full_screen_exclusive::Device>,
    /// The swapchain holds its monitor ([`FullScreenExclusive::Exclusive`]).
    exclusive: bool,
    /// Why the driver refused the monitor at the last (re)creation.
    exclusive_refused: Option<vk::Result>,
    device: ash::Device,
}

impl core::fmt::Debug for Swapchain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Swapchain")
            .field("images", &self.images.len())
            .field("format", &self.format.format)
            .field("color", &self.color)
            .field("extent", &self.extent)
            .field("present_mode", &self.present_mode)
            .finish()
    }
}

impl Swapchain {
    /// Creates the swapchain.
    pub fn new(device: &Device, surface: &Surface, desc: &SwapchainDesc) -> Result<Self, RhiError> {
        Self::create(device, surface, desc, None)
    }

    /// [`Self::new`] with the swapchain functions looked up through `hooks` (Streamline's,
    /// for DLSS frame generation: it presents the generated frames; ADR 0091). Create the
    /// surface through the same hooks.
    pub fn new_hooked(
        device: &Device,
        surface: &Surface,
        desc: &SwapchainDesc,
        hooks: &VulkanHooks,
    ) -> Result<Self, RhiError> {
        Self::create(device, surface, desc, Some(hooks))
    }

    fn create(
        device: &Device,
        surface: &Surface,
        desc: &SwapchainDesc,
        hooks: Option<&VulkanHooks>,
    ) -> Result<Self, RhiError> {
        let loader = match hooks {
            Some(hooks) => khr::swapchain::Device::new(
                &hooks.instance(device.instance_raw().handle()),
                device.raw(),
            ),
            None => khr::swapchain::Device::new(device.instance_raw(), device.raw()),
        };
        let mut swapchain = Self {
            loader,
            raw: vk::SwapchainKHR::null(),
            images: Vec::new(),
            views: Vec::new(),
            render_finished: Vec::new(),
            format: vk::SurfaceFormatKHR::default(),
            color: OutputColor::Sdr,
            extent: vk::Extent2D::default(),
            present_mode: desc.present_mode,
            desc: *desc,
            across_adapters: false,
            hdr_metadata: device.hdr_metadata_enabled().then(|| {
                (
                    ash::ext::hdr_metadata::Device::new(device.instance_raw(), device.raw()),
                    None,
                )
            }),
            latency: device.latency_extensions(),
            present_wait: device
                .latency_extensions()
                .present_wait
                .then(|| ash::khr::present_wait::Device::new(device.instance_raw(), device.raw())),
            last_present_id: AtomicU64::new(0),
            present_function: None,
            full_screen_exclusive: device.full_screen_exclusive_enabled().then(|| {
                ash::ext::full_screen_exclusive::Device::new(device.instance_raw(), device.raw())
            }),
            exclusive: false,
            exclusive_refused: None,
            device: device.raw().clone(),
        };
        swapchain.build(device, surface, desc.width, desc.height)?;
        Ok(swapchain)
    }

    /// Recreates the swapchain for a new size (after `Resized`, out-of-date or
    /// suboptimal). Waits for the device to go idle: the old images may still be in
    /// submitted work or being presented.
    pub fn recreate(
        &mut self,
        device: &Device,
        surface: &Surface,
        width: u32,
        height: u32,
    ) -> Result<(), RhiError> {
        self.build(device, surface, width, height)
    }

    /// Recreates the swapchain for another output (HDR turned on or off); the format may
    /// change with it ([`Swapchain::format`]). Same conditions as
    /// [`Swapchain::recreate`].
    pub fn recreate_with_color(
        &mut self,
        device: &Device,
        surface: &Surface,
        width: u32,
        height: u32,
        color: OutputColor,
    ) -> Result<(), RhiError> {
        self.desc.color = color;
        self.build(device, surface, width, height)
    }

    fn build(
        &mut self,
        device: &Device,
        surface: &Surface,
        width: u32,
        height: u32,
    ) -> Result<(), RhiError> {
        let choice = match self.full_screen_exclusive {
            Some(_) => self.desc.full_screen_exclusive,
            None => FullScreenExclusive::Default,
        };
        self.exclusive_refused = None;
        self.build_as(device, surface, width, height, choice)?;
        if matches!(choice, FullScreenExclusive::Exclusive { .. }) && !self.exclusive {
            // Refused (the window is not on top, say): a borderless window's swapchain
            // until the next recreation tries again. Made from nothing, not as the refused
            // one's successor: NVIDIA's driver keeps the way a replaced swapchain presented
            // (measured: copied through GDI instead of flipped).
            // SAFETY: the swapchain was just made; no image of it was acquired.
            unsafe {
                self.destroy_views();
                self.loader.destroy_swapchain(self.raw, None);
            }
            self.raw = vk::SwapchainKHR::null();
            self.build_as(
                device,
                surface,
                width,
                height,
                FullScreenExclusive::Disallowed,
            )?;
        }
        Ok(())
    }

    fn build_as(
        &mut self,
        device: &Device,
        surface: &Surface,
        width: u32,
        height: u32,
        choice: FullScreenExclusive,
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

        let (color, format) =
            choose_format(self.desc.color, self.desc.srgb, &formats).ok_or(RhiError::Vulkan {
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
        let mut exclusive_info = vk::SurfaceFullScreenExclusiveInfoEXT::default();
        let mut exclusive_monitor = vk::SurfaceFullScreenExclusiveWin32InfoEXT::default();
        match choice {
            FullScreenExclusive::Default => {}
            FullScreenExclusive::Allowed => {
                exclusive_info.full_screen_exclusive = vk::FullScreenExclusiveEXT::ALLOWED;
                info = info.push_next(&mut exclusive_info);
            }
            FullScreenExclusive::Disallowed => {
                exclusive_info.full_screen_exclusive = vk::FullScreenExclusiveEXT::DISALLOWED;
                info = info.push_next(&mut exclusive_info);
            }
            FullScreenExclusive::Exclusive { monitor } => {
                exclusive_info.full_screen_exclusive =
                    vk::FullScreenExclusiveEXT::APPLICATION_CONTROLLED;
                exclusive_monitor.hmonitor = monitor;
                info = info
                    .push_next(&mut exclusive_info)
                    .push_next(&mut exclusive_monitor);
            }
        }
        if self.exclusive
            && let Some(loader) = &self.full_screen_exclusive
        {
            // The monitor goes back before the swapchain holding it is retired.
            // SAFETY: `old` holds the monitor and is not retired yet. (An error means it
            // had already lost it.)
            let _ = unsafe { loader.release_full_screen_exclusive_mode(old) };
            self.exclusive = false;
        }
        // SAFETY: valid create info for a surface of this device's instance; `old` is
        // retired by this call and destroyed below after its views.
        let raw = unsafe { self.loader.create_swapchain(&info, None) }
            .map_err(RhiError::vk("vkCreateSwapchainKHR"))?;
        if old != vk::SwapchainKHR::null() {
            // The last presents of the old images may still wait on their semaphores:
            // finished frames do not prove finished presents, and without the present
            // fences of VK_EXT_swapchain_maintenance1 only an idle device does.
            // Recreation is rare. (An error here is device loss, which the next call
            // reports.)
            // SAFETY: plain wait.
            let _ = unsafe { self.device.device_wait_idle() };
        }
        self.destroy_views();
        if old != vk::SwapchainKHR::null() {
            // SAFETY: the device is idle; nothing uses the old images.
            unsafe { self.loader.destroy_swapchain(old, None) };
        }
        self.raw = raw;
        if let (FullScreenExclusive::Exclusive { .. }, Some(loader)) =
            (choice, &self.full_screen_exclusive)
        {
            // SAFETY: the swapchain was just made application controlled; it does not hold
            // the monitor yet.
            let acquired = unsafe { loader.acquire_full_screen_exclusive_mode(raw) };
            self.exclusive = acquired.is_ok();
            self.exclusive_refused = acquired.err();
        }
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
        self.color = color;
        self.extent = extent;
        self.present_mode = present_mode;
        self.across_adapters = surface.presents_across_adapters(device.adapter());
        if let Some((_, Some(metadata))) = self.hdr_metadata {
            self.set_hdr_metadata(&metadata);
        }
        Ok(())
    }

    /// States the luminance range of the content (`VK_EXT_hdr_metadata`); kept across
    /// recreation. Ignored for SDR output and without the extension
    /// ([`Device::hdr_metadata_enabled`]).
    pub fn set_hdr_metadata(&mut self, metadata: &HdrMetadata) {
        let Some((loader, stored)) = &mut self.hdr_metadata else {
            return;
        };
        *stored = Some(*metadata);
        let primaries = match self.color {
            OutputColor::Sdr => return,
            OutputColor::ScRgb => BT709_PRIMARIES,
            OutputColor::Hdr10 => BT2020_PRIMARIES,
        };
        let xy = |[x, y]: [f32; 2]| vk::XYColorEXT { x, y };
        let info = vk::HdrMetadataEXT::default()
            .display_primary_red(xy(primaries[0]))
            .display_primary_green(xy(primaries[1]))
            .display_primary_blue(xy(primaries[2]))
            .white_point(xy(D65_WHITE))
            .max_luminance(metadata.max_nits)
            .min_luminance(metadata.min_nits)
            .max_content_light_level(metadata.max_content_nits)
            .max_frame_average_light_level(metadata.max_frame_average_nits);
        // SAFETY: the swapchain belongs to the device the loader was made for; host access
        // to it is synchronized by `&mut self`.
        unsafe { loader.set_hdr_metadata(&[self.raw], &[info]) };
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
        if let Some(id) = id
            && self.latency.present_id
        {
            info = info.push_next(&mut present_id);
            self.last_present_id.fetch_max(id, Ordering::Relaxed);
        }
        if let Some(present) = self.present_function {
            // SAFETY: as below; the hook forwards to the driver's `vkQueuePresentKHR`
            // (`set_present_function`).
            return match unsafe { present(queue.raw, &info) } {
                vk::Result::SUCCESS => Ok(false),
                vk::Result::SUBOPTIMAL_KHR => Ok(true),
                result => Err(RhiError::Vulkan {
                    call: "vkQueuePresentKHR",
                    result,
                }),
            };
        }
        // SAFETY: `index` was acquired and rendered; the semaphore is signaled by the
        // caller's submission.
        unsafe { self.loader.queue_present(queue.raw, &info) }
            .map_err(RhiError::vk("vkQueuePresentKHR"))
    }

    /// Presents through `present` instead of the driver's `vkQueuePresentKHR` (or again
    /// through the driver's with `None`): a library that must see every present, such as
    /// Streamline for DLSS (ADR 0063), hooks it this way.
    ///
    /// # Safety
    ///
    /// `present` behaves as `vkQueuePresentKHR` of this swapchain's device (it forwards to
    /// it) and stays callable for as long as it is set.
    pub unsafe fn set_present_function(&mut self, present: Option<vk::PFN_vkQueuePresentKHR>) {
        self.present_function = present;
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

    /// The largest id passed to [`Swapchain::present_with_id`] so far (0 before the first).
    /// The next id must be larger.
    pub fn last_present_id(&self) -> u64 {
        self.last_present_id.load(Ordering::Relaxed)
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

    /// What the images hold: SDR, scRGB or HDR10.
    pub fn color(&self) -> OutputColor {
        self.color
    }

    /// Image size.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Present mode actually in use.
    pub fn present_mode(&self) -> PresentMode {
        self.present_mode
    }

    /// Whether the swapchain holds its monitor for itself
    /// ([`FullScreenExclusive::Exclusive`], as of the last (re)creation).
    pub fn exclusive(&self) -> bool {
        self.exclusive
    }

    /// What the driver answered when it refused the monitor at the last (re)creation
    /// ([`FullScreenExclusive::Exclusive`]); `None` when it did not refuse.
    pub fn exclusive_refused(&self) -> Option<vk::Result> {
        self.exclusive_refused
    }

    /// Whether the device presents across adapters (the window's monitor is driven by
    /// another GPU), as of the last (re)creation: draw the frame into an image of its own
    /// and copy it into the swapchain image then (`engine_render::present::PresentTarget`
    /// does). On ref-1's Radeon iGPU behind a monitor on the RTX, the tone mapper took
    /// 2.56 ms into the swapchain image, 0.82 ms into an image of its own plus 0.62 ms for
    /// the copy; where the monitor is the GPU's own, a copy only costs (19.5 µs on the
    /// RTX).
    pub fn presents_across_adapters(&self) -> bool {
        self.across_adapters
    }

    /// Outputs this surface offers, for the settings menu. HDR ones need
    /// `VK_EXT_swapchain_colorspace` ([`Instance::hdr_color_spaces_enabled`]); a surface
    /// may offer them while the display runs in SDR.
    pub fn supported_colors(
        device: &Device,
        surface: &Surface,
    ) -> Result<Vec<OutputColor>, RhiError> {
        let formats = Self::surface_formats(device, surface)?;
        Ok([OutputColor::Sdr, OutputColor::ScRgb, OutputColor::Hdr10]
            .into_iter()
            .filter(|c| c.pick(true, &formats).is_some() || c.pick(false, &formats).is_some())
            .collect())
    }

    /// The image format [`Swapchain::new`] picks for `desc` on `surface`, without making
    /// the swapchain: what draws into its images can be made while it is (on the Radeon
    /// iGPU, presenting across adapters, making one takes 170 ms; ADR 0079).
    pub fn format_for(
        device: &Device,
        surface: &Surface,
        desc: &SwapchainDesc,
    ) -> Result<vk::Format, RhiError> {
        let formats = Self::surface_formats(device, surface)?;
        choose_format(desc.color, desc.srgb, &formats)
            .map(|(_, format)| format.format)
            .ok_or(RhiError::Vulkan {
                call: "surface has no formats",
                result: vk::Result::ERROR_FORMAT_NOT_SUPPORTED,
            })
    }

    /// Every format and color space the surface offers, for diagnostics.
    pub fn surface_formats(
        device: &Device,
        surface: &Surface,
    ) -> Result<Vec<vk::SurfaceFormatKHR>, RhiError> {
        // SAFETY: the physical device and surface belong to the same instance.
        unsafe {
            surface
                .loader
                .get_physical_device_surface_formats(device.physical(), surface.raw)
        }
        .map_err(RhiError::vk("vkGetPhysicalDeviceSurfaceFormatsKHR"))
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

/// The output and surface format for a request of `color`: its format when offered, else
/// the other HDR encoding, else SDR, else whatever the surface lists first.
fn choose_format(
    color: OutputColor,
    srgb: bool,
    formats: &[vk::SurfaceFormatKHR],
) -> Option<(OutputColor, vk::SurfaceFormatKHR)> {
    color
        .fallbacks()
        .iter()
        .find_map(|&color| Some((color, color.pick(srgb, formats)?)))
        .or_else(|| formats.first().map(|&f| (OutputColor::Sdr, f)))
}

/// CIE xy of the BT.709 / sRGB primaries (red, green, blue).
const BT709_PRIMARIES: [[f32; 2]; 3] = [[0.640, 0.330], [0.300, 0.600], [0.150, 0.060]];
/// CIE xy of the BT.2020 primaries.
const BT2020_PRIMARIES: [[f32; 2]; 3] = [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046]];
/// CIE xy of the D65 white point.
const D65_WHITE: [f32; 2] = [0.3127, 0.3290];

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

#[cfg(test)]
mod tests {
    use super::*;

    fn formats(list: &[(vk::Format, vk::ColorSpaceKHR)]) -> Vec<vk::SurfaceFormatKHR> {
        list.iter()
            .map(|&(format, color_space)| vk::SurfaceFormatKHR {
                format,
                color_space,
            })
            .collect()
    }

    #[test]
    fn output_falls_back_to_what_the_surface_offers() {
        use vk::ColorSpaceKHR as C;
        use vk::Format as F;
        // What NVIDIA lists on ref-1 (whether Windows runs the display in HDR or not).
        let nvidia = formats(&[
            (F::B8G8R8A8_UNORM, C::SRGB_NONLINEAR),
            (F::B8G8R8A8_SRGB, C::SRGB_NONLINEAR),
            (F::R16G16B16A16_SFLOAT, C::EXTENDED_SRGB_LINEAR_EXT),
            (F::A2B10G10R10_UNORM_PACK32, C::HDR10_ST2084_EXT),
            (F::A2B10G10R10_UNORM_PACK32, C::SRGB_NONLINEAR),
        ]);
        // What the Radeon lists for a display attached to the other GPU.
        let sdr_only = formats(&[
            (F::R8G8B8A8_UNORM, C::SRGB_NONLINEAR),
            (F::B8G8R8A8_SRGB, C::SRGB_NONLINEAR),
        ]);
        let hdr10_only = formats(&[
            (F::B8G8R8A8_SRGB, C::SRGB_NONLINEAR),
            (F::A2R10G10B10_UNORM_PACK32, C::HDR10_ST2084_EXT),
        ]);
        let pick = |color, list: &[vk::SurfaceFormatKHR]| {
            choose_format(color, true, list).map(|(c, f)| (c, f.format))
        };
        use OutputColor::{Hdr10, ScRgb, Sdr};
        assert_eq!(pick(Sdr, &nvidia), Some((Sdr, F::B8G8R8A8_SRGB)));
        assert_eq!(pick(ScRgb, &nvidia), Some((ScRgb, F::R16G16B16A16_SFLOAT)));
        assert_eq!(
            pick(Hdr10, &nvidia),
            Some((Hdr10, F::A2B10G10R10_UNORM_PACK32))
        );
        assert_eq!(pick(ScRgb, &sdr_only), Some((Sdr, F::B8G8R8A8_SRGB)));
        assert_eq!(
            pick(ScRgb, &hdr10_only),
            Some((Hdr10, F::A2R10G10B10_UNORM_PACK32))
        );
        assert_eq!(
            choose_format(Sdr, false, &nvidia).map(|(_, f)| f.format),
            Some(F::B8G8R8A8_UNORM)
        );
        // A surface without any of ours: its first format, treated as SDR.
        let odd = formats(&[(F::R5G6B5_UNORM_PACK16, C::SRGB_NONLINEAR)]);
        assert_eq!(pick(ScRgb, &odd), Some((Sdr, F::R5G6B5_UNORM_PACK16)));
        assert_eq!(pick(ScRgb, &[]), None);
    }
}
