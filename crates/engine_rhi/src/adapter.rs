//! Physical devices: properties, required features, optional capabilities, scoring.

use crate::instance::Instance;
use ash::vk;
use core::ffi::CStr;

/// Kind of physical device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterKind {
    /// Dedicated GPU.
    Discrete,
    /// GPU sharing memory with the CPU.
    Integrated,
    /// GPU exposed through virtualization.
    Virtual,
    /// Software implementation (lavapipe, SwiftShader).
    Cpu,
    /// Anything else.
    Other,
}

/// Which adapter to prefer among suitable ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterPreference {
    /// Discrete GPUs first, then the most device-local memory (default for games).
    HighPerformance,
    /// Integrated GPUs first (battery).
    LowPower,
    /// The first adapter whose name contains this text (case-insensitive), for example
    /// `"radeon"` to test the iGPU on ref-1. Falls back to `HighPerformance` order.
    NameContains(String),
}

impl AdapterPreference {
    /// Reads `ENGINE_ADAPTER`: `high-performance`, `low-power`, or any other text as a
    /// name filter.
    pub fn from_env() -> Option<Self> {
        let value = std::env::var("ENGINE_ADAPTER").ok()?;
        let value = value.trim();
        Some(match value.to_ascii_lowercase().as_str() {
            "" => return None,
            "high-performance" | "discrete" => Self::HighPerformance,
            "low-power" | "integrated" => Self::LowPower,
            _ => Self::NameContains(value.to_owned()),
        })
    }
}

/// Optional device extensions and features the engine can use when present (§6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OptionalCaps {
    /// `VK_KHR_swapchain`.
    pub swapchain: bool,
    /// `VK_EXT_mesh_shader`.
    pub mesh_shader: bool,
    /// `VK_KHR_ray_tracing_pipeline` + `VK_KHR_acceleration_structure`.
    pub ray_tracing: bool,
    /// `VK_EXT_descriptor_buffer`.
    pub descriptor_buffer: bool,
    /// `VK_EXT_graphics_pipeline_library`.
    pub graphics_pipeline_library: bool,
    /// `VK_KHR_pipeline_binary`.
    pub pipeline_binary: bool,
    /// `VK_KHR_present_wait` + `VK_KHR_present_id`.
    pub present_wait: bool,
    /// `VK_NV_low_latency2` (Reflex without DLLs, §7).
    pub nv_low_latency2: bool,
    /// `VK_AMD_anti_lag` (Anti-Lag 2, §7).
    pub amd_anti_lag: bool,
    /// `VK_EXT_device_fault` (GPU crash diagnostics).
    pub device_fault: bool,
    /// `VK_KHR_calibrated_timestamps` or the EXT version (CPU/GPU timeline correlation).
    pub calibrated_timestamps: bool,
    /// `VK_EXT_memory_budget` (per-heap budget and usage from the OS).
    pub memory_budget: bool,
}

/// A physical device and what it can do.
#[derive(Debug, Clone)]
pub struct Adapter {
    pub(crate) physical: vk::PhysicalDevice,
    name: String,
    /// PCI vendor id (0x10DE NVIDIA, 0x1002 AMD, 0x8086 Intel).
    pub vendor_id: u32,
    /// PCI device id.
    pub device_id: u32,
    /// Kind of device.
    pub kind: AdapterKind,
    /// Supported Vulkan version.
    pub api_version: u32,
    /// Driver name and version string (`driverName driverInfo`).
    pub driver: String,
    /// Total size of device-local memory heaps in bytes.
    pub device_local_bytes: u64,
    /// Memory heaps and types.
    pub memory: vk::PhysicalDeviceMemoryProperties,
    /// Device limits (alignments, granularities, counts).
    pub limits: vk::PhysicalDeviceLimits,
    pub(crate) queue_families: Vec<vk::QueueFamilyProperties>,
    missing: Vec<&'static str>,
    /// Optional capabilities.
    pub caps: OptionalCaps,
}

