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
    /// `VK_KHR_ray_query` + `VK_KHR_acceleration_structure` (+
    /// `VK_KHR_deferred_host_operations`, which the latter needs): rays traced from any
    /// shader (ADR 0087); their features are checked when the device is made.
    pub ray_query: bool,
    /// `VK_KHR_ray_tracing_position_fetch`: a hit triangle's positions read from the
    /// acceleration structure.
    pub ray_position_fetch: bool,
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
    /// `VK_KHR_pipeline_executable_properties` (the driver's statistics of a compiled
    /// pipeline: registers, occupancy, instructions).
    pub pipeline_executable_info: bool,
    /// The driver implements Vulkan 1.4. The engine's floor stays 1.3 (ADR 0006); the
    /// capabilities below were promoted to core in 1.4 and are used as extensions.
    pub vulkan_1_4: bool,
    /// `VK_KHR_maintenance5` (pipelines from SPIR-V without shader modules, sized index
    /// buffer binds).
    pub maintenance5: bool,
    /// `VK_KHR_maintenance6`.
    pub maintenance6: bool,
    /// `VK_KHR_push_descriptor`.
    pub push_descriptor: bool,
    /// `VK_EXT_host_image_copy` (image uploads from the CPU without staging buffers).
    pub host_image_copy: bool,
    /// `VK_KHR_dynamic_rendering_local_read` (reading attachments inside a render pass).
    pub dynamic_rendering_local_read: bool,
    /// `VK_KHR_load_store_op_none` or the EXT version.
    pub load_store_op_none: bool,
    /// `VK_KHR_shader_maximal_reconvergence` (defined wave behavior after divergence).
    pub maximal_reconvergence: bool,
    /// `VK_KHR_line_rasterization` or the EXT version (wide, smooth and stippled lines).
    pub line_rasterization: bool,
    /// `VK_KHR_index_type_uint8` or the EXT version.
    pub index_type_uint8: bool,
    /// `VK_EXT_hdr_metadata` (mastering metadata for HDR swapchains).
    pub hdr_metadata: bool,
    /// `VK_EXT_image_view_min_lod` (a fractional first level per image view: streamed
    /// textures fade their new levels in, ADR 0078); its `minLod` feature is checked when
    /// the device is made.
    pub image_view_min_lod: bool,
    /// `shaderFloat16` and `shaderInt16`: 16-bit arithmetic in shaders (the FSR 3.1
    /// upscaler's faster variants, ADR 0060).
    pub shader_float16: bool,
    /// `sampleRateShading`: fragment shaders that run per sample (MSAA's per-sample
    /// passes, ADR 0075); enabled when present.
    pub sample_rate_shading: bool,
}

impl OptionalCaps {
    /// Names of the capabilities present, for logs and reports.
    pub fn names(&self) -> Vec<&'static str> {
        [
            (self.mesh_shader, "mesh shaders"),
            (self.ray_tracing, "ray tracing"),
            (self.ray_query, "ray queries"),
            (self.descriptor_buffer, "descriptor buffer"),
            (self.graphics_pipeline_library, "pipeline library"),
            (self.pipeline_binary, "pipeline binary"),
            (self.present_wait, "present wait"),
            (self.nv_low_latency2, "Reflex"),
            (self.amd_anti_lag, "Anti-Lag 2"),
            (self.device_fault, "device fault"),
            (self.calibrated_timestamps, "calibrated timestamps"),
            (self.memory_budget, "memory budget"),
            (self.maintenance5, "maintenance5"),
            (self.maintenance6, "maintenance6"),
            (self.push_descriptor, "push descriptors"),
            (self.host_image_copy, "host image copy"),
            (self.dynamic_rendering_local_read, "local read"),
            (self.load_store_op_none, "load/store op none"),
            (self.maximal_reconvergence, "maximal reconvergence"),
            (self.line_rasterization, "line rasterization"),
            (self.index_type_uint8, "uint8 indices"),
            (self.hdr_metadata, "HDR metadata"),
            (self.shader_float16, "16-bit shader math"),
            (self.sample_rate_shading, "sample rate shading"),
        ]
        .into_iter()
        .filter_map(|(present, name)| present.then_some(name))
        .collect()
    }
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
    /// Driver version (vendor-specific encoding).
    pub driver_version: u32,
    /// Identifies the driver's pipeline cache format: caches from another UUID are
    /// unusable (another driver build).
    pub pipeline_cache_uuid: [u8; 16],
    /// The adapter's locally unique identifier (`deviceLUID`: Windows' `LUID`, low part
    /// first), when the driver gives one: what [`crate::Surface::display_adapter_luid`]
    /// is compared with.
    pub luid: Option<[u8; 8]>,
    /// Driver name and version string (`driverName driverInfo`).
    pub driver: String,
    /// Total size of device-local memory heaps in bytes.
    pub device_local_bytes: u64,
    /// Memory heaps and types.
    pub memory: vk::PhysicalDeviceMemoryProperties,
    /// Device limits (alignments, granularities, counts).
    pub limits: vk::PhysicalDeviceLimits,
    /// How multisampled depth can be resolved in a render pass (Vulkan 1.2's
    /// `supportedDepthResolveModes`; `SAMPLE_ZERO` always).
    pub depth_resolve_modes: vk::ResolveModeFlags,
    pub(crate) queue_families: Vec<vk::QueueFamilyProperties>,
    missing: Vec<&'static str>,
    /// Optional capabilities.
    pub caps: OptionalCaps,
}

