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

/// The logical device with the engine's required features enabled.
pub struct Device {
    raw: ash::Device,
    adapter: Adapter,
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
            .shader_sampled_image_array_non_uniform_indexing(true)
            .shader_storage_image_array_non_uniform_indexing(true)
            .shader_storage_buffer_array_non_uniform_indexing(true)
            .scalar_block_layout(true);
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true)
            .maintenance4(true);
        let features = vk::PhysicalDeviceFeatures::default().sampler_anisotropy(true);
        let mut features2 = vk::PhysicalDeviceFeatures2::default()
            .features(features)
            .push_next(&mut f11)
            .push_next(&mut f12)
            .push_next(&mut f13);

        let mut extensions: Vec<*const i8> = Vec::new();
        if adapter.caps.swapchain && instance.surface_enabled() {
            extensions.push(ash::khr::swapchain::NAME.as_ptr());
        }
        if adapter.caps.memory_budget {
            extensions.push(ash::ext::memory_budget::NAME.as_ptr());
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
            instance: instance.shared.clone(),
        })
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