impl Adapter {
    pub(crate) fn query(instance: &Instance, physical: vk::PhysicalDevice) -> Self {
        let raw = instance.raw();
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        // SAFETY: `physical` came from this instance; the output chain is valid.
        unsafe { raw.get_physical_device_properties2(physical, &mut props2) };
        let props = props2.properties;

        let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
        let mut features2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut f11)
            .push_next(&mut f12)
            .push_next(&mut f13);
        // SAFETY: as above.
        unsafe { raw.get_physical_device_features2(physical, &mut features2) };
        let base = features2.features;

        // SAFETY: as above.
        let memory = unsafe { raw.get_physical_device_memory_properties(physical) };
        let device_local_bytes = memory.memory_heaps[..memory.memory_heap_count as usize]
            .iter()
            .filter(|h| h.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
            .map(|h| h.size)
            .sum();
        // SAFETY: as above.
        let queue_families = unsafe { raw.get_physical_device_queue_family_properties(physical) };
        // SAFETY: as above.
        let extensions =
            unsafe { raw.enumerate_device_extension_properties(physical) }.unwrap_or_default();
        let has = |name: &CStr| {
            extensions
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name))
        };

        let mut missing = Vec::new();
        if props.api_version < vk::API_VERSION_1_3 {
            missing.push("Vulkan 1.3");
        }
        let required: [(&'static str, vk::Bool32); 17] = [
            ("dynamicRendering", f13.dynamic_rendering),
            ("synchronization2", f13.synchronization2),
            ("maintenance4", f13.maintenance4),
            ("timelineSemaphore", f12.timeline_semaphore),
            ("bufferDeviceAddress", f12.buffer_device_address),
            ("descriptorIndexing", f12.descriptor_indexing),
            ("runtimeDescriptorArray", f12.runtime_descriptor_array),
            (
                "descriptorBindingPartiallyBound",
                f12.descriptor_binding_partially_bound,
            ),
            (
                "descriptorBindingVariableDescriptorCount",
                f12.descriptor_binding_variable_descriptor_count,
            ),
            (
                "descriptorBindingSampledImageUpdateAfterBind",
                f12.descriptor_binding_sampled_image_update_after_bind,
            ),
            (
                "descriptorBindingStorageImageUpdateAfterBind",
                f12.descriptor_binding_storage_image_update_after_bind,
            ),
            (
                "descriptorBindingStorageBufferUpdateAfterBind",
                f12.descriptor_binding_storage_buffer_update_after_bind,
            ),
            (
                "shaderSampledImageArrayNonUniformIndexing",
                f12.shader_sampled_image_array_non_uniform_indexing,
            ),
            (
                "shaderStorageImageArrayNonUniformIndexing",
                f12.shader_storage_image_array_non_uniform_indexing,
            ),
            (
                "shaderStorageBufferArrayNonUniformIndexing",
                f12.shader_storage_buffer_array_non_uniform_indexing,
            ),
            ("scalarBlockLayout", f12.scalar_block_layout),
            ("shaderDrawParameters", f11.shader_draw_parameters),
        ];
        missing.extend(
            required
                .iter()
                .filter(|(_, v)| *v == vk::FALSE)
                .map(|(n, _)| *n),
        );
        if base.sampler_anisotropy == vk::FALSE {
            missing.push("samplerAnisotropy");
        }

        let caps = OptionalCaps {
            swapchain: has(ash::khr::swapchain::NAME),
            mesh_shader: has(ash::ext::mesh_shader::NAME),
            ray_tracing: has(ash::khr::ray_tracing_pipeline::NAME)
                && has(ash::khr::acceleration_structure::NAME),
            descriptor_buffer: has(ash::ext::descriptor_buffer::NAME),
            graphics_pipeline_library: has(ash::ext::graphics_pipeline_library::NAME),
            // Newer than the headers in ash 0.38: compared by name.
            pipeline_binary: has(c"VK_KHR_pipeline_binary"),
            present_wait: has(ash::khr::present_wait::NAME) && has(ash::khr::present_id::NAME),
            nv_low_latency2: has(ash::nv::low_latency2::NAME),
            amd_anti_lag: has(c"VK_AMD_anti_lag"),
            device_fault: has(ash::ext::device_fault::NAME),
            calibrated_timestamps: has(ash::khr::calibrated_timestamps::NAME)
                || has(ash::ext::calibrated_timestamps::NAME),
            memory_budget: has(ash::ext::memory_budget::NAME),
        };

        let kind = match props.device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => AdapterKind::Discrete,
            vk::PhysicalDeviceType::INTEGRATED_GPU => AdapterKind::Integrated,
            vk::PhysicalDeviceType::VIRTUAL_GPU => AdapterKind::Virtual,
            vk::PhysicalDeviceType::CPU => AdapterKind::Cpu,
            _ => AdapterKind::Other,
        };
        let name = props
            .device_name_as_c_str()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let driver_name = driver_props
            .driver_name_as_c_str()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let driver_info = driver_props
            .driver_info_as_c_str()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        Self {
            physical,
            name,
            vendor_id: props.vendor_id,
            device_id: props.device_id,
            kind,
            api_version: props.api_version,
            driver: format!("{driver_name} {driver_info}").trim().to_owned(),
            device_local_bytes,
            memory,
            limits: props.limits,
            queue_families,
            missing,
            caps,
        }
    }

    /// Device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn info_name(&self) -> &str {
        &self.name
    }

    /// Properties of queue family `family` (queue flags, timestamp valid bits, …).
    pub fn queue_family(&self, family: u32) -> Option<&vk::QueueFamilyProperties> {
        self.queue_families.get(family as usize)
    }

    /// Required features this adapter lacks. Empty means suitable.
    pub fn missing_features(&self) -> &[&'static str] {
        &self.missing
    }

    /// Ordering key for selection: higher is better.
    pub(crate) fn score(&self, preference: &AdapterPreference) -> (u8, u8, u64) {
        let kind_rank = |order: [AdapterKind; 4]| {
            order
                .iter()
                .position(|k| *k == self.kind)
                .map_or(0, |p| 4 - p as u8)
        };
        let high = [
            AdapterKind::Discrete,
            AdapterKind::Integrated,
            AdapterKind::Virtual,
            AdapterKind::Cpu,
        ];
        let low = [
            AdapterKind::Integrated,
            AdapterKind::Discrete,
            AdapterKind::Virtual,
            AdapterKind::Cpu,
        ];
        match preference {
            AdapterPreference::HighPerformance => (0, kind_rank(high), self.device_local_bytes),
            AdapterPreference::LowPower => (0, kind_rank(low), self.device_local_bytes),
            AdapterPreference::NameContains(text) => {
                let hit = self
                    .name
                    .to_ascii_lowercase()
                    .contains(&text.to_ascii_lowercase());
                (u8::from(hit), kind_rank(high), self.device_local_bytes)
            }
        }
    }

    /// One-line description for logs, e.g.
    /// `NVIDIA GeForce RTX 4060 Ti (discrete, Vulkan 1.4.351, 8.0 GiB, NVIDIA 616.92)`.
    pub fn summary(&self) -> String {
        let kind = match self.kind {
            AdapterKind::Discrete => "discrete",
            AdapterKind::Integrated => "integrated",
            AdapterKind::Virtual => "virtual",
            AdapterKind::Cpu => "cpu",
            AdapterKind::Other => "other",
        };
        format!(
            "{} ({kind}, Vulkan {}.{}.{}, {:.1} GiB, {})",
            self.name,
            vk::api_version_major(self.api_version),
            vk::api_version_minor(self.api_version),
            vk::api_version_patch(self.api_version),
            self.device_local_bytes as f64 / (1u64 << 30) as f64,
            self.driver
        )
    }
}
