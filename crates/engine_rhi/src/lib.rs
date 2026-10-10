//! Thin Vulkan 1.3 layer (CLAUDE.md §6): not a cross-API abstraction, just the engine's
//! way of creating and owning Vulkan objects.
//!
//! - [`Instance`]: loads the Vulkan loader, creates the instance, and optionally enables
//!   the validation layer with a messenger that counts errors (tests assert zero).
//! - [`Adapter`]: a physical device with its properties, the required-feature check
//!   ([`Adapter::missing_features`]) and optional capabilities.
//! - [`Device`]: the logical device with the engine's required feature set and its
//!   queues (graphics, async compute, transfer).
//! - [`Surface`], [`Swapchain`]: presentation (Win32 windows, or headless for tests).
//! - [`Frames`]: frames in flight on one timeline semaphore.
//! - [`Allocator`]: device memory, sub-allocated from blocks with an O(1) TLSF
//!   allocator (ADR 0010).
//! - [`Bindless`]: one descriptor set with every texture and sampler, and the pipeline
//!   layout all pipelines share (ADR 0013).
//! - [`Pipeline`]: graphics pipelines with dynamic rendering and vertex pulling, and
//!   compute pipelines (ADR 0013).
//! - GPU crash diagnostics (ADR 0061): [`Breadcrumbs`] mark render graph passes in the
//!   command stream, and a device loss or hang produces a [`GpuFaultReport`] with
//!   `VK_EXT_device_fault` data ([`last_gpu_fault`]).
//!
//! The minimum GPU tier is Vulkan 1.3 with dynamic rendering, synchronization2, timeline
//! semaphores, buffer device address and bindless descriptor indexing (ADR 0006).

mod adapter;
mod bindless;
mod breadcrumbs;
mod device;
mod error;
mod fault;
mod frames;
mod instance;
mod lazy_pipeline;
mod memory;
mod modules;
mod pipeline;
mod pipeline_cache;
pub mod spirv;
mod swapchain;

pub use adapter::{Adapter, AdapterKind, AdapterPreference, OptionalCaps};
pub use ash;
pub use bindless::{Bindless, PUSH_CONSTANT_BYTES, SamplerKind, Wrap};
pub use breadcrumbs::{Breadcrumbs, MAX_PASSES, MAX_TRACKS, Track};
pub use device::{Device, LatencyExtensions, Queue};
pub use error::RhiError;
pub use fault::{AddressFault, DeviceFault, GpuFaultReport, VendorFault, last_gpu_fault};
pub use frames::{Frame, Frames};
pub use instance::{Instance, InstanceDesc, ValidationReport};
pub use lazy_pipeline::LazyPipeline;
pub use memory::{
    Allocation, AllocationRequest, Allocator, AllocatorConfig, DedicatedTo, HeapBudget, HeapStats,
    MemoryBudget, MemoryLocation, Range, RangeAllocator, ResourceKind,
};
pub use pipeline::{
    Blend, DepthBias, GraphicsPipelineDesc, Pipeline, PipelineContext, PipelineCreationStats,
    PipelineDesc, PipelineStatistic, ShaderStageDesc, SharedPipelineLayout, creation_stats,
    spirv_words,
};
pub use pipeline_cache::{PipelineCacheLoad, PipelineCacheSave};
pub use swapchain::{
    AcquiredImage, HdrMetadata, OutputColor, PresentMode, Surface, Swapchain, SwapchainDesc,
    color_range,
};
