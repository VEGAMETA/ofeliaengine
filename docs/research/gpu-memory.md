# GPU memory allocation: research

Date: 2026-09-26. Input for [ADR 0010](../adr/0010-gpu-memory-allocator.md). We read the
code for ideas only. No code was copied.

## Sources

| Source | Version checked | License | What we looked at |
|---|---|---|---|
| [VulkanMemoryAllocator](https://github.com/GPUOpen-LibrariesAndSDKs/VulkanMemoryAllocator) (VMA), `include/vk_mem_alloc.h` | 3.4.0, `master` fetched 2026-09-26 | MIT | TLSF metadata, block sizing, dedicated heuristics, granularity handling |
| [gpu-allocator](https://github.com/Traverse-Research/gpu-allocator) (Traverse Research) | 0.28.0 (crates.io) | MIT OR Apache-2.0 | free-list sub-allocator, block sizes, API; also benchmarked |
| [gpu-alloc](https://github.com/zakarumych/gpu-alloc) (used by wgpu-hal) | 0.6.0 (crates.io) | MIT OR Apache-2.0 | buddy and free-list allocators |
| [Granite](https://github.com/Themaister/Granite) `vulkan/memory_allocator.*`, `util/arena_allocator.*` | `master`, fetched 2026-09-26 | MIT | "Legion" bitmask allocator and size classes |
| Masmano, Ripoll, Crespo, Real, "TLSF: a New Dynamic Memory Allocator for Real-Time Systems", ECRTS 2004 | — | paper | the algorithm |

## How they work

### VMA 3.4 (the reference C/C++ allocator)

- **Default algorithm: TLSF** (`VmaBlockMetadata_TLSF`) with `SECOND_LEVEL_INDEX = 5`
  (32 lists per power of two) and a separate linear class for small sizes (≤ 256 B).
  Allocation and free are O(1): two bit scans over bitmaps find the first non-empty list
  that is large enough.
- **Search strategy:** the "next list" (guaranteed fit) first, then the null block, then
  larger lists, and finally the best-fit list, walking blocks that may fit depending on
  alignment.
- **Block sizes:**
  - `VMA_DEFAULT_LARGE_HEAP_BLOCK_SIZE` = 256 MiB;
  - heaps ≤ 1 GiB (`VMA_SMALL_HEAP_MAX_SIZE`) use heap size / 8;
  - new blocks start at 1/8, 1/4, 1/2 of the preferred size while smaller than the
    largest existing block (`NEW_BLOCK_SIZE_SHIFT_MAX = 3`);
  - on failure it retries at smaller sizes.
- **Dedicated allocations:**
  - when the driver requires or prefers them (`VkMemoryDedicatedRequirements`);
  - when the size is greater than half the preferred block size;
  - but no longer preferred above 3/4 of `maxMemoryAllocationCount`.
- **`bufferImageGranularity`:** a per-block page map (`VmaBlockBufferImageGranularity`)
  tracks linear/optimal resources on the same page.
- **Other features:**
  - budget tracking through `VK_EXT_memory_budget`;
  - optional buffer device address flag on blocks;
  - persistent mapping.

### gpu-allocator 0.28

- **Free list, best fit.** Chunks live in a `HashMap<u64, MemoryChunk>` plus a
  `HashSet` of free chunk ids. `allocate` iterates **all free chunks** of a block (O(free
  chunks)) and checks `bufferImageGranularity` conflicts against neighbours. Each chunk
  stores an optional `String` name and an optional backtrace.
- **Blocks:** 256 MiB for device memory by default, 64 MiB for host memory
  (`AllocationSizes`). Dedicated allocations are made on request or when the size exceeds
  the block size.
- Pros: simple, well tested, used in production (Traverse Research, Embark).
- Cons:
  - heap allocations on every allocation and free (hash map entries, the name `String`);
  - linear search;
  - pulls in `log`, `presser` and `thiserror`.

### gpu-alloc 0.6 (wgpu)

- A **buddy allocator** for small sizes and a **free-list/linear** allocator for larger
  ones. A generic `MemoryDevice` trait abstracts the API.
- Pros: bounded fragmentation in the buddy part.
- Cons:
  - buddy rounding wastes up to 50% per allocation;
  - generic over back ends we don't need.

### Granite ("Legion" allocator)

- Each heap is split into 32 sub-blocks. A 32-bit mask per run length gives O(1)
  "find N contiguous free sub-blocks" with bit tricks.
- Size classes are chained: each class's heaps are allocations from the next class up,
  with sub-block sizes growing 32× per class.
- Pros: very fast and compact.
- Cons: allocations are rounded to sub-block multiples. Waste is up to ~50% just above a
  class boundary.

## Measurements

`crates/engine_rhi/benches/memory.rs`:
- the same deterministic request stream for both allocators, on real `VkDeviceMemory`;
- N live allocations of 256 B – 1.5 MiB (log-uniform), alignment 256 B – 64 KiB;
- 50 000 × (free a random one, allocate a new one) after an identical warm-up;
- median of 5 runs.

Container (lavapipe, 4 vCPU, **not reference**), release:

| Allocator | live | ns per free+allocate | heap allocations per pair | VkDeviceMemory objects | bytes in blocks / live bytes |
|---|---|---|---|---|---|
| engine (TLSF) | 1 000 | 160 | 0 | 4 | 2.55 |
| gpu-allocator 0.28 | 1 000 | 10 484 | 4 | 4 | 1.36 |
| engine (TLSF) | 4 000 | 168 | 0 | 6 | 1.28 |
| gpu-allocator 0.28 | 4 000 | 29 071 | 4 | 15 | 1.24 |
| engine (TLSF) | 16 000 | 242 | 0 | 17 | 1.25 |
| gpu-allocator 0.28 | 16 000 | 80 435 | 4 | 60 | 1.26 |

- TLSF is 65–330× faster and stays nearly flat as the population grows. The free-list
  search grows linearly.
- Our allocator makes **zero heap allocations** in steady state (also asserted by
  `tests/memory_zero_alloc.rs`), against 4 per pair for gpu-allocator.
- The overhead at 1 000 live allocations comes from block growth: 32 + 64 + 128 + 256 MiB
  for ~200 MiB live, the same policy VMA uses. It converges to ~1.25 as the pool grows.
- **Before a fix, our first version** tried the thorough (list-walking) search in each block
  before moving to the next block: 1 906 ns at 16 000 live. Trying the O(1) path on every
  block first, and the thorough search only if all fail, brought it to 242 ns.

ref-1 (NVIDIA + AMD) numbers come from the GPU workflow and go into `docs/perf/history.md`.

## What we take

- **TLSF** (paper; VMA's default), our own implementation:
  - 256-byte granule;
  - 32 second-level lists;
  - nodes in a vector addressed by index;
  - recycled nodes, so no heap allocations in steady state.
- **VMA's block policy:**
  - 256 MiB blocks on large heaps, heap / 8 on small ones;
  - blocks growing from 1/8;
  - retry smaller on failure;
  - dedicated above half a block or when the driver asks;
  - no dedicated preference near the allocation-count limit.
- **Budget reporting** through `VK_EXT_memory_budget`.

## What we do differently

- **Separate pools for buffers and optimal images** instead of tracking
  `bufferImageGranularity` per page. It is simpler, and it can never pad. The cost is at
  most one extra partially used block per memory type.
- **Two-pass block search:** the O(1) path in every block before any list walking.
- **Every host-visible block is mapped once for its lifetime**, since a pool serves every
  location. On integrated GPUs, "device" memory is host-visible too.
- **Every buffer block is allocated with `DEVICE_ADDRESS`**: bindless and buffer device
  address are baseline (ADR 0006).
- **Memory type fallback order per location** (for example VRAM → BAR → system memory for
  GPU-only resources) instead of VMA's usage flags. There are four locations: `Device`,
  `Upload`, `Dynamic`, `Readback`.
- No defragmentation yet (VMA has it). Revisit with texture streaming.
