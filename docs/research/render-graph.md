# Render graph: research

Date: 2026-09-26. Input for [ADR 0011](../adr/0011-render-graph.md). We read the code for
ideas only. No code was copied.

## Sources

| Source | Version checked | License | What we looked at |
|---|---|---|---|
| Y. O'Donnell, "FrameGraph: Extensible Rendering Architecture in Frostbite", GDC 2017 | talk | — | per-frame setup / compile / execute, culling by reference counts, transient resources with memory aliasing |
| [Granite](https://github.com/Themaister/Granite) `renderer/render_graph.{hpp,cpp}`, H.-K. Arntzen, "Render graphs and Vulkan — a deep dive" (blog, 2017) | `master`, fetched 2026-09-26 | MIT | baked graph, pass reordering, physical resources, barriers, aliasing |
| [Filament](https://github.com/google/filament) `filament/src/fg/FrameGraph.{h,cpp}` | `main`, fetched 2026-09-26 | Apache-2.0 | dependency graph, culling, devirtualization at first use, usage resolution |
| [kajiya](https://github.com/EmbarkStudios/kajiya) `kajiya-rg/src/graph.rs`, `kajiya-backend/src/transient_resource_cache.rs` | `main`, fetched 2026-09-26 | MIT / Apache-2.0 | per-frame Rust graph, access types, transient cache |
| Unreal Engine RDG (documentation), G. Wihlidal, "Halcyon Architecture" (SEED, 2018) | products/talks | — | transient allocator with aliasing, barrier batching, async compute |
| Khronos, synchronization validation (Vulkan SDK) | SDK 1.4.x | — | a tool that proves the barriers correct in tests |

## How they work

### Frostbite FrameGraph

- **Three phases every frame:**
  - **setup:** passes declare their resources and accesses;
  - **compile:**
    - culls with reference counts (flood fill from the outputs);
    - computes lifetimes;
    - allocates transient resources with memory aliasing;
  - **execute:** runs pass lambdas.
- The main arguments of the talk:
  - declaring a frame is cheap enough to do every frame, with no caching of a baked
    graph;
  - aliasing saves a large fraction of render-target memory;
  - async compute is expressed in the same graph.

### Granite

- The graph is **baked** once, and rebaked when the setup changes:
  - traverse dependencies from the backbuffer (culling);
  - **reorder passes** to increase the distance between dependent passes (better
    pipelining);
  - merge passes into render-pass subpasses;
  - build physical resources (resources that share dimensions alias via renaming);
  - build barriers per physical pass.
- Pros: very little per-frame work after baking, and subpass merging helps tilers.
- Cons:
  - changing the frame (a toggled effect, a new viewport) means rebaking;
  - the design is tied to render passes, which dynamic rendering makes unnecessary on
    desktop.

### Filament FrameGraph

- A dependency graph of pass nodes and resource nodes. `cull()` uses reference counts.
- Resources are **devirtualized** (created) at their first user and destroyed after
  their last.
- **Usage flags are resolved from the edges**: a texture gets exactly the usage its
  readers and writers need.
- The backend caches textures across frames. There is no memory aliasing on Vulkan.

### kajiya

- A per-frame Rust graph. Each resource access is a `vk_sync::AccessType`, a single
  enum that encodes stage, access and layout.
- A barrier is issued just before each pass, per resource (not batched).
- A transient resource cache keyed by description (`HashMap<ImageDesc, Vec<Image>>`),
  without memory aliasing.
- No culling.

### Unreal RDG and Halcyon

- **RDG:**
  - per-frame graph with culling;
  - a **transient resource allocator** that places resources in heaps by lifetime;
  - batched barriers;
  - async compute with fork/join points;
  - pass merging.
- **Halcyon:**
  - the same ideas;
  - "render graph as the only way to issue GPU work";
  - automatic barriers are a correctness feature as much as a performance one.

## Measurements

`crates/engine_render/benches/graph.rs`: declare + compile + record one frame in steady
state, with empty pass callbacks. Container (lavapipe, **not reference**), release:

| passes | µs per frame | µs per pass | transient memory, aliased / unaliased |
|---|---|---|---|
| 16 | 6.2 | 0.38 | 1.0 / 4.3 MiB |
| 64 | 27.3 | 0.43 | 1.0 / 17.8 MiB |
| 256 | 112.4 | 0.44 | 1.0 / 71.8 MiB |

The first version cost 0.8–2.0 µs per pass and was superlinear (501 µs at 256 passes). A
phase breakdown at 256 passes showed that 59% of the time went into placement, the O(n²)
aliasing predecessor lists and a linear search of the object cache. Three changes made it
linear:

- caching the placement while the frame's shape repeats;
- keeping only the latest occupants that cover a resource's memory as its predecessors
  (older ones are ordered through the barrier chain);
- a cursor for cache lookups.

Other results:

- **Zero heap allocations** per frame over 100 steady-state frames with 13 passes
  (`tests/graph_zero_alloc.rs`).
- **Synchronization validation reports no hazard** for a graph with clears, copies,
  attachment load after transfer, buffer fills, culling and aliasing, over 6 frames on
  2 slots (`tests/graph.rs`). The same test runs on NVIDIA and AMD on ref-1.

## What we take

- **Frostbite's per-frame setup / compile / execute:**
  - no baking, so any frame shape changes freely (viewports, toggles);
  - culling from imported resources and passes with side effects.
- **Filament's usage resolution:** image and buffer usage come from the declared
  accesses.
- **Unreal's and Frostbite's memory aliasing:**
  - transient resources placed by lifetime in a heap per frame in flight (from our TLSF
    allocator, ADR 0010);
  - an aliasing barrier from the previous occupants at first use.
- **kajiya-style access types**, but sync2-native: each access maps to stages, access
  mask, layout and usage.
- **Batched barriers per pass:** image barriers plus one global memory barrier for
  buffers.
- **Synchronization validation** in tests, as the proof of correctness.

## What we don't (yet)

- **Granite's pass reordering.** Declaration order is the execution order; the author
  controls pipelining. Revisit with async compute.
- **Subpass merging:** we use dynamic rendering (ADR 0006).
- **Async compute:** the API will get a queue per pass. Execution is single-queue for now.
- **GPU timestamps per pass:** these come with Tracy GPU zones and the viewport profiler.
