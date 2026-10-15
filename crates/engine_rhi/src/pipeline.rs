//! Graphics and compute pipelines (ADR 0013).
//!
//! Every pipeline uses the [`crate::Bindless`] pipeline layout. Graphics pipelines use
//! dynamic rendering (attachment formats instead of render passes), dynamic viewport and
//! scissor, and **no vertex input state**: vertices are pulled from buffers through
//! buffer device addresses (the GPU-driven path, CLAUDE.md §6).

use crate::device::Device;
use crate::error::RhiError;
use ash::vk;
use core::ffi::CStr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use engine_core::jobs::ThreadPool;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// A SPIR-V entry point.
#[derive(Debug, Clone, Copy)]
pub struct ShaderStageDesc<'a> {
    /// SPIR-V words.
    pub spirv: &'a [u32],
    /// Entry point name in the module.
    pub entry: &'a CStr,
}

/// Color blending of all color attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    /// Overwrite.
    Opaque,
    /// `src * a + dst * (1 - a)`.
    Alpha,
    /// `src + dst`.
    Additive,
    /// Premultiplied alpha: `src + dst * (1 - src.a)`. A source with `a = 0` adds, so
    /// alpha-blended and additive draws share one pipeline.
    Premultiplied,
    /// Multiply with a premultiplied source: `src * dst + dst * (1 - src.a)` for color
    /// (`dst * lerp(1, color, a)`); destination alpha is kept.
    Multiply,
    /// The average of source and destination, every channel: `(src + dst) / 2` (the
    /// second of two images averaged in place, ADR 0075).
    Half,
}

/// Depth bias for shadow maps: `constant` units plus `slope` × the polygon's depth slope,
/// at most `clamp` (0 = no clamp).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthBias {
    /// Constant factor.
    pub constant: f32,
    /// Slope factor.
    pub slope: f32,
    /// Largest bias (0 disables the clamp).
    pub clamp: f32,
}

/// Fixed-function state and formats of a graphics pipeline.
#[derive(Debug, Clone, Copy)]
pub struct GraphicsPipelineDesc<'a> {
    /// Vertex (or mesh) stage.
    pub vertex: ShaderStageDesc<'a>,
    /// Fragment stage, if any (depth-only passes have none).
    pub fragment: Option<ShaderStageDesc<'a>>,
    /// Formats of the color attachments, in location order.
    pub color_formats: &'a [vk::Format],
    /// Depth attachment format, or `UNDEFINED` for none.
    pub depth_format: vk::Format,
    /// Primitive topology.
    pub topology: vk::PrimitiveTopology,
    /// Faces to cull.
    pub cull_mode: vk::CullModeFlags,
    /// Winding of front faces (counter-clockwise with the engine's projection).
    pub front_face: vk::FrontFace,
    /// Cull mode and front face set while recording (`vkCmdSetCullMode`,
    /// `vkCmdSetFrontFace`, core in Vulkan 1.3) instead of `cull_mode` and `front_face`:
    /// one pipeline draws single- and double-sided, mirrored and unmirrored geometry.
    pub dynamic_culling: bool,
    /// Depth test enabled.
    pub depth_test: bool,
    /// Depth writes enabled.
    pub depth_write: bool,
    /// Depth comparison (`GREATER_OR_EQUAL` for reversed Z).
    pub depth_compare: vk::CompareOp,
    /// Color blending.
    pub blend: Blend,
    /// Depth bias (shadow maps), if any.
    pub depth_bias: Option<DepthBias>,
    /// Samples per pixel.
    pub samples: vk::SampleCountFlags,
    /// Alpha to coverage: the fragment's alpha at location 0 masks the samples it covers
    /// (multisampled targets; ADR 0075).
    pub alpha_to_coverage: bool,
    /// Specialization constants of every stage: value `i` is `constant_id` `i`. The driver
    /// compiles the pipeline with them, so branches on them cost nothing (and code behind
    /// a false one takes no registers).
    pub specialization: &'a [u32],
    /// Viewports and scissors, set while recording: more than one only with
    /// [`crate::OptionalCaps::viewport_index`], the vertex shader picking one.
    pub viewports: u32,
}

