//! Logical device and queues.

use crate::adapter::Adapter;
use crate::error::RhiError;
use crate::instance::{Instance, InstanceShared};
use ash::vk;
use std::sync::Arc;

/// A device queue and its family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Queue {
    /// Vulkan queue handle.
    pub raw: vk::Queue,
    /// Queue family index.
    pub family: u32,
}

/// Optional extensions for latency control and frame pacing (CLAUDE.md §7), enabled
/// when the adapter supports them and the device presents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatencyExtensions {
    /// `VK_KHR_present_id`: presents carry ids (needed by the others).
    pub present_id: bool,
    /// `VK_KHR_present_wait`: wait until a present id is on screen.
    pub present_wait: bool,
    /// `VK_NV_low_latency2`: NVIDIA Reflex through the driver, no DLL.
    pub nv_low_latency2: bool,
    /// `VK_AMD_anti_lag`: AMD Anti-Lag 2 through the driver, no DLL.
    pub amd_anti_lag: bool,
}

/// `VkPhysicalDeviceAntiLagFeaturesAMD` (Vulkan 1.3.290; newer than the bindings).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AntiLagFeaturesAmd {
    s_type: vk::StructureType,
    p_next: *mut core::ffi::c_void,
    anti_lag: vk::Bool32,
}

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ANTI_LAG_FEATURES_AMD`.
const ANTI_LAG_FEATURES_AMD: vk::StructureType = vk::StructureType::from_raw(1_000_476_000);

impl Default for AntiLagFeaturesAmd {
    fn default() -> Self {
        Self {
            s_type: ANTI_LAG_FEATURES_AMD,
            p_next: core::ptr::null_mut(),
            anti_lag: vk::FALSE,
        }
    }
}

// SAFETY: a `repr(C)` structure that starts with `sType` and `pNext`, as the feature
// chains require.
unsafe impl vk::ExtendsPhysicalDeviceFeatures2 for AntiLagFeaturesAmd {}
// SAFETY: as above.
unsafe impl vk::ExtendsDeviceCreateInfo for AntiLagFeaturesAmd {}

/// The logical device with the engine's required features enabled.
pub struct Device {
    raw: ash::Device,
    adapter: Adapter,
    latency: LatencyExtensions,
    /// `VK_EXT_hdr_metadata` is enabled.
    hdr_metadata: bool,
    /// Pipelines capture the driver's statistics (`ENGINE_PIPELINE_STATS=1`).
    pipeline_statistics: bool,
    /// Graphics + compute + transfer (+ present on a desktop GPU).
    pub graphics: Queue,
    /// A compute queue on a family without graphics (async compute), if the GPU has one.
    pub async_compute: Option<Queue>,
    /// A transfer-only queue (copy engine), if the GPU has one.
    pub transfer: Option<Queue>,
    // Keeps the instance alive until the device is destroyed.
    instance: Arc<InstanceShared>,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("adapter", &self.adapter.name())
            .field("graphics", &self.graphics)
            .field("async_compute", &self.async_compute)
            .field("transfer", &self.transfer)
            .finish()
    }
}

impl Device {
    /// Creates the device on `adapter`, which must be suitable
    /// ([`Adapter::missing_features`] empty).
    pub fn new(instance: &Instance, adapter: &Adapter) -> Result<Self, RhiError> {
        engine_profiler::zone!("rhi::Device::new");
        if !adapter.missing_features().is_empty() {
            return Err(RhiError::NoSuitableAdapter(vec![(
                adapter.name().to_owned(),
                adapter.missing_features().to_vec(),
            )]));
        }
        let families = &adapter.queue_families;
        let find = |want: vk::QueueFlags, avoid: vk::QueueFlags| {
            families
                .iter()
                .position(|f| {
                    f.queue_count > 0
                        && f.queue_flags.contains(want)
                        && !f.queue_flags.intersects(avoid)
                })
                .map(|i| i as u32)
        };
        let graphics_family = find(
            vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE,
            vk::QueueFlags::empty(),
        )
        .ok_or(RhiError::NoSuitableAdapter(vec![(
            adapter.name().to_owned(),
            vec!["graphics+compute queue"],
        )]))?;
        let compute_family = find(vk::QueueFlags::COMPUTE, vk::QueueFlags::GRAPHICS);
        let transfer_family = find(
            vk::QueueFlags::TRANSFER,
            vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE,
        );

        let priority = [1.0f32];
        let mut queue_infos = vec![
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(graphics_family)
                .queue_priorities(&priority),
        ];
        for family in [compute_family, transfer_family].into_iter().flatten() {
            queue_infos.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priority),
            );
        }

        // Exactly the features checked in `Adapter::query`, plus a few optional ones.
        let mut f11 = vk::PhysicalDeviceVulkan11Features::default().shader_draw_parameters(true);
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(true)
            .buffer_device_address(true)
            .descriptor_indexing(true)
            .runtime_descriptor_array(true)
            .descriptor_binding_partially_bound(true)
            .descriptor_binding_variable_descriptor_count(true)
            .descriptor_binding_sampled_image_update_after_bind(true)
            .descriptor_binding_storage_image_update_after_bind(true)
            .descriptor_binding_storage_buffer_update_after_bind(true)
            .descriptor_binding_update_unused_while_pending(true)
            .shader_sampled_image_array_non_uniform_indexing(true)
            .shader_storage_image_array_non_uniform_indexing(true)
            .shader_storage_buffer_array_non_uniform_indexing(true)
            .scalar_block_layout(true);
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true)
            .maintenance4(true)
            .shader_demote_to_helper_invocation(true);
        let features = vk::PhysicalDeviceFeatures::default()
            .sampler_anisotropy(true)
            .texture_compression_bc(true)
            .multi_draw_indirect(true)
            .draw_indirect_first_instance(true);
        let mut features2 = vk::PhysicalDeviceFeatures2::default()
            .features(features)
            .push_next(&mut f11)
            .push_next(&mut f12)
            .push_next(&mut f13);

        let mut extensions: Vec<*const i8> = Vec::new();
        let presenting = adapter.caps.swapchain && instance.surface_enabled();
        if presenting {
            extensions.push(ash::khr::swapchain::NAME.as_ptr());
        }
        if adapter.caps.memory_budget {
            extensions.push(ash::ext::memory_budget::NAME.as_ptr());
        }
        // Mastering metadata for HDR swapchains.
        let hdr_metadata = presenting && adapter.caps.hdr_metadata;
        if hdr_metadata {
            extensions.push(ash::ext::hdr_metadata::NAME.as_ptr());
        }

        // The driver's statistics of compiled pipelines, on request: measuring shaders.
        let pipeline_statistics = adapter.caps.pipeline_executable_info
            && std::env::var("ENGINE_PIPELINE_STATS").is_ok_and(|v| v == "1");
        let mut e_statistics = vk::PhysicalDevicePipelineExecutablePropertiesFeaturesKHR::default()
            .pipeline_executable_info(true);
        if pipeline_statistics {
            extensions.push(ash::khr::pipeline_executable_properties::NAME.as_ptr());
            features2 = features2.push_next(&mut e_statistics);
        }

        // Latency extensions: query their features first (only for extensions the
        // adapter lists), then enable what is supported.
        let mut q_present_id = vk::PhysicalDevicePresentIdFeaturesKHR::default();
        let mut q_present_wait = vk::PhysicalDevicePresentWaitFeaturesKHR::default();
        let mut q_anti_lag = AntiLagFeaturesAmd::default();
        {
            let mut query = vk::PhysicalDeviceFeatures2::default();
            if presenting && adapter.caps.present_wait {
                query = query
                    .push_next(&mut q_present_id)
                    .push_next(&mut q_present_wait);
            }
            if presenting && adapter.caps.amd_anti_lag {
                query = query.push_next(&mut q_anti_lag);
            }
            // SAFETY: the chain only holds structures of extensions the device lists.
            unsafe {
                instance
                    .raw()
                    .get_physical_device_features2(adapter.physical, &mut query);
            }
        }
        let mut latency = LatencyExtensions::default();
        latency.present_id = presenting && q_present_id.present_id == vk::TRUE;
        latency.present_wait = latency.present_id && q_present_wait.present_wait == vk::TRUE;
        latency.nv_low_latency2 = latency.present_id && adapter.caps.nv_low_latency2;
        latency.amd_anti_lag = presenting && q_anti_lag.anti_lag == vk::TRUE;
        let mut e_present_id = vk::PhysicalDevicePresentIdFeaturesKHR::default().present_id(true);
        let mut e_present_wait =
            vk::PhysicalDevicePresentWaitFeaturesKHR::default().present_wait(true);
        let mut e_anti_lag = AntiLagFeaturesAmd {
            anti_lag: vk::TRUE,
            ..AntiLagFeaturesAmd::default()
        };
        if latency.present_id {
            extensions.push(ash::khr::present_id::NAME.as_ptr());
            features2 = features2.push_next(&mut e_present_id);
        }
        if latency.present_wait {
            extensions.push(ash::khr::present_wait::NAME.as_ptr());
            features2 = features2.push_next(&mut e_present_wait);
        }
        if latency.nv_low_latency2 {
            extensions.push(ash::nv::low_latency2::NAME.as_ptr());
        }
        if latency.amd_anti_lag {
            extensions.push(c"VK_AMD_anti_lag".as_ptr());
            features2 = features2.push_next(&mut e_anti_lag);
        }

        let create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&extensions)
            .push_next(&mut features2);
        // SAFETY: the adapter belongs to `instance`; every pointer in `create_info` refers
        // to locals that outlive the call; the features were checked as supported.
        let raw = unsafe {
            instance
                .raw()
                .create_device(adapter.physical, &create_info, None)
        }
        .map_err(RhiError::vk("vkCreateDevice"))?;

        let queue = |family: u32| Queue {
            // SAFETY: each family was requested with one queue.
            raw: unsafe { raw.get_device_queue(family, 0) },
            family,
        };
        Ok(Self {
            graphics: queue(graphics_family),
            async_compute: compute_family.map(queue),
            transfer: transfer_family.map(queue),
            raw,
            adapter: adapter.clone(),
            latency,
            hdr_metadata,
            pipeline_statistics,
            instance: instance.shared.clone(),
        })
    }

    /// Whether pipelines capture the driver's statistics ([`crate::Pipeline::statistics`]):
    /// set `ENGINE_PIPELINE_STATS=1` before creating the device, on a driver with
    /// `VK_KHR_pipeline_executable_properties`.
    pub fn pipeline_statistics_enabled(&self) -> bool {
        self.pipeline_statistics
    }

    /// Which latency extensions are enabled.
    pub fn latency_extensions(&self) -> LatencyExtensions {
        self.latency
    }

    /// Whether `VK_EXT_hdr_metadata` is enabled ([`Swapchain::set_hdr_metadata`]).
    ///
    /// [`Swapchain::set_hdr_metadata`]: crate::Swapchain::set_hdr_metadata
    pub fn hdr_metadata_enabled(&self) -> bool {
        self.hdr_metadata
    }

    /// The ash device, for calls this layer does not wrap yet.
    pub fn raw(&self) -> &ash::Device {
        &self.raw
    }

    /// The adapter the device was created on.
    pub fn adapter(&self) -> &Adapter {
        &self.adapter
    }

    /// The Vulkan physical device handle.
    pub fn physical(&self) -> vk::PhysicalDevice {
        self.adapter.physical
    }

    /// The ash instance (for physical-device queries).
    pub fn instance_raw(&self) -> &ash::Instance {
        &self.instance.raw
    }

    /// Whether `VK_EXT_debug_utils` is available for object names and markers.
    pub fn debug_utils_enabled(&self) -> bool {
        self.instance.debug_utils_enabled
    }

    /// Blocks until the device is idle (shutdown, tests).
    pub fn wait_idle(&self) -> Result<(), RhiError> {
        // SAFETY: the device is alive; no queue is used concurrently by this call's contract
        // (callers stop submitting first).
        unsafe { self.raw.device_wait_idle() }.map_err(RhiError::vk("vkDeviceWaitIdle"))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: all objects created from the device are owned by values that borrow or
        // outlive-check against it; waiting for idle first makes destruction safe for
        // in-flight work.
        unsafe {
            let _ = self.raw.device_wait_idle();
            self.raw.destroy_device(None);
        }
    }
}
