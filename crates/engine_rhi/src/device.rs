//! Logical device and queues.

use crate::adapter::Adapter;
use crate::error::RhiError;
use crate::instance::{Instance, InstanceShared};
use crate::pipeline_cache::{self, CacheIdentity, PipelineCacheLoad, PipelineCacheSave};
use ash::vk;
use core::ffi::CStr;
use core::sync::atomic::{AtomicU64, Ordering};
use engine_core::hash::state_hash64;
use engine_core::jobs::ThreadPool;
use std::path::Path;
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

/// Queues a library asks for besides the engine's ([`Device::with_extras`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtraQueues {
    /// More queues of the graphics family.
    pub graphics: u32,
    /// More queues of the compute family (of the graphics family without one).
    pub compute: u32,
    /// A queue of the family that decodes AV1 video, with the video extensions (ADR 0100),
    /// when the adapter has one: [`Device::video_decode`]. Asked for by games that play
    /// video: it costs the RTX 4060 Ti 2.5 ms of `vkCreateDevice` (50 against 47.5 ms) and
    /// the Radeon nothing.
    pub video_decode: bool,
}

/// Where a library's extra queues start: family and index, for graphics and compute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtraQueueStart {
    /// The graphics family and the first index after the engine's queues in it.
    pub graphics: (u32, u32),
    /// The compute family (the graphics family without one) and the first free index.
    pub compute: (u32, u32),
}

/// The logical device with the engine's required features enabled.
pub struct Device {
    /// Shared with every [`crate::Pipeline`]: ash's device is its function table, 1.5 KB,
    /// too large to copy into each (the renderers hold hundreds, on the stack while they
    /// are built).
    raw: Arc<ash::Device>,
    adapter: Adapter,
    latency: LatencyExtensions,
    /// `VK_EXT_hdr_metadata` is enabled.
    hdr_metadata: bool,
    /// `VK_EXT_full_screen_exclusive` is enabled.
    full_screen_exclusive: bool,
    /// `VK_EXT_image_view_min_lod` and its `minLod` feature are enabled.
    image_view_min_lod: bool,
    /// `shaderFloat16` and `shaderInt16` are enabled.
    shader_float16: bool,
    /// Ray queries and acceleration structures (ADR 0087), when enabled.
    ray_query: Option<crate::accel::RayQuery>,
    /// Reads the driver's statistics of pipelines, which capture them
    /// (`ENGINE_PIPELINE_STATS=1`); `None` when they do not.
    pipeline_statistics: Option<ash::khr::pipeline_executable_properties::Device>,
    /// Extensions a library asked for ([`Device::with_extensions`]) that were enabled.
    extra_extensions: Vec<std::ffi::CString>,
    /// Graphics + compute + transfer (+ present on a desktop GPU).
    pub graphics: Queue,
    /// A second queue of the graphics family, when the family has one: a paced present
    /// thread submits its copies and presents on it while the frame loop submits on
    /// [`Self::graphics`] (ADR 0091). A queue is externally synchronized, so two threads
    /// cannot share one.
    pub present: Option<Queue>,
    /// Where a library's extra queues start ([`Self::with_extras`]).
    extra_queue_start: ExtraQueueStart,
    /// A compute queue on a family without graphics (async compute), if the GPU has one.
    pub async_compute: Option<Queue>,
    /// A transfer-only queue (copy engine), if the GPU has one.
    pub transfer: Option<Queue>,
    /// A queue of the family that decodes AV1 video (ADR 0100): asked for with
    /// [`ExtraQueues::video_decode`], when the adapter has one
    /// ([`crate::OptionalCaps::video_decode_av1`]); the video extensions are enabled with
    /// it. `ENGINE_VIDEO_DECODE=0` leaves it out (to test the software decoder).
    pub video_decode: Option<Queue>,
    /// Every pipeline is created through it (ADR 0055); internally synchronized, so
    /// pipelines are created from many threads at once.
    pipeline_cache: vk::PipelineCache,
    /// GPU fault reports and breadcrumbs (ADR 0061); taken when the device is dropped.
    diagnostics: Option<Arc<crate::fault::Diagnostics>>,
    /// Hash of the cache data as loaded or last saved: unchanged data is not rewritten.
    saved_cache: AtomicU64,
    /// Creates pipelines in parallel ([`Self::create_parallel`]); without it, threads of
    /// its own.
    jobs: Option<Arc<ThreadPool>>,
    /// The thread that creates described pipelines in the background (ADR 0079), started
    /// with the first one; `None` inside when it could not start.
    warm_up: std::sync::OnceLock<Option<crate::lazy_pipeline::PipelineWarmUp>>,
    /// Pipelines described from now on are queued for the warm-up.
    warm_up_enabled: bool,
    // Keeps the instance alive until the device is destroyed.
    instance: Arc<InstanceShared>,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("adapter", &self.adapter.name())
            .field("graphics", &self.graphics)
            .field("present", &self.present)
            .field("async_compute", &self.async_compute)
            .field("transfer", &self.transfer)
            .field("video_decode", &self.video_decode)
            .finish()
    }
}

