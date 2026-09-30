//! GPU memory allocation (ADR 0010).
//!
//! Vulkan limits the number of `VkDeviceMemory` objects (often 4096) and allocating one is
//! slow, so resources are sub-allocated from large blocks:
//!
//! - **Pools** per memory type and resource kind. Buffers and optimal-tiling images
//!   never share a block, so `bufferImageGranularity` never forces padding.
//! - **Blocks** of 256 MiB on large heaps (heap / 8 on heaps up to 1 GiB), starting at
//!   1/8 of that and doubling as the pool grows, so small scenes stay small. Each block's
//!   free space is managed by an O(1) [TLSF](tlsf) allocator. Host-visible blocks stay
//!   mapped for their whole life. Buffer blocks are allocated with `DEVICE_ADDRESS`, so
//!   every buffer can use buffer device address (bindless, ADR 0006).
//! - **Dedicated allocations** when the driver asks for them or a resource is larger
//!   than half a block (large render targets, big streaming textures).
//! - An empty block is released unless it is the pool's only empty block, which avoids
//!   allocate/free thrashing at a boundary.
//!
//! [`Allocator`] is externally synchronized (`&mut self`): the resource layer above
//! decides which thread owns it.

pub(crate) mod tlsf;

use crate::device::Device;
use crate::error::RhiError;
use ash::vk;
use core::ptr::NonNull;
use tlsf::{Tlsf, TlsfAllocation};

/// An offsets-only range allocator (the O(1) TLSF behind GPU memory blocks) for
/// sub-allocating inside one resource: the vertices and indices of a geometry pool, the
/// slots of a large buffer. Units are whatever the caller counts in; sizes round up to the
/// granule. Allocating and freeing touch no heap once the internal node list has grown to
/// the working set.
#[derive(Debug)]
pub struct RangeAllocator(Tlsf);

/// A range handed out by [`RangeAllocator::allocate`]. Free it with the allocator that
/// made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range(TlsfAllocation);

impl Range {
    /// First unit.
    pub fn offset(&self) -> u64 {
        self.0.offset
    }

    /// Units reserved (the request rounded up to the granule).
    pub fn size(&self) -> u64 {
        self.0.size
    }
}

impl RangeAllocator {
    /// Manages `[0, capacity)` in `granule`s (a power of two; 1 for exact sizes), up to
    /// 2^32 granules.
    pub fn new(capacity: u64, granule: u64) -> Self {
        Self(Tlsf::with_granule(capacity, granule))
    }

    /// Reserves `size` units at an offset aligned to `align` (a power of two). `None` when
    /// no free range fits: [`Self::grow`] and try again.
    pub fn allocate(&mut self, size: u64, align: u64) -> Option<Range> {
        self.0
            .allocate_fast(size, align)
            .or_else(|| self.0.allocate(size, align))
            .map(Range)
    }

    /// Returns a range to the allocator.
    pub fn free(&mut self, range: Range) {
        self.0.free(range.0);
    }

    /// Extends the managed range to `[0, capacity)`; existing ranges keep their offsets.
    pub fn grow(&mut self, capacity: u64) {
        self.0.grow(capacity);
    }

    /// Size of the managed range.
    pub fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    /// Units in live ranges.
    pub fn used(&self) -> u64 {
        self.0.used()
    }

    /// Number of live ranges.
    pub fn len(&self) -> u32 {
        self.0.allocation_count()
    }

    /// Whether no range is live.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Where memory lives and how the CPU uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryLocation {
    /// Only the GPU accesses it (textures, meshes, render targets). Never mapped.
    Device,
    /// Staging for uploads: the CPU writes it once, the GPU copies from it. Host memory,
    /// so it does not use up the small device-local, host-visible heap (BAR).
    Upload,
    /// Written by the CPU every frame and read directly by the GPU (constants, dynamic
    /// geometry). Device-local and host-visible (BAR, resizable BAR, integrated GPUs)
    /// when available, otherwise host memory.
    Dynamic,
    /// Written by the GPU and read by the CPU (screenshots, queries). Host-cached when
    /// available, for fast CPU reads.
    Readback,
    /// Staging for streamed uploads: written by drive reads and by decompression, which
    /// reads back what it wrote, then copied by the GPU. Host-cached host memory:
    /// decompressing into write-combined memory (`Upload`) runs 25-40× slower (0.03
    /// against 1.27 GB/s per thread, ADR 0057), while the GPU copies from either at the
    /// same speed.
    Streaming,
}

