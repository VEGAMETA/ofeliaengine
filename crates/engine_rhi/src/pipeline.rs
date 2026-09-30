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
use core::sync::atomic::{AtomicU64, Ordering};

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
    /// Specialization constants of every stage: value `i` is `constant_id` `i`. The driver
    /// compiles the pipeline with them, so branches on them cost nothing (and code behind
    /// a false one takes no registers).
    pub specialization: &'a [u32],
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
            specialization: &[],
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

/// Pipelines capture statistics when the device asks for them.
fn statistics_flags(device: &Device) -> vk::PipelineCreateFlags {
    if device.pipeline_statistics_enabled() {
        vk::PipelineCreateFlags::CAPTURE_STATISTICS_KHR
    } else {
        vk::PipelineCreateFlags::empty()
    }
}

/// A pipeline and the device that owns it. Destroyed on drop (the GPU must be done
/// with it).
pub struct Pipeline {
    device: ash::Device,
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
        Self::create_graphics(device, layout, desc, false)?.ok_or(RhiError::Vulkan {
            call: "vkCreateGraphicsPipelines",
            result: vk::Result::PIPELINE_COMPILE_REQUIRED,
        })
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
        engine_profiler::zone!("rhi::Pipeline::create_many");
        let create = |desc: &PipelineDesc<'_>, cached_only| match desc {
            PipelineDesc::Graphics(desc) => {
                Self::create_graphics(device, layout, desc, cached_only)
            }
            PipelineDesc::Compute {
                stage,
                specialization,
            } => Self::create_compute(device, layout, *stage, specialization, cached_only),
        };
        let mut pipelines = Vec::with_capacity(descs.len());
        for desc in descs {
            pipelines.push(create(desc, true)?);
        }
        let missing: Vec<usize> = (0..descs.len())
            .filter(|&i| pipelines[i].is_none())
            .collect();
        let compiled = device.create_parallel(missing.len(), |k| {
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

    /// Creates a graphics pipeline; with `cached_only`, only if the pipeline cache holds
    /// it (`None` otherwise).
    fn create_graphics(
        device: &Device,
        layout: vk::PipelineLayout,
        desc: &GraphicsPipelineDesc<'_>,
        cached_only: bool,
    ) -> Result<Option<Self>, RhiError> {
        engine_profiler::zone!("rhi::Pipeline::graphics");
        let d = device.raw();
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
            .viewport_count(1)
            .scissor_count(1);
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
        let multisample =
            vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(desc.samples);
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
        }
        .color_write_mask(vk::ColorComponentFlags::RGBA);
        let attachments = vec![attachment; desc.color_formats.len()];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
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
            .flags(statistics_flags(device) | cache_flags(cached_only))
            .push_next(&mut rendering);
        let start = std::time::Instant::now();
        // SAFETY: every pointer in `info` refers to locals alive for the call.
        let created =
            unsafe { d.create_graphics_pipelines(device.pipeline_cache(), &[info], None) };
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
        Ok(Some(Self {
            device: d.clone(),
            raw,
            bind_point: vk::PipelineBindPoint::GRAPHICS,
        }))
    }

    /// The driver's statistics of each stage (registers, occupancy, instruction counts;
    /// names and meanings are the driver's), or nothing unless the device captures them
    /// ([`Device::pipeline_statistics_enabled`]).
    pub fn statistics(&self, device: &Device) -> Vec<PipelineStatistic> {
        let mut out = Vec::new();
        if !device.pipeline_statistics_enabled() {
            return out;
        }
        let loader = ash::khr::pipeline_executable_properties::Device::new(
            device.instance_raw(),
            device.raw(),
        );
        let info = vk::PipelineInfoKHR::default().pipeline(self.raw);
        // SAFETY: the pipeline was created with CAPTURE_STATISTICS by this device, and the
        // extension is enabled.
        let Ok(executables) = (unsafe { loader.get_pipeline_executable_properties(&info) }) else {
            return out;
        };
        for (index, executable) in executables.iter().enumerate() {
            let stage = executable
                .name_as_c_str()
                .map_or_else(|_| format!("{index}"), |n| n.to_string_lossy().into_owned());
            let info = vk::PipelineExecutableInfoKHR::default()
                .pipeline(self.raw)
                .executable_index(index as u32);
            // SAFETY: as above; the index comes from the driver.
            let Ok(statistics) = (unsafe { loader.get_pipeline_executable_statistics(&info) })
            else {
                continue;
            };
            for statistic in statistics {
                // SAFETY: the union member matches the format the driver reports.
                let value = unsafe {
                    match statistic.format {
                        vk::PipelineExecutableStatisticFormatKHR::BOOL32 => {
                            f64::from(statistic.value.b32)
                        }
                        vk::PipelineExecutableStatisticFormatKHR::INT64 => {
                            statistic.value.i64 as f64
                        }
                        vk::PipelineExecutableStatisticFormatKHR::UINT64 => {
                            statistic.value.u64 as f64
                        }
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
        Self::create_compute(device, layout, stage, specialization, false)?.ok_or(
            RhiError::Vulkan {
                call: "vkCreateComputePipelines",
                result: vk::Result::PIPELINE_COMPILE_REQUIRED,
            },
        )
    }

    /// Creates a compute pipeline; with `cached_only`, only if the pipeline cache holds
    /// it (`None` otherwise).
    fn create_compute(
        device: &Device,
        layout: vk::PipelineLayout,
        stage: ShaderStageDesc<'_>,
        specialization: &[u32],
        cached_only: bool,
    ) -> Result<Option<Self>, RhiError> {
        engine_profiler::zone!("rhi::Pipeline::compute");
        let d = device.raw();
        let mut modules = Modules {
            device: d,
            list: [vk::ShaderModule::null(); 2],
        };
        let module = modules.create(0, stage.spirv)?;
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
            .flags(statistics_flags(device) | cache_flags(cached_only));
        let start = std::time::Instant::now();
        // SAFETY: as above.
        let created = unsafe { d.create_compute_pipelines(device.pipeline_cache(), &[info], None) };
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
        Ok(Some(Self {
            device: d.clone(),
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