impl<'a> GraphicsPipelineDesc<'a> {
    /// Opaque triangles with back-face culling, depth test and write (`LESS`), no MSAA.
    pub fn new(vertex: ShaderStageDesc<'a>, fragment: Option<ShaderStageDesc<'a>>) -> Self {
        Self {
            vertex,
            fragment,
            color_formats: &[],
            depth_format: vk::Format::UNDEFINED,
            topology: vk::PrimitiveTopology::TRIANGLE_LIST,
            cull_mode: vk::CullModeFlags::BACK,
            front_face: vk::FrontFace::COUNTER_CLOCKWISE,
            dynamic_culling: false,
            depth_test: true,
            depth_write: true,
            depth_compare: vk::CompareOp::LESS,
            blend: Blend::Opaque,
            depth_bias: None,
            samples: vk::SampleCountFlags::TYPE_1,
            alpha_to_coverage: false,
            specialization: &[],
            viewports: 1,
        }
    }
}

/// A pipeline to create with others ([`Pipeline::create_many`]).
#[derive(Debug, Clone, Copy)]
pub enum PipelineDesc<'a> {
    /// A graphics pipeline.
    Graphics(GraphicsPipelineDesc<'a>),
    /// A compute pipeline.
    Compute {
        /// The compute stage.
        stage: ShaderStageDesc<'a>,
        /// Specialization constants: value `i` is `constant_id` `i`.
        specialization: &'a [u32],
    },
}

/// One statistic the driver reports for a stage of a compiled pipeline
/// ([`Pipeline::statistics`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineStatistic {
    /// The executable (stage) it describes, as the driver names it.
    pub stage: String,
    /// The statistic's name, as the driver names it ("VGPRs", "Register count", ...).
    pub name: String,
    /// Its value.
    pub value: f64,
}

/// A pipeline's driver statistics, recorded when it was created on a device that
/// captures them (`ENGINE_PIPELINE_STATS=1`): checks over every pipeline a run made read
/// them with [`take_pipeline_records`] (the perf gate's register check, ADR 0062).
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineRecord {
    /// Each stage's entry point and the hash of its SPIR-V ([`spirv_hash`]), vertex (or
    /// mesh) stage first.
    pub stages: Vec<(String, u128)>,
    /// Its specialization constants.
    pub specialization: Vec<u32>,
    /// What a graphics pipeline draws to, which the compiled code can depend on: the color
    /// formats, then `/` and the depth format, ` x` and the samples per pixel, ` a2c` for
    /// alpha to coverage (as `R16G16B16A16_SFLOAT/D32_SFLOAT x4`). Empty for compute.
    pub attachments: String,
    /// The statistics, as [`Pipeline::statistics`] returns them.
    pub statistics: Vec<PipelineStatistic>,
}

/// The records of pipelines created while their device captured statistics, until
/// [`take_pipeline_records`] takes them.
static RECORDS: Mutex<Vec<PipelineRecord>> = Mutex::new(Vec::new());