impl MemoryLocation {
    /// `(required, preferred, avoided)` property flags.
    fn flags(
        self,
    ) -> (
        vk::MemoryPropertyFlags,
        vk::MemoryPropertyFlags,
        vk::MemoryPropertyFlags,
    ) {
        use vk::MemoryPropertyFlags as F;
        let host = F::HOST_VISIBLE | F::HOST_COHERENT;
        match self {
            // Video memory first, then BAR, then system memory when video memory is full.
            Self::Device => (F::empty(), F::DEVICE_LOCAL, F::HOST_VISIBLE),
            Self::Upload => (host, F::empty(), F::DEVICE_LOCAL | F::HOST_CACHED),
            Self::Dynamic => (host, F::DEVICE_LOCAL, F::HOST_CACHED),
            Self::Readback => (host, F::HOST_CACHED, F::empty()),
            Self::Streaming => (host, F::HOST_CACHED, F::DEVICE_LOCAL),
        }
    }
}

/// What the memory is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    /// A buffer or a linear-tiling image.
    Buffer,
    /// An optimal-tiling image.
    Image,
}

/// The resource a dedicated allocation belongs to (`VkMemoryDedicatedAllocateInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedicatedTo {
    /// A buffer.
    Buffer(vk::Buffer),
    /// An image.
    Image(vk::Image),
}

/// A request for [`Allocator::allocate`]. [`Allocator::create_buffer`] and
/// [`Allocator::create_image`] fill it in from the driver's requirements.
#[derive(Debug, Clone, Copy)]
pub struct AllocationRequest {
    /// Size, alignment and allowed memory types.
    pub requirements: vk::MemoryRequirements,
    /// Where the memory should live.
    pub location: MemoryLocation,
    /// What it will be bound to.
    pub kind: ResourceKind,
    /// `VkMemoryDedicatedRequirements::prefersDedicatedAllocation`.
    pub prefers_dedicated: bool,
    /// `VkMemoryDedicatedRequirements::requiresDedicatedAllocation`.
    pub requires_dedicated: bool,
    /// The resource, passed to the driver if the allocation ends up dedicated.
    pub dedicated_to: Option<DedicatedTo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Block {
        pool: u32,
        block: u32,
        range: TlsfAllocation,
    },
    Dedicated,
}

/// A piece of device memory. Return it with [`Allocator::free`] once the GPU no longer
/// uses it; dropping it without freeing leaks the range until the allocator is dropped.
#[derive(Debug)]
pub struct Allocation {
    memory: vk::DeviceMemory,
    offset: u64,
    size: u64,
    memory_type: u32,
    mapped: Option<NonNull<u8>>,
    place: Place,
}

// SAFETY: the mapped pointer refers to device memory mapped for the allocator's whole
// life; moving the handle to another thread does not change what it points to.
unsafe impl Send for Allocation {}

impl Allocation {
    /// The `VkDeviceMemory` to bind.
    pub fn memory(&self) -> vk::DeviceMemory {
        self.memory
    }

    /// Offset within [`Self::memory`].
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Requested size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Memory type index.
    pub fn memory_type(&self) -> u32 {
        self.memory_type
    }

    /// Whether the allocation has its own `VkDeviceMemory`.
    pub fn is_dedicated(&self) -> bool {
        self.place == Place::Dedicated
    }

    /// CPU pointer to the start of the allocation; always present for the host-visible
    /// locations. The memory is host-coherent: writes need no flush.
    pub fn mapped_ptr(&self) -> Option<NonNull<u8>> {
        self.mapped
    }