impl Adapter {
    pub(crate) fn query(instance: &Instance, physical: vk::PhysicalDevice) -> Self {
        let raw = instance.raw();
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
        let mut resolve = vk::PhysicalDeviceDepthStencilResolveProperties::default();
        let mut ids = vk::PhysicalDeviceIDProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default()
            .push_next(&mut driver_props)
            .push_next(&mut subgroup)
            .push_next(&mut resolve)
            .push_next(&mut ids);
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
        let required: [(&'static str, vk::Bool32); 20] = [
            ("dynamicRendering", f13.dynamic_rendering),
            ("synchronization2", f13.synchronization2),
            ("maintenance4", f13.maintenance4),
            (
                "pipelineCreationCacheControl",
                f13.pipeline_creation_cache_control,
            ),
            // `discard` in alpha-tested materials: the lane becomes a helper, so the
            // quad's derivatives stay defined (required by Vulkan 1.3).
            (
                "shaderDemoteToHelperInvocation",
                f13.shader_demote_to_helper_invocation,
            ),
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
                "descriptorBindingUpdateUnusedWhilePending",
                f12.descriptor_binding_update_unused_while_pending,
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
        for (name, supported) in [
            ("samplerAnisotropy", base.sampler_anisotropy),
            // Block-compressed textures (BC1-7, ADR 0041): every desktop GPU.
            ("textureCompressionBC", base.texture_compression_bc),
            // GPU-driven drawing (ADR 0034): many indirect draws per call, each with its
            // own first instance.
            ("multiDrawIndirect", base.multi_draw_indirect),
            (
                "drawIndirectFirstInstance",
                base.draw_indirect_first_instance,
            ),
            // Reflection probes in one cube array (ADR 0073): every desktop GPU.
            ("imageCubeArray", base.image_cube_array),
        ] {
            if supported == vk::FALSE {
                missing.push(name);
            }
        }
        // Wave-level aggregation in compute (GPU culling, ADR 0034); wave sums and
        // prefix sums (the GPU sort, ADR 0080). Every Vulkan 1.3 desktop GPU has them.
        let subgroup_ops = vk::SubgroupFeatureFlags::BASIC
            | vk::SubgroupFeatureFlags::VOTE
            | vk::SubgroupFeatureFlags::BALLOT
            | vk::SubgroupFeatureFlags::ARITHMETIC;
        if !subgroup
            .supported_stages
            .contains(vk::ShaderStageFlags::COMPUTE)
            || !subgroup.supported_operations.contains(subgroup_ops)
        {
            missing.push("subgroup basic/vote/ballot/arithmetic operations in compute");
        }

        let caps = OptionalCaps {
            swapchain: has(ash::khr::swapchain::NAME),
            mesh_shader: has(ash::ext::mesh_shader::NAME),
            ray_tracing: has(ash::khr::ray_tracing_pipeline::NAME)
                && has(ash::khr::acceleration_structure::NAME),
            ray_query: has(ash::khr::ray_query::NAME)
                && has(ash::khr::acceleration_structure::NAME)
                && has(ash::khr::deferred_host_operations::NAME),
            ray_position_fetch: has(ash::khr::ray_tracing_position_fetch::NAME),
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
            pipeline_executable_info: has(ash::khr::pipeline_executable_properties::NAME),
            // Promoted to core in Vulkan 1.4; ash 0.38 predates some of them, so all are
            // compared by name. 1.4 drivers still list them as extensions.
            vulkan_1_4: props.api_version >= vk::make_api_version(0, 1, 4, 0),
            maintenance5: has(c"VK_KHR_maintenance5"),
            maintenance6: has(c"VK_KHR_maintenance6"),
            push_descriptor: has(c"VK_KHR_push_descriptor"),
            host_image_copy: has(c"VK_EXT_host_image_copy"),
            dynamic_rendering_local_read: has(c"VK_KHR_dynamic_rendering_local_read"),
            load_store_op_none: has(c"VK_KHR_load_store_op_none")
                || has(c"VK_EXT_load_store_op_none"),
            maximal_reconvergence: has(c"VK_KHR_shader_maximal_reconvergence"),
            line_rasterization: has(c"VK_KHR_line_rasterization")
                || has(c"VK_EXT_line_rasterization"),
            index_type_uint8: has(c"VK_KHR_index_type_uint8") || has(c"VK_EXT_index_type_uint8"),
            hdr_metadata: has(c"VK_EXT_hdr_metadata"),
            image_view_min_lod: has(c"VK_EXT_image_view_min_lod"),
            shader_float16: f12.shader_float16 == vk::TRUE && base.shader_int16 == vk::TRUE,
            sample_rate_shading: base.sample_rate_shading == vk::TRUE,
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
            driver_version: props.driver_version,
            pipeline_cache_uuid: props.pipeline_cache_uuid,
            luid: (ids.device_luid_valid == vk::TRUE).then_some(ids.device_luid),
            driver: format!("{driver_name} {driver_info}").trim().to_owned(),
            device_local_bytes,
            memory,
            limits: props.limits,
            depth_resolve_modes: resolve.supported_depth_resolve_modes,
            queue_families,
            missing,
            caps,
        }
    }

    /// Device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The Vulkan physical device handle, for libraries that query it themselves.
    pub fn physical(&self) -> vk::PhysicalDevice {
        self.physical
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
