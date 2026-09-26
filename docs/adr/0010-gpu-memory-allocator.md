# 0010. GPU memory allocator

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/gpu-memory.md](../research/gpu-memory.md)

## Context

CLAUDE.md §6: "Memory: `gpu-allocator` or our own (compare in an ADR)." The render graph,
meshes, textures and streaming all need device memory.

Vulkan limits the number of `VkDeviceMemory` objects (often 4096), and allocating one is
slow, so resources must be sub-allocated from large blocks.

Constraints (§5):
- zero heap allocations per frame in steady state (streaming allocates and frees while
  frames run);
- no locks on hot paths;
- measurements before decisions.

## Options

1. **gpu-allocator 0.28.** Mature and widely used. But:
   - its free-list best fit is O(free chunks);
   - it makes 4 heap allocations per allocate+free (hash maps, name strings);
   - it adds `log`, `presser` and `thiserror`.
2. **VMA through bindings.** The reference implementation with the best features
   (TLSF, defragmentation, budget). But it is C++ in our build, and it adds a
   binding-generation step.
3. **Our own TLSF allocator with VMA's block policy** (chosen). Measured 65–330× faster
   than gpu-allocator, with zero heap allocations, in about 1 500 lines including tests
   and documentation.

## Measurements

Container (lavapipe, **not reference**), release, `benches/memory.rs`, same request stream
for both. N live allocations of 256 B – 1.5 MiB; 50 000 × (free one + allocate one):

| live | engine ns/pair | gpu-allocator ns/pair | engine heap allocs/pair | gpu-allocator heap allocs/pair | objects (engine / g-a) | overhead (engine / g-a) |
|---|---|---|---|---|---|---|
| 1 000 | 160 | 10 484 | 0 | 4 | 4 / 4 | 2.55 / 1.36 |
| 4 000 | 168 | 29 071 | 0 | 4 | 6 / 15 | 1.28 / 1.24 |
| 16 000 | 242 | 80 435 | 0 | 4 | 17 / 60 | 1.25 / 1.26 |

- The high overhead at 1 000 live allocations is reserved capacity from block growth
  (32 + 64 + 128 + 256 MiB), as in VMA. It is not fragmentation.
- The GPU workflow runs the benchmark on ref-1 (NVIDIA and AMD) and records the numbers in
  `docs/perf/history.md`.

Tests:
- a TLSF property test (200 000 random operations against a model);
- placement rules for discrete, integrated and protected memory types;
- upload → copy → readback, buffer device address, image pools, dedicated allocations
  and block release, all under the validation layer on lavapipe, NVIDIA and AMD;
- zero heap allocations over 20 000 free+allocate pairs after warm-up.

## Decision

`engine_rhi::Allocator`:
- **TLSF** per block:
  - 256 B granule;
  - 32 second-level lists;
  - bitmaps and bit scans;
  - nodes addressed by index and recycled.
  - The O(1) path is tried on every block before the list-walking fallback.
- **Pools** per (memory type, buffer | optimal image), so `bufferImageGranularity` never
  applies.
- **Blocks:**
  - 256 MiB on heaps over 1 GiB, heap / 8 otherwise;
  - new blocks start at 1/8 of that and double while the pool grows;
  - smaller retries on out-of-memory;
  - one empty block kept per pool.
- **Dedicated allocations** when the driver requires or prefers them, or the resource is
  larger than half a block. There is no dedicated preference above 3/4 of
  `maxMemoryAllocationCount`.
- **Locations** `Device`, `Upload`, `Dynamic`, `Readback`, each mapping to required,
  preferred and avoided property flags. Memory types are tried best first; for example,
  GPU-only resources go to VRAM → BAR → system memory. Protected, lazily allocated and AMD
  device-coherent types are never used.
- **Host-visible memory** is mapped once for its lifetime and must be host-coherent, so
  writes need no flush.
- **Buffer memory** is allocated with `DEVICE_ADDRESS`.
- **Budget:** per-heap stats, with the OS budget and usage from `VK_EXT_memory_budget`
  (enabled when present).
- **`&mut self` API** (externally synchronized). The resource layer will own it on one
  thread and receive frees through a queue, so the render thread takes no lock.

gpu-allocator stays as a **dev-dependency of the benchmark only**. It is never linked into
the engine.

## Consequences

- Allocating and freeing GPU memory is cheap enough for streaming and transient resources
  and allocation-free in steady state.
- Features not built yet:
  - **Defragmentation:** will come with texture streaming, which is where fragmentation
    matters.
  - **Priority** (`VK_EXT_memory_priority`, pageable device-local memory): later.
  - **Aliasing of transient render-graph resources:** the render graph (next) will
    allocate a block per frame budget and place resources by lifetime. It uses
    `Allocator::allocate` with raw requirements.
- `Allocator` must be dropped before the `Device`. It asserts in debug builds that nothing
  leaked.