/// Takes the records of the pipelines created, in this process and on any device, since
/// the last call: none unless a device captures statistics
/// ([`Device::pipeline_statistics_enabled`]).
pub fn take_pipeline_records() -> Vec<PipelineRecord> {
    core::mem::take(&mut *RECORDS.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Hash of a SPIR-V module: of its bytes, as its `.spv` file holds them.
pub fn spirv_hash(words: &[u32]) -> u128 {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    engine_core::hash::hash128(&bytes)
}

/// Records the statistics of `raw`, just created from `stages` (see [`PipelineRecord`]).
fn record(
    reader: &ash::khr::pipeline_executable_properties::Device,
    raw: vk::Pipeline,
    stages: &[ShaderStageDesc<'_>],
    specialization: &[u32],
    attachments: String,
) {
    let record = PipelineRecord {
        stages: stages
            .iter()
            .map(|s| (s.entry.to_string_lossy().into_owned(), spirv_hash(s.spirv)))
            .collect(),
        specialization: specialization.to_vec(),
        attachments,
        statistics: read_statistics(reader, raw),
    };
    RECORDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(record);
}

/// [`PipelineRecord::attachments`] of `desc`.
fn record_attachments(desc: &GraphicsPipelineDesc<'_>) -> String {
    let colors: Vec<String> = desc
        .color_formats
        .iter()
        .map(|f| format!("{f:?}"))
        .collect();
    let mut text = colors.join(",");
    if desc.depth_format != vk::Format::UNDEFINED {
        text.push_str(&format!("/{:?}", desc.depth_format));
    }
    if desc.samples != vk::SampleCountFlags::TYPE_1 {
        text.push_str(&format!(" x{}", desc.samples.as_raw()));
    }
    if desc.alpha_to_coverage {
        text.push_str(" a2c");
    }
    text
}

/// The driver's statistics of each stage of `raw`, which was created with
/// `CAPTURE_STATISTICS` on the device `reader` reads.
fn read_statistics(
    reader: &ash::khr::pipeline_executable_properties::Device,
    raw: vk::Pipeline,
) -> Vec<PipelineStatistic> {
    let mut out = Vec::new();
    let info = vk::PipelineInfoKHR::default().pipeline(raw);
    // SAFETY: the pipeline was created with CAPTURE_STATISTICS by this device, and the
    // extension is enabled.
    let Ok(executables) = (unsafe { reader.get_pipeline_executable_properties(&info) }) else {
        return out;
    };
    for (index, executable) in executables.iter().enumerate() {
        let stage = executable
            .name_as_c_str()
            .map_or_else(|_| format!("{index}"), |n| n.to_string_lossy().into_owned());
        let info = vk::PipelineExecutableInfoKHR::default()
            .pipeline(raw)
            .executable_index(index as u32);
        // SAFETY: as above; the index comes from the driver.
        let Ok(statistics) = (unsafe { reader.get_pipeline_executable_statistics(&info) }) else {
            continue;
        };
        for statistic in statistics {
            // SAFETY: the union member matches the format the driver reports.
            let value = unsafe {
                match statistic.format {
                    vk::PipelineExecutableStatisticFormatKHR::BOOL32 => {
                        f64::from(statistic.value.b32)
                    }
                    vk::PipelineExecutableStatisticFormatKHR::INT64 => statistic.value.i64 as f64,
                    vk::PipelineExecutableStatisticFormatKHR::UINT64 => statistic.value.u64 as f64,
                    _ => statistic.value.f64,
                }
            };
            out.push(PipelineStatistic {
                stage: stage.clone(),
                name: statistic
                    .name_as_c_str()
                    .map_or_else(|_| String::new(), |n| n.to_string_lossy().into_owned()),
                value,
            });
        }
    }
    out
}

/// Pipeline creation in this process, over all devices ([`creation_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineCreationStats {
    /// Pipelines created.
    pub created: u64,
    /// Pipelines [`Pipeline::create_many`] found in the pipeline cache.
    pub cache_hits: u64,
    /// Pipelines [`Pipeline::create_many`] did not find there and compiled.
    pub cache_misses: u64,
    /// Time spent in the driver's creation calls (lookups included), summed over threads.
    pub time: core::time::Duration,
}

static CREATED: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static NANOS: AtomicU64 = AtomicU64::new(0);

/// How many pipelines this process created, how many came from the pipeline cache, and
/// how long the driver took.
pub fn creation_stats() -> PipelineCreationStats {
    PipelineCreationStats {
        created: CREATED.load(Ordering::Relaxed),
        cache_hits: HITS.load(Ordering::Relaxed),
        cache_misses: MISSES.load(Ordering::Relaxed),
        time: core::time::Duration::from_nanos(NANOS.load(Ordering::Relaxed)),
    }
}

/// Counts a creation call that started at `start`: `created` or not (a lookup that
/// missed), `lookup` or a full creation.
fn count_creation(start: std::time::Instant, created: bool, lookup: bool) {
    NANOS.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    match (created, lookup) {
        (true, true) => {
            CREATED.fetch_add(1, Ordering::Relaxed);
            HITS.fetch_add(1, Ordering::Relaxed);
        }
        (true, false) => {
            CREATED.fetch_add(1, Ordering::Relaxed);
        }
        (false, _) => {
            MISSES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// With `cached_only`, creation fails (`VK_PIPELINE_COMPILE_REQUIRED`) instead of
/// compiling when the pipeline cache does not hold the pipeline (core in Vulkan 1.3).
fn cache_flags(cached_only: bool) -> vk::PipelineCreateFlags {
    if cached_only {
        vk::PipelineCreateFlags::FAIL_ON_PIPELINE_COMPILE_REQUIRED
    } else {
        vk::PipelineCreateFlags::empty()
    }
}

/// A pipeline layout destroyed with the last of its holders: a pipeline still to be
/// created with it ([`crate::LazyPipeline`], ADR 0079) keeps it alive, so the device's
/// warm-up thread never creates one with a destroyed layout.
/// [`crate::Bindless::shared_pipeline_layout`] is the one every renderer uses.
#[derive(Clone)]
pub struct SharedPipelineLayout(Arc<LayoutOwner>);

struct LayoutOwner {
    device: Arc<ash::Device>,
    raw: vk::PipelineLayout,
}

impl Drop for LayoutOwner {
    fn drop(&mut self) {
        // SAFETY: the last holder is gone: nothing creates a pipeline with it any more,
        // and recording with it ended before (the holders' contract, as for any object).
        unsafe { self.device.destroy_pipeline_layout(self.raw, None) };
    }
}

impl core::fmt::Debug for SharedPipelineLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("SharedPipelineLayout")
            .field(&self.0.raw)
            .finish()
    }
}

impl SharedPipelineLayout {
    /// Takes over `raw`, a pipeline layout of `device`, to destroy it with the last holder.
    ///
    /// # Safety
    ///
    /// `raw` was created on `device`, and nothing else destroys it.
    pub unsafe fn new(device: &Device, raw: vk::PipelineLayout) -> Self {
        Self(Arc::new(LayoutOwner {
            device: device.pipeline_context().device,
            raw,
        }))
    }

    /// The Vulkan pipeline layout.
    pub fn raw(&self) -> vk::PipelineLayout {
        self.0.raw
    }
}

/// What creating pipelines needs from their device (ADR 0055, ADR 0079): the device, its
/// pipeline cache, whether pipelines capture the driver's statistics, and the threads that
/// create many at once ([`Device::pipeline_context`]). Cheap to clone and to send to
/// another thread, so pipelines can be created while drawing or on a warm-up thread
/// ([`crate::LazyPipeline`]). Valid while its device lives: whatever holds one is dropped
/// before the device, like every object made from it.
#[derive(Clone)]
pub struct PipelineContext {
    pub(crate) device: Arc<ash::Device>,
    pub(crate) cache: vk::PipelineCache,
    /// Reads the driver's statistics, when pipelines capture them.
    pub(crate) statistics: Option<ash::khr::pipeline_executable_properties::Device>,
    pub(crate) jobs: Option<Arc<ThreadPool>>,
}

impl core::fmt::Debug for PipelineContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PipelineContext")
            .field("cache", &self.cache)
            .field("statistics", &self.statistics.is_some())
            .field("jobs", &self.jobs.is_some())
            .finish()
    }
}

impl PipelineContext {
    /// Calls `create(i)` for every `i` in `0..count` in parallel, as
    /// [`Device::create_parallel`] does.
    pub fn create_parallel<T, F>(&self, count: usize, create: F) -> Result<Vec<T>, RhiError>
    where
        T: Send + Sync,
        F: Fn(usize) -> Result<T, RhiError> + Sync,
    {
        create_parallel(self.jobs.as_deref(), count, create)
    }

    /// Creation flags: statistics when the device captures them, and `cached_only`'s.
    fn flags(&self, cached_only: bool) -> vk::PipelineCreateFlags {
        let statistics = if self.statistics.is_some() {
            vk::PipelineCreateFlags::CAPTURE_STATISTICS_KHR
        } else {
            vk::PipelineCreateFlags::empty()
        };
        statistics | cache_flags(cached_only)
    }
}

/// Calls `create(i)` for every `i` in `0..count` in parallel and returns the results in
/// index order, or the error of the lowest index that failed: on `jobs`, or else on up to
/// one thread per core started for the call ([`Device::create_parallel`]).
pub(crate) fn create_parallel<T, F>(
    jobs: Option<&ThreadPool>,
    count: usize,
    create: F,
) -> Result<Vec<T>, RhiError>
where
    T: Send + Sync,
    F: Fn(usize) -> Result<T, RhiError> + Sync,
{
    engine_profiler::zone!("rhi::create_parallel");
    if count <= 1 {
        return (0..count).map(create).collect();
    }
    let slots: Vec<OnceLock<Result<T, RhiError>>> = (0..count).map(|_| OnceLock::new()).collect();
    let run = |i: usize| {
        let _ = slots[i].set(create(i));
    };
    match jobs {
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

/// A pipeline and the device that owns it. Destroyed on drop (the GPU must be done
/// with it).
pub struct Pipeline {
    device: Arc<ash::Device>,
    raw: vk::Pipeline,
    bind_point: vk::PipelineBindPoint,
}

impl core::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pipeline")
            .field("raw", &self.raw)
            .field("bind_point", &self.bind_point)
            .finish()
    }
}

struct Modules<'d> {
    device: &'d ash::Device,
    list: [vk::ShaderModule; 2],
}

