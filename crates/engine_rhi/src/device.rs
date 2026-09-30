//! Logical device and queues.

use crate::adapter::Adapter;
use crate::error::RhiError;
use crate::instance::{Instance, InstanceShared};
use crate::pipeline_cache::{self, CacheIdentity, PipelineCacheLoad, PipelineCacheSave};
use ash::vk;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use engine_core::hash::state_hash64;
use engine_core::jobs::ThreadPool;
use std::path::Path;
use std::sync::{Arc, OnceLock};

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
    /// Every pipeline is created through it (ADR 0055); internally synchronized, so
    /// pipelines are created from many threads at once.
    pipeline_cache: vk::PipelineCache,
    /// Hash of the cache data as loaded or last saved: unchanged data is not rewritten.
    saved_cache: AtomicU64,
    /// Creates pipelines in parallel ([`Self::create_parallel`]); without it, threads of
    /// its own.
    jobs: Option<Arc<ThreadPool>>,
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
            // Creating a pipeline only if the cache holds it (`Pipeline::create_many`).
            .pipeline_creation_cache_control(true)
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
            // SAFETY: each family was requested with one queue.
            raw: unsafe { raw.get_device_queue(family, 0) },
            family,
        };
        Ok(Self {
            pipeline_cache,
            saved_cache: AtomicU64::new(state_hash64(&[])),
            jobs: None,
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
        engine_profiler::zone!("rhi::Device::create_parallel");
        if count <= 1 {
            return (0..count).map(create).collect();
        }
        let slots: Vec<OnceLock<Result<T, RhiError>>> =
            (0..count).map(|_| OnceLock::new()).collect();
        let run = |i: usize| {
            let _ = slots[i].set(create(i));
        };
        match &self.jobs {
            Some(pool) => pool.parallel_for(count, 1, |_, range| range.for_each(run)),
            None => {
                let next = AtomicUsize::new(0);
                let work = || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break;
                        }
                        run(i);
                    }
                };
                let threads = std::thread::available_parallelism()
                    .map_or(4, |n| n.get())
                    .min(count);
                std::thread::scope(|scope| {
                    for _ in 1..threads {
                        // A thread that cannot start leaves its share to the others.
                        let _ = std::thread::Builder::new()
                            .name(String::from("engine-pipelines"))
                            .spawn_scoped(scope, work);
                    }
                    work();
                });
            }
        }
        slots
            .into_iter()
            .map(|slot| {
                slot.into_inner().unwrap_or(Err(RhiError::Vulkan {
                    call: "parallel creation (a job did not run)",
                    result: vk::Result::ERROR_UNKNOWN,
                }))
            })
            .collect()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: all objects created from the device are owned by values that borrow or
        // outlive-check against it; waiting for idle first makes destruction safe for
        // in-flight work.
        unsafe {
            let _ = self.raw.device_wait_idle();
            self.raw.destroy_pipeline_cache(self.pipeline_cache, None);
            self.raw.destroy_device(None);
        }
    }
}