    /// The mapped bytes, for host-visible locations.
    ///
    /// The GPU must not access the range while the slice is used; synchronize with
    /// fences or timeline semaphores as for any shared memory.
    pub fn mapped_slice_mut(&mut self) -> Option<&mut [u8]> {
        let ptr = self.mapped?;
        // SAFETY: the pointer covers `size` mapped bytes owned by this allocation, which
        // stays mapped while it lives; `&mut self` makes the slice unique on the CPU.
        Some(unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), self.size as usize) })
    }
}

struct Block {
    memory: vk::DeviceMemory,
    tlsf: Tlsf,
    mapped: Option<NonNull<u8>>,
}

struct Pool {
    memory_type: u32,
    blocks: Vec<Option<Block>>,
}

impl Pool {
    fn empty_blocks(&self) -> usize {
        self.blocks
            .iter()
            .flatten()
            .filter(|b| b.tlsf.is_empty())
            .count()
    }

    fn largest_block(&self) -> u64 {
        self.blocks
            .iter()
            .flatten()
            .map(|b| b.tlsf.capacity())
            .max()
            .unwrap_or(0)
    }
}

/// Usage of one memory heap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes in blocks.
    pub block_bytes: u64,
    /// Blocks.
    pub blocks: u32,
    /// Bytes allocated from blocks.
    pub allocated_bytes: u64,
    /// Allocations in blocks.
    pub allocations: u32,
    /// Bytes in dedicated allocations.
    pub dedicated_bytes: u64,
    /// Dedicated allocations.
    pub dedicated_allocations: u32,
    /// The OS budget for this process (`VK_EXT_memory_budget`), when available.
    pub budget: Option<u64>,
    /// The process's usage as the OS sees it (all allocators), when available.
    pub usage: Option<u64>,
}

/// Block sizes and thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorConfig {
    /// Block size on heaps larger than 1 GiB. Default 256 MiB.
    pub large_heap_block: u64,
    /// Heaps up to this size use blocks of heap / 8. Default 1 GiB.
    pub small_heap_limit: u64,
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self {
            large_heap_block: 256 << 20,
            small_heap_limit: 1 << 30,
        }
    }
}

/// The device memory allocator. Drop it before the [`Device`].
pub struct Allocator {
    device: ash::Device,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    properties: vk::PhysicalDeviceMemoryProperties,
    /// Index: `memory_type * 2 + kind`.
    pools: Vec<Pool>,
    preferred_block: Vec<u64>,
    dedicated: Vec<(u64, u32)>,
    device_memory_count: u32,
    max_device_memory_count: u32,
    budget_available: bool,
}

// SAFETY: the mapped block pointers are only dereferenced through allocations; the
// allocator itself is used by one thread at a time (`&mut self`).
unsafe impl Send for Allocator {}

impl core::fmt::Debug for Allocator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Allocator")
            .field("device_memory_count", &self.device_memory_count)
            .finish()
    }
}

const EXCLUDED: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::from_raw(
    vk::MemoryPropertyFlags::PROTECTED.as_raw()
        | vk::MemoryPropertyFlags::LAZILY_ALLOCATED.as_raw()
        | vk::MemoryPropertyFlags::DEVICE_COHERENT_AMD.as_raw()
        | vk::MemoryPropertyFlags::DEVICE_UNCACHED_AMD.as_raw(),
);

/// Memory types allowed by `type_bits` that suit `location`, best first. A fixed array
/// (Vulkan has at most 32 memory types), so allocating touches no heap.
pub(crate) fn memory_type_candidates(
    properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    location: MemoryLocation,
) -> Candidates {
    let (required, preferred, avoided) = location.flags();
    let score = |flags: vk::MemoryPropertyFlags| {
        (flags & preferred).as_raw().count_ones() as i32 * 2
            - (flags & avoided).as_raw().count_ones() as i32
    };
    let mut list = Candidates {
        types: [(0, 0); vk::MAX_MEMORY_TYPES],
        len: 0,
    };
    let count = (properties.memory_type_count as usize).min(vk::MAX_MEMORY_TYPES);
    for (i, t) in properties.memory_types[..count].iter().enumerate() {
        if type_bits & (1 << i) != 0
            && t.property_flags.contains(required)
            && !t.property_flags.intersects(EXCLUDED)
        {
            list.types[list.len] = (-score(t.property_flags), i as u32);
            list.len += 1;
        }
    }
    list.types[..list.len].sort_unstable();
    list
}