impl Modules<'_> {
    fn create(&mut self, slot: usize, spirv: &[u32]) -> Result<vk::ShaderModule, RhiError> {
        // SAFETY: the words are a SPIR-V module (validated by the driver/layer).
        let module = unsafe {
            self.device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(spirv), None)
        }
        .map_err(RhiError::vk("vkCreateShaderModule"))?;
        self.list[slot] = module;
        Ok(module)
    }
}

impl Drop for Modules<'_> {
    fn drop(&mut self) {
        for &module in &self.list {
            if module != vk::ShaderModule::null() {
                // SAFETY: modules are only needed until pipeline creation returns.
                unsafe { self.device.destroy_shader_module(module, None) };
            }
        }
    }
}

/// `ENGINE_PIPELINE_SALT=<n>` (n > 0): every pipeline carries one more specialization
/// constant, with an id no shader uses. It changes nothing in the pipeline (the Vulkan
/// specification ignores map entries the shader does not use) but changes every cache key,
/// so the driver compiles from scratch: for measuring cold pipeline creation.
fn salt() -> Option<u32> {
    static SALT: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *SALT.get_or_init(|| {
        std::env::var("ENGINE_PIPELINE_SALT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v != 0)
    })
}

/// The constant id of the salt: far above any id a shader declares.
const SALT_CONSTANT_ID: u32 = 0x7FFF_0000;

/// Map entries and data for specialization constants: `values[i]` is constant id `i`.
fn specialization_parts(values: &[u32]) -> (Vec<vk::SpecializationMapEntry>, Vec<u8>) {
    let mut entries: Vec<_> = (0..values.len() as u32)
        .map(|i| vk::SpecializationMapEntry {
            constant_id: i,
            offset: i * 4,
            size: 4,
        })
        .collect();
    let mut data: Vec<u8> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
    if let Some(salt) = salt() {
        entries.push(vk::SpecializationMapEntry {
            constant_id: SALT_CONSTANT_ID,
            offset: data.len() as u32,
            size: 4,
        });
        data.extend_from_slice(&salt.to_ne_bytes());
    }
    (entries, data)
}

impl Pipeline {
    /// Creates a graphics pipeline with `layout` (normally
    /// [`crate::Bindless::pipeline_layout`]), through the device's pipeline cache. Many
    /// at once: [`Self::create_many`].
    pub fn graphics(
        device: &Device,
        layout: vk::PipelineLayout,
        desc: &GraphicsPipelineDesc<'_>,
    ) -> Result<Self, RhiError> {
        Self::create_graphics(&device.pipeline_context(), layout, desc, false)?.ok_or(
            RhiError::Vulkan {
                call: "vkCreateGraphicsPipelines",
                result: vk::Result::PIPELINE_COMPILE_REQUIRED,
            },
        )
    }

    /// Creates the pipelines of `descs`, in their order, as fast as the driver allows
    /// (ADR 0055):
    ///
    /// 1. those the pipeline cache holds, one after another on the calling thread: a
    ///    lookup takes a fraction of a millisecond, and drivers serialize lookups (on the
    ///    RTX 4060 Ti, 41 lookups on 16 threads took 38 ms, one after another 6 ms);
    /// 2. the others in parallel ([`Device::create_parallel`]): compiling takes tens of
    ///    milliseconds each, on the calling thread of each.
    pub fn create_many(
        device: &Device,
        layout: vk::PipelineLayout,
        descs: &[PipelineDesc<'_>],
    ) -> Result<Vec<Self>, RhiError> {
        Self::create_many_in(&device.pipeline_context(), layout, descs)
    }

    /// [`Self::create_many`] through a [`PipelineContext`].
    pub fn create_many_in(
        context: &PipelineContext,
        layout: vk::PipelineLayout,
        descs: &[PipelineDesc<'_>],
    ) -> Result<Vec<Self>, RhiError> {
        engine_profiler::zone!("rhi::Pipeline::create_many");
        let create =
            |desc: &PipelineDesc<'_>, cached_only| Self::create(context, layout, desc, cached_only);
        let mut pipelines = Vec::with_capacity(descs.len());
        for desc in descs {
            pipelines.push(create(desc, true)?);
        }
        let missing: Vec<usize> = (0..descs.len())
            .filter(|&i| pipelines[i].is_none())
            .collect();
        let compiled = context.create_parallel(missing.len(), |k| {
            create(&descs[missing[k]], false)?.ok_or(RhiError::Vulkan {
                call: "vkCreate*Pipelines",
                result: vk::Result::PIPELINE_COMPILE_REQUIRED,
            })
        })?;
        for (i, pipeline) in missing.into_iter().zip(compiled) {
            pipelines[i] = Some(pipeline);
        }
        Ok(pipelines.into_iter().flatten().collect())
    }

    /// Creates the pipeline `desc` describes; with `cached_only`, only if the pipeline
    /// cache holds it (`None` otherwise).
    pub(crate) fn create(
        context: &PipelineContext,
        layout: vk::PipelineLayout,
        desc: &PipelineDesc<'_>,
        cached_only: bool,
    ) -> Result<Option<Self>, RhiError> {
        match desc {
            PipelineDesc::Graphics(desc) => {
                Self::create_graphics(context, layout, desc, cached_only)
            }
            PipelineDesc::Compute {
                stage,
                specialization,
            } => Self::create_compute(context, layout, *stage, specialization, cached_only),
        }
    }

    /// Creates a graphics pipeline; with `cached_only`, only if the pipeline cache holds
    /// it (`None` otherwise).
    fn create_graphics(
        context: &PipelineContext,
        layout: vk::PipelineLayout,
        desc: &GraphicsPipelineDesc<'_>,
        cached_only: bool,
    ) -> Result<Option<Self>, RhiError> {
        engine_profiler::zone!("rhi::Pipeline::graphics");
        let d = &*context.device;
        let mut modules = Modules {
            device: d,
            list: [vk::ShaderModule::null(); 2],
        };
        let (entries, data) = specialization_parts(desc.specialization);
        let specialization = vk::SpecializationInfo::default()
            .map_entries(&entries)
            .data(&data);
        let stage = |flags, module, entry| {
            let info = vk::PipelineShaderStageCreateInfo::default()
                .stage(flags)
                .module(module)
                .name(entry);
            if entries.is_empty() {
                info
            } else {
                info.specialization_info(&specialization)
            }
        };
        let mut stages = Vec::with_capacity(2);
        let vertex = modules.create(0, desc.vertex.spirv)?;
        stages.push(stage(
            vk::ShaderStageFlags::VERTEX,
            vertex,
            desc.vertex.entry,
        ));
        if let Some(fragment) = desc.fragment {
            let module = modules.create(1, fragment.spirv)?;
            stages.push(stage(
                vk::ShaderStageFlags::FRAGMENT,
                module,
                fragment.entry,
            ));
        }
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(desc.topology);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(desc.viewports.max(1))
            .scissor_count(desc.viewports.max(1));
        let mut raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(desc.cull_mode)
            .front_face(desc.front_face)
            .line_width(1.0);
        if let Some(bias) = desc.depth_bias {
            raster = raster
                .depth_bias_enable(true)
                .depth_bias_constant_factor(bias.constant)
                .depth_bias_slope_factor(bias.slope)
                .depth_bias_clamp(bias.clamp);
        }
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(desc.samples)
            .alpha_to_coverage_enable(desc.alpha_to_coverage);
        let depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(desc.depth_test)
            .depth_write_enable(desc.depth_write)
            .depth_compare_op(desc.depth_compare);
        let attachment = match desc.blend {
            Blend::Opaque => vk::PipelineColorBlendAttachmentState::default(),
            Blend::Alpha => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Additive => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Premultiplied => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Multiply => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::DST_COLOR)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ZERO)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                .alpha_blend_op(vk::BlendOp::ADD),
            Blend::Half => vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::CONSTANT_COLOR)
                .dst_color_blend_factor(vk::BlendFactor::CONSTANT_COLOR)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::CONSTANT_ALPHA)
                .dst_alpha_blend_factor(vk::BlendFactor::CONSTANT_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
        }
        .color_write_mask(vk::ColorComponentFlags::RGBA);
        let attachments = vec![attachment; desc.color_formats.len()];
        let blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&attachments)
            .blend_constants(if desc.blend == Blend::Half {
                [0.5; 4]
            } else {
                [0.0; 4]
            });
        let dynamic_states = [
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
            vk::DynamicState::CULL_MODE,
            vk::DynamicState::FRONT_FACE,
        ];
        let dynamic_count = if desc.dynamic_culling { 4 } else { 2 };
        let dynamic = vk::PipelineDynamicStateCreateInfo::default()
            .dynamic_states(&dynamic_states[..dynamic_count]);
        let mut rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(desc.color_formats)
            .depth_attachment_format(desc.depth_format);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .flags(context.flags(cached_only))
            .push_next(&mut rendering);
        let start = std::time::Instant::now();
        // SAFETY: every pointer in `info` refers to locals alive for the call.
        let created = unsafe { d.create_graphics_pipelines(context.cache, &[info], None) };
        let raw = match created {
            Ok(pipelines) => pipelines[0],
            Err((_, vk::Result::PIPELINE_COMPILE_REQUIRED)) if cached_only => {
                count_creation(start, false, true);
                return Ok(None);
            }
            Err((_, result)) => {
                return Err(RhiError::Vulkan {
                    call: "vkCreateGraphicsPipelines",
                    result,
                });
            }
        };
        count_creation(start, true, cached_only);
        if let Some(reader) = &context.statistics {
            let stages: Vec<ShaderStageDesc<'_>> =
                core::iter::once(desc.vertex).chain(desc.fragment).collect();
            record(
                reader,
                raw,
                &stages,
                desc.specialization,
                record_attachments(desc),
            );
        }
        Ok(Some(Self {
            device: Arc::clone(&context.device),
            raw,
            bind_point: vk::PipelineBindPoint::GRAPHICS,
        }))
    }

    /// The driver's statistics of each stage (registers, occupancy, instruction counts;
    /// names and meanings are the driver's), or nothing unless the device captures them
    /// ([`Device::pipeline_statistics_enabled`]).
    pub fn statistics(&self, device: &Device) -> Vec<PipelineStatistic> {
        device
            .pipeline_statistics_reader()
            .map_or_else(Vec::new, |reader| read_statistics(reader, self.raw))
    }

    /// Creates a compute pipeline with `layout`, through the device's pipeline cache.
    pub fn compute(
        device: &Device,
        layout: vk::PipelineLayout,
        stage: ShaderStageDesc<'_>,
    ) -> Result<Self, RhiError> {
        Self::compute_specialized(device, layout, stage, &[])
    }

    /// Like [`Self::compute`], with specialization constants: `specialization[i]` is
    /// constant id `i` (32-bit values), as in [`GraphicsPipelineDesc::specialization`].
    pub fn compute_specialized(
        device: &Device,
        layout: vk::PipelineLayout,
        stage: ShaderStageDesc<'_>,
        specialization: &[u32],
    ) -> Result<Self, RhiError> {
        Self::create_compute(
            &device.pipeline_context(),
            layout,
            stage,
            specialization,
            false,
        )?
        .ok_or(RhiError::Vulkan {
            call: "vkCreateComputePipelines",
            result: vk::Result::PIPELINE_COMPILE_REQUIRED,
        })
    }

    /// Creates a compute pipeline; with `cached_only`, only if the pipeline cache holds
    /// it (`None` otherwise).
    fn create_compute(
        context: &PipelineContext,
        layout: vk::PipelineLayout,
        stage: ShaderStageDesc<'_>,
        specialization: &[u32],
        cached_only: bool,
    ) -> Result<Option<Self>, RhiError> {
        engine_profiler::zone!("rhi::Pipeline::compute");
        let d = &*context.device;
        let mut modules = Modules {
            device: d,
            list: [vk::ShaderModule::null(); 2],
        };
        let module = modules.create(0, stage.spirv)?;
        let constants = specialization;
        let (entries, data) = specialization_parts(specialization);
        let specialization = vk::SpecializationInfo::default()
            .map_entries(&entries)
            .data(&data);
        let mut stage_info = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(stage.entry);
        if !entries.is_empty() {
            stage_info = stage_info.specialization_info(&specialization);
        }
        let info = vk::ComputePipelineCreateInfo::default()
            .stage(stage_info)
            .layout(layout)
            .flags(context.flags(cached_only));
        let start = std::time::Instant::now();
        // SAFETY: as above.
        let created = unsafe { d.create_compute_pipelines(context.cache, &[info], None) };
        let raw = match created {
            Ok(pipelines) => pipelines[0],
            Err((_, vk::Result::PIPELINE_COMPILE_REQUIRED)) if cached_only => {
                count_creation(start, false, true);
                return Ok(None);
            }
            Err((_, result)) => {
                return Err(RhiError::Vulkan {
                    call: "vkCreateComputePipelines",
                    result,
                });
            }
        };
        count_creation(start, true, cached_only);
        if let Some(reader) = &context.statistics {
            record(reader, raw, &[stage], constants, String::new());
        }
        Ok(Some(Self {
            device: Arc::clone(&context.device),
            raw,
            bind_point: vk::PipelineBindPoint::COMPUTE,
        }))
    }

    /// The Vulkan pipeline.
    pub fn raw(&self) -> vk::Pipeline {
        self.raw
    }

    /// Binds the pipeline.
    ///
    /// # Safety
    ///
    /// `cmd` is recording and belongs to this device.
    pub unsafe fn bind(&self, cmd: vk::CommandBuffer) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.device
                .cmd_bind_pipeline(cmd, self.bind_point, self.raw)
        };
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: owned pipeline; the GPU no longer uses it (type-level contract).
        unsafe { self.device.destroy_pipeline(self.raw, None) };
    }
}

/// Reads SPIR-V from bytes (for example `include_bytes!`), checking size and magic.
pub fn spirv_words(bytes: &[u8]) -> Option<Vec<u32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    (words.first() == Some(&0x0723_0203)).then_some(words)
}