impl Device {
    /// Creates the device on `adapter`, which must be suitable
    /// ([`Adapter::missing_features`] empty).
    pub fn new(instance: &Instance, adapter: &Adapter) -> Result<Self, RhiError> {
        Self::with_extensions(instance, adapter, &[])
    }

    /// Like [`Self::new`], also enabling `extra` device extensions a library needs (the
    /// ones Streamline asks for DLSS, ADR 0063); they must need no feature structures.
    /// Those the adapter does not list are skipped: [`Self::extra_extension_enabled`]
    /// tells which were enabled.
    pub fn with_extensions(
        instance: &Instance,
        adapter: &Adapter,
        extra: &[&CStr],
    ) -> Result<Self, RhiError> {
        Self::with_extras(instance, adapter, extra, ExtraQueues::default())
    }

    /// Like [`Self::with_extensions`], also creating `queues` for a library after the
    /// engine's own in their families (Streamline's DLSS frame generation, ADR 0091; it
    /// gets them itself, from the indices [`Self::extra_queue_start`] gives).
    pub fn with_extras(
        instance: &Instance,
        adapter: &Adapter,
        extra: &[&CStr],
        queues: ExtraQueues,
    ) -> Result<Self, RhiError> {
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

        // The graphics family's second queue (ADR 0091) and a library's extra queues, at the
        // same priority.
        let priorities = [1.0f32; 16];
        let graphics_count = families[graphics_family as usize].queue_count as usize;
        let graphics_queues = graphics_count.min(2);
        let compute_extra = if compute_family.is_some() {
            queues.compute
        } else {
            0
        };
        let graphics_wanted = graphics_queues
            + queues.graphics as usize
            + if compute_family.is_none() {
                queues.compute as usize
            } else {
                0
            };
        if graphics_wanted > graphics_count.min(priorities.len()) {
            return Err(RhiError::NoSuitableAdapter(vec![(
                adapter.name().to_owned(),
                vec!["enough graphics queues for the library"],
            )]));
        }
        let mut queue_infos = vec![
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(graphics_family)
                .queue_priorities(&priorities[..graphics_wanted]),
        ];
        if let Some(family) = compute_family {
            let count = 1 + compute_extra as usize;
            if count > (families[family as usize].queue_count as usize).min(priorities.len()) {
                return Err(RhiError::NoSuitableAdapter(vec![(
                    adapter.name().to_owned(),
                    vec!["enough compute queues for the library"],
                )]));
            }
            queue_infos.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priorities[..count]),
            );
        }
        if let Some(family) = transfer_family {
            queue_infos.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priorities[..1]),
            );
        }
        // AV1 decoding when asked for (ADR 0100): one queue of its own family. A family
        // the engine already takes queues from is left alone (its queues are spoken for).
        let decode_family = adapter
            .video_decode_family
            .filter(|_| queues.video_decode)
            .filter(|&f| {
                f != graphics_family && Some(f) != compute_family && Some(f) != transfer_family
            })
            .filter(|_| std::env::var("ENGINE_VIDEO_DECODE").map_or(true, |v| v.trim() != "0"));
        if let Some(family) = decode_family {
            queue_infos.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priorities[..1]),
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
            .scalar_block_layout(true)
            // 16-bit math where supported: optional (FSR 3.1's faster variants, ADR 0060).
            .shader_float16(adapter.caps.shader_float16)
            // Meshlets drawn one indirect draw each, counted on the GPU (ADR 0085).
            .draw_indirect_count(adapter.caps.draw_indirect_count)
            // Vertex shaders that pick their viewport (ADR 0093).
            .shader_output_viewport_index(adapter.caps.viewport_index);
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true)
            .maintenance4(true)
            // Creating a pipeline only if the cache holds it (`Pipeline::create_many`).
            .pipeline_creation_cache_control(true)
            .shader_demote_to_helper_invocation(true)
            // Required of every Vulkan 1.3 device; libraries that tag the engine's objects
            // use it (Streamline for DLSS, ADR 0063).
            .private_data(true);
        let features = vk::PhysicalDeviceFeatures::default()
            .sampler_anisotropy(true)
            .texture_compression_bc(true)
            .multi_draw_indirect(true)
            .draw_indirect_first_instance(true)
            .image_cube_array(true)
            .shader_int16(adapter.caps.shader_float16)
            .shader_int64(adapter.caps.shader_int64)
            .sample_rate_shading(adapter.caps.sample_rate_shading)
            .fill_mode_non_solid(adapter.caps.fill_mode_non_solid)
            .multi_viewport(adapter.caps.viewport_index)
            .fragment_stores_and_atomics(adapter.caps.fragment_stores_and_atomics)
            .shader_clip_distance(adapter.caps.shader_clip_distance);
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
        if decode_family.is_some() {
            extensions.push(ash::khr::video_queue::NAME.as_ptr());
            extensions.push(ash::khr::video_decode_queue::NAME.as_ptr());
            extensions.push(ash::khr::video_decode_av1::NAME.as_ptr());
        }
        // Mastering metadata for HDR swapchains.
        let hdr_metadata = presenting && adapter.caps.hdr_metadata;
        if hdr_metadata {
            extensions.push(ash::ext::hdr_metadata::NAME.as_ptr());
        }
        // A swapchain taking its monitor for itself, or told never to (ADR 0099).
        let full_screen_exclusive = presenting
            && adapter.caps.full_screen_exclusive
            && instance.shared.surface_capabilities2_enabled;
        if full_screen_exclusive {
            extensions.push(ash::ext::full_screen_exclusive::NAME.as_ptr());
        }

        // A fractional first level per image view: streamed textures fade new levels in
        // (ADR 0078). The feature is queried: the extension alone does not promise it.
        let mut q_min_lod = vk::PhysicalDeviceImageViewMinLodFeaturesEXT::default();
        if adapter.caps.image_view_min_lod {
            let mut query = vk::PhysicalDeviceFeatures2::default().push_next(&mut q_min_lod);
            // SAFETY: the chain holds the structure of an extension the device lists.
            unsafe {
                instance
                    .raw()
                    .get_physical_device_features2(adapter.physical, &mut query);
            }
        }
        let image_view_min_lod = q_min_lod.min_lod == vk::TRUE;
        let mut e_min_lod = vk::PhysicalDeviceImageViewMinLodFeaturesEXT::default().min_lod(true);
        if image_view_min_lod {
            extensions.push(c"VK_EXT_image_view_min_lod".as_ptr());
            features2 = features2.push_next(&mut e_min_lod);
        }

        // Ray queries and acceleration structures (ADR 0087), when the adapter lists them
        // and `ENGINE_RAY_QUERY` is not `0` (to test what GPUs without them do): their
        // features are queried first.
        let wants_rays = adapter.caps.ray_query
            && std::env::var("ENGINE_RAY_QUERY").map_or(true, |v| v.trim() != "0");
        let mut q_accel = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default();
        let mut q_ray_query = vk::PhysicalDeviceRayQueryFeaturesKHR::default();
        let mut q_position_fetch = vk::PhysicalDeviceRayTracingPositionFetchFeaturesKHR::default();
        if wants_rays {
            let mut query = vk::PhysicalDeviceFeatures2::default()
                .push_next(&mut q_accel)
                .push_next(&mut q_ray_query);
            if adapter.caps.ray_position_fetch {
                query = query.push_next(&mut q_position_fetch);
            }
            // SAFETY: the chain holds structures of extensions the device lists.
            unsafe {
                instance
                    .raw()
                    .get_physical_device_features2(adapter.physical, &mut query);
            }
        }
        let rays = wants_rays
            && q_accel.acceleration_structure == vk::TRUE
            && q_ray_query.ray_query == vk::TRUE;
        let position_fetch = rays && q_position_fetch.ray_tracing_position_fetch == vk::TRUE;
        let mut e_accel = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default()
            .acceleration_structure(true);
        let mut e_ray_query = vk::PhysicalDeviceRayQueryFeaturesKHR::default().ray_query(true);
        let mut e_position_fetch = vk::PhysicalDeviceRayTracingPositionFetchFeaturesKHR::default()
            .ray_tracing_position_fetch(true);
        if rays {
            extensions.push(ash::khr::acceleration_structure::NAME.as_ptr());
            extensions.push(ash::khr::deferred_host_operations::NAME.as_ptr());
            extensions.push(ash::khr::ray_query::NAME.as_ptr());
            features2 = features2
                .push_next(&mut e_accel)
                .push_next(&mut e_ray_query);
            if position_fetch {
                extensions.push(ash::khr::ray_tracing_position_fetch::NAME.as_ptr());
                features2 = features2.push_next(&mut e_position_fetch);
            }
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

        // GPU crash diagnostics (ADR 0061): VK_EXT_device_fault, and the buffer markers of
        // the breadcrumbs (in AMD's uncached memory where available).
        let mut diagnostics_features =
            crate::fault::Features::query(instance.raw(), adapter.physical);
        extensions.extend(diagnostics_features.extensions().map(|e| e.as_ptr()));
        if diagnostics_features.fault.device_fault == vk::TRUE {
            features2 = features2.push_next(&mut diagnostics_features.fault);
        }
        if diagnostics_features.coherent.device_coherent_memory == vk::TRUE {
            features2 = features2.push_next(&mut diagnostics_features.coherent);
        }

        let mut extra_extensions = Vec::new();
        if !extra.is_empty() {
            // SAFETY: the adapter belongs to `instance`.
            let available = unsafe {
                instance
                    .raw()
                    .enumerate_device_extension_properties(adapter.physical)
            }
            .map_err(RhiError::vk("vkEnumerateDeviceExtensionProperties"))?;
            for &name in extra {
                if !available
                    .iter()
                    .any(|e| e.extension_name_as_c_str() == Ok(name))
                {
                    continue;
                }
                let listed = extensions.iter().any(|&e| {
                    // SAFETY: the list holds pointers to NUL-terminated static names.
                    (unsafe { CStr::from_ptr(e) }) == name
                });
                if !listed {
                    extensions.push(name.as_ptr());
                }
                extra_extensions.push(name.to_owned());
            }
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

        // SAFETY: the device was just created; the create info holds no pointers.
        let pipeline_cache = match unsafe {
            raw.create_pipeline_cache(&vk::PipelineCacheCreateInfo::default(), None)
        } {
            Ok(cache) => cache,
            Err(result) => {
                // SAFETY: nothing was created from the device yet.
                unsafe { raw.destroy_device(None) };
                return Err(RhiError::Vulkan {
                    call: "vkCreatePipelineCache",
                    result,
                });
            }
        };
        let queue = |family: u32| Queue {
            // SAFETY: each family was requested with at least one queue.
            raw: unsafe { raw.get_device_queue(family, 0) },
            family,
        };
        let present = (graphics_queues > 1).then(|| Queue {
            // SAFETY: the graphics family was requested with two queues.
            raw: unsafe { raw.get_device_queue(graphics_family, 1) },
            family: graphics_family,
        });
        let diagnostics =
            crate::fault::Diagnostics::new(instance.raw(), &raw, adapter, &diagnostics_features);
        let ray_query = rays.then(|| {
            crate::accel::RayQuery::new(instance.raw(), &raw, adapter.physical, position_fetch)
        });
        let pipeline_statistics = pipeline_statistics
            .then(|| ash::khr::pipeline_executable_properties::Device::new(instance.raw(), &raw));
        Ok(Self {
            pipeline_cache,
            diagnostics: Some(diagnostics),
            saved_cache: AtomicU64::new(state_hash64(&[])),
            jobs: None,
            warm_up: std::sync::OnceLock::new(),
            warm_up_enabled: true,
            graphics: queue(graphics_family),
            present,
            extra_queue_start: ExtraQueueStart {
                graphics: (graphics_family, graphics_queues as u32),
                compute: match compute_family {
                    Some(family) => (family, 1),
                    None => (graphics_family, graphics_queues as u32 + queues.graphics),
                },
            },
            async_compute: compute_family.map(queue),
            transfer: transfer_family.map(queue),
            video_decode: decode_family.map(queue),
            raw: Arc::new(raw),
            adapter: adapter.clone(),
            latency,
            hdr_metadata,
            full_screen_exclusive,
            image_view_min_lod,
            shader_float16: adapter.caps.shader_float16,
            ray_query,
            pipeline_statistics,
            extra_extensions,
            instance: instance.shared.clone(),
        })
    }

    /// Whether pipelines capture the driver's statistics ([`crate::Pipeline::statistics`]):
    /// set `ENGINE_PIPELINE_STATS=1` before creating the device, on a driver with
    /// `VK_KHR_pipeline_executable_properties`.
    pub fn pipeline_statistics_enabled(&self) -> bool {
        self.pipeline_statistics.is_some()
    }

    /// Reads the driver's statistics of this device's pipelines, when they capture them.
    pub(crate) fn pipeline_statistics_reader(
        &self,
    ) -> Option<&ash::khr::pipeline_executable_properties::Device> {
        self.pipeline_statistics.as_ref()
    }

    /// Where the extra queues of [`Self::with_extras`] start (the next index after the
    /// engine's own when none were asked for).
    pub fn extra_queue_start(&self) -> ExtraQueueStart {
        self.extra_queue_start
    }

    /// Whether `name`, passed to [`Self::with_extensions`], was enabled.
    pub fn extra_extension_enabled(&self, name: &CStr) -> bool {
        self.extra_extensions.iter().any(|e| e.as_c_str() == name)
    }

    /// The GPU breadcrumbs (ADR 0061): present when the GPU has `VK_AMD_buffer_marker`
    /// (AMD and NVIDIA drivers do) and `ENGINE_GPU_BREADCRUMBS` is not `0`. The render
    /// graph marks every pass with them.
    pub fn breadcrumbs(&self) -> Option<&Arc<crate::Breadcrumbs>> {
        self.diagnostics.as_ref()?.breadcrumbs.as_ref()
    }

    /// Whether `VK_EXT_device_fault` is enabled: device losses report the driver's fault
    /// description, faulting addresses and vendor dump ([`crate::last_gpu_fault`]).
    pub fn device_fault_enabled(&self) -> bool {
        self.diagnostics
            .as_ref()
            .is_some_and(|d| d.device_fault_enabled())
    }

    /// Which latency extensions are enabled.
    pub fn latency_extensions(&self) -> LatencyExtensions {
        self.latency
    }

    /// Whether image views can start at a fractional level (`VK_EXT_image_view_min_lod`
    /// with its `minLod` feature): streamed textures fade new levels in (ADR 0078).
    pub fn image_view_min_lod(&self) -> bool {
        self.image_view_min_lod
    }

    /// Ray queries and acceleration structures (ADR 0087), when the device has them
    /// enabled: `None` on GPUs without `VK_KHR_ray_query`, or with `ENGINE_RAY_QUERY=0`.
    pub fn ray_query(&self) -> Option<&crate::accel::RayQuery> {
        self.ray_query.as_ref()
    }

    /// Whether `VK_EXT_hdr_metadata` is enabled ([`Swapchain::set_hdr_metadata`]).
    ///
    /// [`Swapchain::set_hdr_metadata`]: crate::Swapchain::set_hdr_metadata
    pub fn hdr_metadata_enabled(&self) -> bool {
        self.hdr_metadata
    }

    /// Whether `VK_EXT_full_screen_exclusive` is enabled: swapchains follow their
    /// [`FullScreenExclusive`](crate::FullScreenExclusive) choice (without it, every
    /// choice is the driver's default).
    pub fn full_screen_exclusive_enabled(&self) -> bool {
        self.full_screen_exclusive
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

    /// The ash entry of the instance (global commands, `vkGetInstanceProcAddr`), for
    /// libraries that load Vulkan functions themselves.
    pub fn entry(&self) -> &ash::Entry {
        &self.instance.entry
    }

    /// Whether 16-bit float and integer math in shaders (`shaderFloat16`, `shaderInt16`) is
    /// enabled: [`crate::OptionalCaps::shader_float16`] of the adapter.
    pub fn shader_float16_enabled(&self) -> bool {
        self.shader_float16
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

    /// The pipeline cache every pipeline of this device is created through (ADR 0055).
    pub fn pipeline_cache(&self) -> vk::PipelineCache {
        self.pipeline_cache
    }

    /// A file name for this GPU's pipeline cache, `pipelines-<vendor>-<device>.bin`: a
    /// machine with two GPUs keeps a cache for each instead of replacing one with the
    /// other.
    pub fn pipeline_cache_file_name(&self) -> String {
        format!(
            "pipelines-{:04x}-{:04x}.bin",
            self.adapter.vendor_id, self.adapter.device_id
        )
    }

    /// Loads the pipeline cache [`Self::save_pipeline_cache`] wrote in an earlier run.
    /// Call it before creating the renderers: their pipelines then come from the cache
    /// instead of the driver's compiler (milliseconds instead of a second or more).
    ///
    /// Never fails: a missing file (the first run) or one that cannot be used (another GPU,
    /// a driver update, damage) leaves the cache as it was, and the result says why.
    pub fn load_pipeline_cache(&mut self, path: &Path) -> PipelineCacheLoad {
        engine_profiler::zone!("rhi::Device::load_pipeline_cache");
        let file = match std::fs::read(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return PipelineCacheLoad::Missing;
            }
            Err(e) => return PipelineCacheLoad::Rejected(format!("cannot read it: {e}")),
        };
        let data = match pipeline_cache::unwrap(&CacheIdentity::of(&self.adapter), &file) {
            Ok(data) => data,
            Err(reason) => return PipelineCacheLoad::Rejected(reason),
        };
        let info = vk::PipelineCacheCreateInfo::default().initial_data(data);
        // SAFETY: the data passed the envelope's checks and names this device in the
        // driver's own header.
        let loaded = match unsafe { self.raw.create_pipeline_cache(&info, None) } {
            Ok(cache) => cache,
            Err(result) => {
                return PipelineCacheLoad::Rejected(format!("the driver refused it: {result}"));
            }
        };
        // What the cache gathered before the load goes into the loaded one.
        // SAFETY: `&mut self`: no pipeline is being created through either cache.
        if let Err(result) = unsafe {
            self.raw
                .merge_pipeline_caches(loaded, &[self.pipeline_cache])
        } {
            // SAFETY: as above; `loaded` is unused.
            unsafe { self.raw.destroy_pipeline_cache(loaded, None) };
            return PipelineCacheLoad::Rejected(format!("cannot merge it: {result}"));
        }
        // SAFETY: as above; pipelines created through the old cache do not depend on it.
        unsafe { self.raw.destroy_pipeline_cache(self.pipeline_cache, None) };
        self.pipeline_cache = loaded;
        self.saved_cache
            .store(state_hash64(data), Ordering::Relaxed);
        PipelineCacheLoad::Loaded { bytes: data.len() }
    }

    /// Writes the pipeline cache to `path` for [`Self::load_pipeline_cache`] in the next
    /// run: after start-up, after a loading screen, at exit. Does nothing when the cache
    /// has not changed since it was loaded or last saved. The file is written next to
    /// `path` and renamed over it, so a crash never leaves half a file.
    pub fn save_pipeline_cache(&self, path: &Path) -> Result<PipelineCacheSave, RhiError> {
        engine_profiler::zone!("rhi::Device::save_pipeline_cache");
        // SAFETY: the cache belongs to this device; reading its data needs no exclusive
        // access.
        let data = unsafe { self.raw.get_pipeline_cache_data(self.pipeline_cache) }
            .map_err(RhiError::vk("vkGetPipelineCacheData"))?;
        let hash = state_hash64(&data);
        if hash == self.saved_cache.load(Ordering::Relaxed) {
            return Ok(PipelineCacheSave::Unchanged);
        }
        let file = pipeline_cache::wrap(&CacheIdentity::of(&self.adapter), &data);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|error| RhiError::Io {
                what: "creating the pipeline cache's directory",
                error,
            })?;
        }
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        std::fs::write(&temporary, &file).map_err(|error| RhiError::Io {
            what: "writing the pipeline cache",
            error,
        })?;
        std::fs::rename(&temporary, path).map_err(|error| RhiError::Io {
            what: "replacing the pipeline cache",
            error,
        })?;
        self.saved_cache.store(hash, Ordering::Relaxed);
        Ok(PipelineCacheSave::Written { bytes: file.len() })
    }

    /// Creates pipelines on `pool`'s threads ([`Self::create_parallel`]) instead of
    /// threads started for the purpose.
    pub fn set_job_pool(&mut self, pool: Arc<ThreadPool>) {
        self.jobs = Some(pool);
    }

    /// Calls `create(i)` for every `i` in `0..count` in parallel and returns the results
    /// in index order, or the error of the lowest index that failed. For creating many
    /// pipelines at once: drivers compile each on the calling thread, so a renderer's
    /// pipelines take the time of the slowest few instead of their sum.
    ///
    /// Runs on the pool given to [`Self::set_job_pool`], or else on up to one thread per
    /// core started for the call.
    pub fn create_parallel<T, F>(&self, count: usize, create: F) -> Result<Vec<T>, RhiError>
    where
        T: Send + Sync,
        F: Fn(usize) -> Result<T, RhiError> + Sync,
    {
        crate::pipeline::create_parallel(self.jobs.as_deref(), count, create)
    }

    /// Whether pipelines described from now on ([`crate::LazyPipeline::describe_many`])
    /// are created in the background by the device's warm-up thread, besides on first use
    /// (on by default; ADR 0079). Off, a variant a frame first needs is compiled while
    /// drawing: for measuring that, and tests that must not see the thread's work.
    pub fn set_pipeline_warm_up(&mut self, enabled: bool) {
        self.warm_up_enabled = enabled;
    }

    /// The warm-up thread, started with the first pipeline described, when it is on.
    pub(crate) fn pipeline_warm_up(&self) -> Option<&crate::lazy_pipeline::PipelineWarmUp> {
        if !self.warm_up_enabled {
            return None;
        }
        self.warm_up
            .get_or_init(|| {
                crate::lazy_pipeline::PipelineWarmUp::start()
                    .map_err(|e| log::warn!("pipelines are created on first use only: {e}"))
                    .ok()
            })
            .as_ref()
    }

    /// Pipelines the warm-up thread has queued and not created yet.
    pub fn pipelines_warming_up(&self) -> usize {
        self.warm_up
            .get()
            .and_then(Option::as_ref)
            .map_or(0, crate::lazy_pipeline::PipelineWarmUp::pending)
    }

    /// Waits until the warm-up thread has created every pipeline queued so far: for
    /// measurements and tests that must not see its work (its allocations, the driver's
    /// compiles).
    pub fn wait_for_pipeline_warm_up(&self) {
        if let Some(warm_up) = self.warm_up.get().and_then(Option::as_ref) {
            warm_up.wait();
        }
    }

    /// What creating pipelines needs from this device, to create them elsewhere: on first
    /// use while drawing, or on a warm-up thread ([`crate::LazyPipeline`], ADR 0079).
    pub fn pipeline_context(&self) -> crate::pipeline::PipelineContext {
        crate::pipeline::PipelineContext {
            device: Arc::clone(&self.raw),
            cache: self.pipeline_cache,
            statistics: self.pipeline_statistics.clone(),
            jobs: self.jobs.clone(),
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // The warm-up thread creates pipelines through the cache and the device destroyed
        // below: it stops first, after the pipeline it is creating.
        drop(self.warm_up.take());
        // SAFETY: all objects created from the device are owned by values that borrow or
        // outlive-check against it; waiting for idle first makes destruction safe for
        // in-flight work.
        unsafe {
            let _ = self.raw.device_wait_idle();
            if let Some(diagnostics) = self.diagnostics.take() {
                diagnostics.destroy();
            }
            self.raw.destroy_pipeline_cache(self.pipeline_cache, None);
            self.raw.destroy_device(None);
        }
    }
}