/// Result of [`memory_type_candidates`].
pub(crate) struct Candidates {
    /// `(negated score, memory type)`, sorted.
    types: [(i32, u32); vk::MAX_MEMORY_TYPES],
    len: usize,
}

impl Candidates {
    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.types[..self.len].iter().map(|&(_, t)| t)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Allocator {
    /// An allocator for `device` with the default configuration.
    pub fn new(device: &Device) -> Self {
        Self::with_config(device, AllocatorConfig::default())
    }

    /// An allocator for `device`.
    pub fn with_config(device: &Device, config: AllocatorConfig) -> Self {
        let adapter = device.adapter();
        let properties = adapter.memory;
        let preferred_block = properties.memory_heaps[..properties.memory_heap_count as usize]
            .iter()
            .map(|heap| {
                if heap.size <= config.small_heap_limit {
                    (heap.size / 8).next_multiple_of(tlsf::GRANULE)
                } else {
                    config.large_heap_block
                }
            })
            .collect();
        let mut pools = Vec::with_capacity(properties.memory_type_count as usize * 2);
        for memory_type in 0..properties.memory_type_count {
            // One pool per resource kind.
            for _ in 0..2 {
                pools.push(Pool {
                    memory_type,
                    blocks: Vec::new(),
                });
            }
        }
        Self {
            device: device.raw().clone(),
            instance: device.instance_raw().clone(),
            physical: device.physical(),
            properties,
            pools,
            preferred_block,
            dedicated: vec![(0, 0); properties.memory_heap_count as usize],
            device_memory_count: 0,
            max_device_memory_count: adapter.limits.max_memory_allocation_count,
            budget_available: adapter.caps.memory_budget,
        }
    }

    fn heap_of(&self, memory_type: u32) -> usize {
        self.properties.memory_types[memory_type as usize].heap_index as usize
    }

    /// Allocates memory for `request`. Tries each suitable memory type, best first
    /// (for example host memory when device memory is exhausted).
    pub fn allocate(&mut self, request: &AllocationRequest) -> Result<Allocation, RhiError> {
        let candidates = memory_type_candidates(
            &self.properties,
            request.requirements.memory_type_bits,
            request.location,
        );
        if candidates.is_empty() {
            return Err(RhiError::NoMemoryType {
                type_bits: request.requirements.memory_type_bits,
                location: request.location,
            });
        }
        let mut last_error = vk::Result::ERROR_OUT_OF_DEVICE_MEMORY;
        for memory_type in candidates.iter() {
            match self.allocate_from_type(request, memory_type) {
                Ok(allocation) => return Ok(allocation),
                Err(
                    e @ (vk::Result::ERROR_OUT_OF_DEVICE_MEMORY
                    | vk::Result::ERROR_OUT_OF_HOST_MEMORY),
                ) => {
                    last_error = e;
                }
                Err(e) => return Err(RhiError::vk("vkAllocateMemory")(e)),
            }
        }
        Err(RhiError::vk("vkAllocateMemory")(last_error))
    }

    fn allocate_from_type(
        &mut self,
        request: &AllocationRequest,
        memory_type: u32,
    ) -> Result<Allocation, vk::Result> {
        let size = request.requirements.size;
        let align = request.requirements.alignment.max(1);
        let preferred = self.preferred_block[self.heap_of(memory_type)];
        let near_limit = self.device_memory_count >= self.max_device_memory_count / 4 * 3;
        let dedicated = request.requires_dedicated
            || (!near_limit && (request.prefers_dedicated || size > preferred / 2));
        if dedicated {
            return self.allocate_dedicated(request, memory_type);
        }

        let pool_index = memory_type as usize * 2 + request.kind as usize;
        // Existing blocks first: the O(1) path in every block, then the thorough search
        // (which walks free lists) only if no block had a guaranteed fit.
        for thorough in [false, true] {
            for (block_index, slot) in self.pools[pool_index].blocks.iter_mut().enumerate() {
                let Some(block) = slot else { continue };
                let range = if thorough {
                    block.tlsf.allocate(size, align)
                } else {
                    block.tlsf.allocate_fast(size, align)
                };
                if let Some(range) = range {
                    return Ok(Self::block_allocation(
                        block,
                        pool_index,
                        block_index,
                        range,
                        size,
                        memory_type,
                    ));
                }
            }
        }

        // A new block: 1/8, 1/4, 1/2 of the preferred size while the pool is small.
        let largest = self.pools[pool_index].largest_block();
        let mut block_size = preferred;
        for _ in 0..3 {
            let half = block_size / 2;
            if half > largest && half >= size.saturating_mul(2) {
                block_size = half;
            } else {
                break;
            }
        }
        let block_size = block_size.max(size.next_multiple_of(tlsf::GRANULE));
        let mut attempt = block_size;
        let block = loop {
            match self.create_block(memory_type, request.kind, attempt) {
                Ok(block) => break block,
                // The heap may still hold a smaller block.
                Err(
                    vk::Result::ERROR_OUT_OF_DEVICE_MEMORY | vk::Result::ERROR_OUT_OF_HOST_MEMORY,
                ) if attempt / 2 >= size && attempt / 2 >= tlsf::GRANULE => {
                    attempt /= 2;
                }
                Err(e) => return Err(e),
            }
        };
        let pool = &mut self.pools[pool_index];
        let block_index = match pool.blocks.iter().position(Option::is_none) {
            Some(free) => free,
            None => {
                pool.blocks.push(None);
                pool.blocks.len() - 1
            }
        };
        let block = pool.blocks[block_index].insert(block);
        let range = block
            .tlsf
            .allocate(size, align)
            .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
        Ok(Self::block_allocation(
            block,
            pool_index,
            block_index,
            range,
            size,
            memory_type,
        ))
    }

    fn block_allocation(
        block: &Block,
        pool: usize,
        block_index: usize,
        range: TlsfAllocation,
        size: u64,
        memory_type: u32,
    ) -> Allocation {
        Allocation {
            memory: block.memory,
            offset: range.offset,
            size,
            memory_type,
            // SAFETY: `range` lies inside the block, which is mapped as a whole.
            mapped: block
                .mapped
                .map(|base| unsafe { base.add(range.offset as usize) }),
            place: Place::Block {
                pool: pool as u32,
                block: block_index as u32,
                range,
            },
        }
    }

    /// Allocates a `VkDeviceMemory`; host-visible memory is mapped for its whole life
    /// (a pool serves every location, so a block made for a GPU-only resource on an
    /// integrated GPU may later serve an upload).
    fn raw_allocate(
        &mut self,
        size: u64,
        memory_type: u32,
        kind: ResourceKind,
        dedicated_to: Option<DedicatedTo>,
    ) -> Result<(vk::DeviceMemory, Option<NonNull<u8>>), vk::Result> {
        let map = self.properties.memory_types[memory_type as usize]
            .property_flags
            .contains(vk::MemoryPropertyFlags::HOST_VISIBLE);
        engine_profiler::zone!("rhi::vkAllocateMemory");
        let mut flags_info =
            vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default();
        match dedicated_to {
            Some(DedicatedTo::Buffer(buffer)) => dedicated_info = dedicated_info.buffer(buffer),
            Some(DedicatedTo::Image(image)) => dedicated_info = dedicated_info.image(image),
            None => {}
        }
        let mut info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(memory_type);
        if kind == ResourceKind::Buffer {
            info = info.push_next(&mut flags_info);
        }
        if dedicated_to.is_some() {
            info = info.push_next(&mut dedicated_info);
        }
        // SAFETY: valid allocate info; the chained structs outlive the call.
        let memory = unsafe { self.device.allocate_memory(&info, None) }?;
        let mapped = if map {
            // SAFETY: host-visible memory, mapped once for its whole life.
            match unsafe {
                self.device
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            } {
                Ok(ptr) => NonNull::new(ptr.cast::<u8>()),
                Err(e) => {
                    // SAFETY: just allocated, not in use.
                    unsafe { self.device.free_memory(memory, None) };
                    return Err(e);
                }
            }
        } else {
            None
        };
        self.device_memory_count += 1;
        Ok((memory, mapped))
    }

    fn create_block(
        &mut self,
        memory_type: u32,
        kind: ResourceKind,
        size: u64,
    ) -> Result<Block, vk::Result> {
        let (memory, mapped) = self.raw_allocate(size, memory_type, kind, None)?;
        engine_profiler::plot!("gpu memory blocks", self.device_memory_count);
        Ok(Block {
            memory,
            tlsf: Tlsf::new(size),
            mapped,
        })
    }

    fn allocate_dedicated(
        &mut self,
        request: &AllocationRequest,
        memory_type: u32,
    ) -> Result<Allocation, vk::Result> {
        let size = request.requirements.size;
        let (memory, mapped) =
            self.raw_allocate(size, memory_type, request.kind, request.dedicated_to)?;
        let heap = self.heap_of(memory_type);
        self.dedicated[heap].0 += size;
        self.dedicated[heap].1 += 1;
        Ok(Allocation {
            memory,
            offset: 0,
            size,
            memory_type,
            mapped,
            place: Place::Dedicated,
        })
    }

    /// Returns `allocation`. The GPU must no longer use it.
    pub fn free(&mut self, allocation: Allocation) {
        match allocation.place {
            Place::Dedicated => {
                let heap = self.heap_of(allocation.memory_type);
                self.dedicated[heap].0 -= allocation.size;
                self.dedicated[heap].1 -= 1;
                self.device_memory_count -= 1;
                // SAFETY: the allocation owns this memory; freeing also unmaps it.
                unsafe { self.device.free_memory(allocation.memory, None) };
            }
            Place::Block { pool, block, range } => {
                let pool = &mut self.pools[pool as usize];
                let Some(Some(b)) = pool.blocks.get_mut(block as usize) else {
                    debug_assert!(false, "allocation from an unknown block");
                    return;
                };
                b.tlsf.free(range);
                // Keep one empty block per pool as a buffer against thrashing.
                if b.tlsf.is_empty()
                    && pool.empty_blocks() > 1
                    && let Some(b) = pool.blocks[block as usize].take()
                {
                    self.device_memory_count -= 1;
                    // SAFETY: the block is empty: nothing is bound to it any more.
                    unsafe { self.device.free_memory(b.memory, None) };
                    engine_profiler::plot!("gpu memory blocks", self.device_memory_count);
                }
            }
        }
    }

    /// Creates a buffer and binds memory to it.
    pub fn create_buffer(
        &mut self,
        info: &vk::BufferCreateInfo<'_>,
        location: MemoryLocation,
    ) -> Result<(vk::Buffer, Allocation), RhiError> {
        // SAFETY: valid create info.
        let buffer = unsafe { self.device.create_buffer(info, None) }
            .map_err(RhiError::vk("vkCreateBuffer"))?;
        let mut dedicated = vk::MemoryDedicatedRequirements::default();
        let mut requirements = vk::MemoryRequirements2::default().push_next(&mut dedicated);
        // SAFETY: `buffer` was just created on this device.
        unsafe {
            self.device.get_buffer_memory_requirements2(
                &vk::BufferMemoryRequirementsInfo2::default().buffer(buffer),
                &mut requirements,
            );
        }
        let request = AllocationRequest {
            requirements: requirements.memory_requirements,
            location,
            kind: ResourceKind::Buffer,
            prefers_dedicated: dedicated.prefers_dedicated_allocation == vk::TRUE,
            requires_dedicated: dedicated.requires_dedicated_allocation == vk::TRUE,
            dedicated_to: Some(DedicatedTo::Buffer(buffer)),
        };
        let bound = self.allocate(&request).and_then(|allocation| {
            // SAFETY: fresh buffer, memory of a type its requirements allow, aligned offset.
            match unsafe {
                self.device
                    .bind_buffer_memory(buffer, allocation.memory, allocation.offset)
            } {
                Ok(()) => Ok(allocation),
                Err(e) => {
                    self.free(allocation);
                    Err(RhiError::vk("vkBindBufferMemory")(e))
                }
            }
        });
        match bound {
            Ok(allocation) => Ok((buffer, allocation)),
            Err(e) => {
                // SAFETY: never used.
                unsafe { self.device.destroy_buffer(buffer, None) };
                Err(e)
            }
        }
    }

    /// Creates an image and binds memory to it. Linear-tiling images share blocks with
    /// buffers.
    pub fn create_image(
        &mut self,
        info: &vk::ImageCreateInfo<'_>,
        location: MemoryLocation,
    ) -> Result<(vk::Image, Allocation), RhiError> {
        // SAFETY: valid create info.
        let image = unsafe { self.device.create_image(info, None) }
            .map_err(RhiError::vk("vkCreateImage"))?;
        let mut dedicated = vk::MemoryDedicatedRequirements::default();
        let mut requirements = vk::MemoryRequirements2::default().push_next(&mut dedicated);
        // SAFETY: `image` was just created on this device.
        unsafe {
            self.device.get_image_memory_requirements2(
                &vk::ImageMemoryRequirementsInfo2::default().image(image),
                &mut requirements,
            );
        }
        let kind = if info.tiling == vk::ImageTiling::LINEAR {
            ResourceKind::Buffer
        } else {
            ResourceKind::Image
        };
        let request = AllocationRequest {
            requirements: requirements.memory_requirements,
            location,
            kind,
            prefers_dedicated: dedicated.prefers_dedicated_allocation == vk::TRUE,
            requires_dedicated: dedicated.requires_dedicated_allocation == vk::TRUE,
            dedicated_to: Some(DedicatedTo::Image(image)),
        };
        let bound = self.allocate(&request).and_then(|allocation| {
            // SAFETY: fresh image, memory of a type its requirements allow, aligned offset.
            match unsafe {
                self.device
                    .bind_image_memory(image, allocation.memory, allocation.offset)
            } {
                Ok(()) => Ok(allocation),
                Err(e) => {
                    self.free(allocation);
                    Err(RhiError::vk("vkBindImageMemory")(e))
                }
            }
        });
        match bound {
            Ok(allocation) => Ok((image, allocation)),
            Err(e) => {
                // SAFETY: never used.
                unsafe { self.device.destroy_image(image, None) };
                Err(e)
            }
        }
    }

    /// Destroys a buffer made by [`Self::create_buffer`] and frees its memory.
    ///
    /// # Safety
    ///
    /// `buffer` and `allocation` come from the same `create_buffer` call on this
    /// allocator, and the GPU no longer uses the buffer.
    pub unsafe fn destroy_buffer(&mut self, buffer: vk::Buffer, allocation: Allocation) {
        // SAFETY: guaranteed by the caller.
        unsafe { self.device.destroy_buffer(buffer, None) };
        self.free(allocation);
    }

    /// Destroys an image made by [`Self::create_image`] and frees its memory.
    ///
    /// # Safety
    ///
    /// `image` and `allocation` come from the same `create_image` call on this
    /// allocator, and the GPU no longer uses the image.
    pub unsafe fn destroy_image(&mut self, image: vk::Image, allocation: Allocation) {
        // SAFETY: guaranteed by the caller.
        unsafe { self.device.destroy_image(image, None) };
        self.free(allocation);
    }

    /// Number of `VkDeviceMemory` objects (blocks and dedicated allocations).
    pub fn device_memory_count(&self) -> u32 {
        self.device_memory_count
    }

    /// Usage per memory heap, with the OS budget when `VK_EXT_memory_budget` is enabled.
    pub fn heap_stats(&self) -> Vec<HeapStats> {
        let heaps = self.properties.memory_heap_count as usize;
        let mut stats = vec![HeapStats::default(); heaps];
        for pool in &self.pools {
            let heap = self.heap_of(pool.memory_type);
            for block in pool.blocks.iter().flatten() {
                let s = &mut stats[heap];
                s.block_bytes += block.tlsf.capacity();
                s.blocks += 1;
                s.allocated_bytes += block.tlsf.used();
                s.allocations += block.tlsf.allocation_count();
            }
        }
        for (s, &(bytes, count)) in stats.iter_mut().zip(&self.dedicated) {
            s.dedicated_bytes = bytes;
            s.dedicated_allocations = count;
        }
        if self.budget_available {
            let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
            let mut properties =
                vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
            // SAFETY: the extension is enabled on the device; the chain is valid.
            unsafe {
                self.instance
                    .get_physical_device_memory_properties2(self.physical, &mut properties);
            }
            for (i, s) in stats.iter_mut().enumerate() {
                s.budget = Some(budget.heap_budget[i]);
                s.usage = Some(budget.heap_usage[i]);
            }
        }
        stats
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        let live: u32 = self
            .pools
            .iter()
            .flat_map(|p| p.blocks.iter().flatten())
            .map(|b| b.tlsf.allocation_count())
            .sum::<u32>()
            + self.dedicated.iter().map(|d| d.1).sum::<u32>();
        if !std::thread::panicking() {
            debug_assert_eq!(live, 0, "GPU allocations leaked");
        }
        for pool in &mut self.pools {
            for block in pool.blocks.drain(..).flatten() {
                // SAFETY: the allocator is being dropped; blocks are no longer used.
                unsafe { self.device.free_memory(block.memory, None) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk::MemoryPropertyFlags as F;

    fn properties(types: &[F], heaps: &[u64]) -> vk::PhysicalDeviceMemoryProperties {
        let mut p = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: types.len() as u32,
            memory_heap_count: heaps.len() as u32,
            ..Default::default()
        };
        for (i, &flags) in types.iter().enumerate() {
            p.memory_types[i] = vk::MemoryType {
                property_flags: flags,
                heap_index: u32::from(!flags.contains(F::DEVICE_LOCAL)),
            };
        }
        for (i, &size) in heaps.iter().enumerate() {
            p.memory_heaps[i].size = size;
        }
        p
    }

    fn pick(p: &vk::PhysicalDeviceMemoryProperties, location: MemoryLocation) -> Vec<u32> {
        memory_type_candidates(p, u32::MAX, location)
            .iter()
            .collect()
    }

    #[test]
    fn discrete_gpu_placement() {
        // Typical discrete layout: VRAM, system memory (coherent, cached) and the BAR.
        let p = properties(
            &[
                F::DEVICE_LOCAL,
                F::HOST_VISIBLE | F::HOST_COHERENT,
                F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED,
                F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT,
            ],
            &[16 << 30, 32 << 30],
        );
        // VRAM, then the BAR, then system memory as the last resort.
        assert_eq!(pick(&p, MemoryLocation::Device), [0, 3, 1, 2]);
        // Staging avoids the BAR and cached memory.
        assert_eq!(pick(&p, MemoryLocation::Upload)[0], 1);
        // Per-frame data goes to the BAR first.
        assert_eq!(pick(&p, MemoryLocation::Dynamic), [3, 1, 2]);
        // Readback prefers cached memory.
        assert_eq!(pick(&p, MemoryLocation::Readback)[0], 2);
        // A resource that only allows type 1 still gets it for any host location.
        assert_eq!(
            memory_type_candidates(&p, 0b0010, MemoryLocation::Upload)
                .iter()
                .collect::<Vec<_>>(),
            [1]
        );
    }

    #[test]
    fn integrated_gpu_and_exclusions() {
        // UMA: everything device-local; one type is protected and must never be used.
        let p = properties(
            &[
                F::DEVICE_LOCAL,
                F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT,
                F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED,
                F::DEVICE_LOCAL | F::PROTECTED,
            ],
            &[8 << 30],
        );
        assert_eq!(pick(&p, MemoryLocation::Device), [0, 1, 2]);
        assert_eq!(pick(&p, MemoryLocation::Dynamic), [1, 2]);
        assert_eq!(pick(&p, MemoryLocation::Readback)[0], 2);
        // Host locations need HOST_VISIBLE: nothing when only type 0 is allowed.
        assert!(memory_type_candidates(&p, 0b0001, MemoryLocation::Upload).is_empty());
    }
}
