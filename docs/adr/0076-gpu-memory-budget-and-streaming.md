# 0076. A GPU memory budget, texture streaming by GPU feedback, mesh detail levels streamed

- Status: Accepted
- Date: 2026-10-02
- Research: [docs/research/streaming-budget.md](../research/streaming-budget.md)
- Builds on: [ADR 0010](0010-gpu-memory-allocator.md) (the allocator),
  [ADR 0040](0040-materials-and-textures.md) (the texture pool, materials),
  [ADR 0053](0053-mesh-lods.md) (detail levels chosen by the culling pass),
  [ADR 0056](0056-cooked-packs-and-streaming-reads.md) (packs, the I/O ring),
  [ADR 0057](0057-texture-streaming.md) (texture streaming step 1),
  [ADR 0064](0064-gpu-scene-extraction-runtime.md), [ADR 0072](0072-frame-renderer.md)

## Context

ADR 0057 streams a pack's texture levels smallest first, but every level of every texture,
and it left two steps: **which levels to load** (what the GPU samples) and **a memory
budget** that evicts what is not needed. Meshes were loaded whole. Nothing knew how much GPU
memory the process may use: the OS budget was readable (`heap_stats`) but nobody asked.

The team asked for one budget owner every streaming system asks, priorities (visible now,
near, far; required levels never evicted), texture levels from GPU feedback within the
register rule, mesh detail levels streamed by the same budget, graceful degradation and no
hitches.

## Options

### Who owns the budget

1. **One owner in `engine_render` (chosen, `budget::GpuBudget`):** reads
   `VK_EXT_memory_budget` through `Allocator::budget` (allocation-free; every 8 frames,
   estimated in between from the allocator's own bytes, as VMA does), takes the game's
   share of the device-local heaps' budget (90 % on a discrete GPU, 50 % on an integrated
   one, optional cap, or a fixed pool), subtracts what the process uses outside the
   streaming clients, and hands the rest out **class by class**: every client's required
   bytes, then visible demand (scaled together when it does not fit), then cached; what
   nobody asked for is split evenly so new demand plans the same frame.
2. A pool size per system (UE's `r.Streaming.PoolSize`, Unity's budget): simple, but each
   system guesses, and none sees the OS budget shrink.
3. Allocation failure as the signal: too late (the OS demotes before it fails; hitches).

### Which texture levels: measured or estimated

1. **GPU feedback only (chosen):** a pass reports, per material slot, the finest level
   sampled (in texture-coordinate units: one value serves all of a material's textures,
   which share the mesh's coordinates) and its pixel count. Exact for what is drawn,
   whatever the tiling, angle, anisotropy or upscaler bias.
2. Screen-size estimates (UE's texel factors, Unity's UV distribution metric): work before
   anything is drawn and can prefetch, but are estimates needing per-asset correction.
3. **Both, combined** (what the brief suggested): **dropped for patents** — the Graphine
   family (US10748241, US11195250, US11893656, EP3216011) claims a plurality of predictor
   units with different schemes whose predicted sets a streaming manager combines into a
   working set. With feedback as the only source of wanted levels we stay outside it;
   distance and pixels only order work. Question for the team below.

### Where feedback is written: the register rule

1. **A separate low-resolution pass (chosen, `feedback::TextureFeedback`):** after the
   view's forward passes, the same culled draw commands (both occlusion phases, their
   detail levels) drawn again into a 1/8-per-side depth target with `depthVertexMasked`
   and `feedbackFragment` (8 VGPRs); opaque surfaces first with depth writes, transparent
   ones after without; the level computed as the 16× anisotropic sampler does, plus the
   materials' LOD bias, minus log2 of the divisor; one atomic min and add per wave. Each
   pass draws one of four slices of the commands (rotating). Readback ring; results a
   frame-in-flight later.
2. In the forward shading: no extra vertex work, but every shading pipeline gains code and
   registers (or doubles its variants); views that do not stream would pay. Rejected by
   the register rule (CLAUDE.md §5).
3. Per-instance screen-size heuristics in the culling pass: cheap, but an estimate (above).

### Residency changes

1. **Images sized to their resident levels, reallocated by copy (chosen):**
   `TexturePool::add_streamed` makes an image of the levels from a first one down;
   `set_base` makes an image of another size and copies the shared uploaded levels on the
   GPU (grow: room for finer levels; shrink: eviction). Copies share the per-frame byte
   budget with uploads.
2. Sparse residency: no copies, but per-driver tile limits and the mip tail; not needed at
   the measured cost (copies are part of the ≤ 2–4 MiB a frame of texture traffic below).
3. Full images with a view clamp (ADR 0057's state): saves time, not memory.

### Mesh detail levels

1. **Levels as vertex prefixes (chosen):** the cook orders a mesh's vertices by first use
   from the coarsest level (meshoptimizer's advice for shared-vertex LOD chains), so each
   level uses a prefix. Pack version 3 stores the coarsest level in the mesh entry and each
   finer level (its new vertices and indices) in a mesh level entry. The geometry pool
   grows or shrinks a mesh by copying its prefix into a new range on the GPU; levels not
   resident repeat the finest resident level's indices, so the **culling pass is
   unchanged**.
2. Independent vertex arrays per level: no copies, but up to +88 % vertex memory when all
   levels are resident.
3. **Which levels:** the culling pass's own rule computed on the CPU (`cull::choose_lod`,
   shared with the renderer's `lod_scale`): the rule is a deterministic function of box,
   transform and camera, so the CPU knows exactly what the GPU will pick; no feedback
   needed. Instances outside the frustum do not ask.

### Degrading under pressure and avoiding hitches

- Visible textures at their wanted level; when that does not fit, a **common bias** (all one
  level coarser, then two) and a level back for the textures covering most pixels while room
  remains. Levels beyond the plan stay **cached** (most recently seen first) until room is
  needed. Meshes: the same with their levels.
- **Hysteresis:** a wanted level gets coarser only after 30 frames without the finer one;
  levels given back go first to textures already holding them; a level to read must fit with
  a 1/32 margin. Before these, a Sponza flight in a 24 MiB pool read 1,005 MiB; after,
  141 MiB.
- **No hitches:** reads and decompression on the streaming thread (ADR 0057), mesh levels
  decoded there too (`StreamThread::spawn_with`'s post-process); per frame at most the
  texture pool's byte budget of copies and uploads and 64k mesh vertices staged.

## Numbers

ref-1, release, `VK_LOADER_LAYERS_DISABLE=~implicit~`; RTX 4060 Ti / Radeon iGPU.
`stream_bench` (`engine_streaming`): Sponza cooked into a pack (version 3), 1920 × 1080,
a camera flying down the nave and back over 900 frames paced at 60 fps, statistics after
the first 10 frames. The machine was shared with another agent's benchmarks; Radeon runs
repeated on a quiet machine where they differed.

**Budget accounting** (`tests/memory.rs`): 768 MiB of images allocated moves the OS usage by
+992 MiB and the allocator's count by +992 MiB on both GPUs. Device-local budget seen:
RTX 14.98 GiB; Radeon 21.24 GiB (the driver exposes most of system memory as device-local,
hence the 50 % share).

**Feedback** (register rule): no existing SPIR-V changed; Radeon forward variants stay at
48 / 64 / 64 / 80 VGPRs (`scene_bench --sky --pipeline-stats`), `feedbackFragment` uses 8.
Accuracy (`tests/feedback.rs`): within 0.004 levels of the analytic level; the sampler reads
the level asked for and the next on both GPUs; a hidden quad reports nothing.

| feedback pass, µs | RTX | Radeon |
|---|---:|---:|
| perf gate `sponza_streamed` (static view down the nave, pass + clear + readback) | 14.4 | 151–165 |
| flight, median (unpaced) | 8.4 | 49 |
| all commands every pass (1 slice), flight | 15 | 204 |

The streamed frame against `sponza` in the perf gate: RTX GPU frame 762.6 / 738.5 µs,
Radeon 17,342 / 17,156 µs: the feedback pass, nothing else (forward 403 / 402 and
12,714 / 12,685 µs: the same levels resident).

**Sponza flight**, textures 91 MiB with every level:

| | RTX | Radeon |
|---|---:|---:|
| textures resident, ample budget | 84.0 MiB | 83.9 MiB |
| meshes resident: start → end of flight | 1.7 → 9.1 MiB | 1.7 → 9.1 MiB |
| request → uploaded, mean / worst | 337–416 / 1,150–1,450 ms | 467–503 / 1,612–1,659 ms |
| streaming on the frame's thread, median / worst | 0.04 / 0.6 ms | 0.05 / 0.5 ms |
| texture traffic per frame, p99 / worst | 2.0 / 3.3 MiB | 2.0–2.3 / 4.0 MiB |
| mesh levels on the frame's thread | 9–14 ns per vertex, ≤ 0.2 ms a frame | 15–20 ns, ≤ 0.35 ms |
| GPU frame worst / median, streaming / no feedback (quiet, paced) | power-state bound¹ | 1.50–1.55 / 1.50–1.62 |
| 24 MiB pool: peak resident, read in the flight | 26.3 MiB², 99–161 MiB | 26.3–27.0 MiB², 93–147 MiB |

¹ Paced at 60 fps the RTX drops its clocks between frames: 146 of 890 frames take over twice
the median with streaming off too. Unpaced, worst / median is 4.45 with feedback and 4.83
without (0.3 ms frames).
² Over the grant transiently: an image replaced by a smaller one lives until the frames that
used it finish.

**Stress scene** (`stream_bench --synthetic 48,2048`: 48 quads, each its own 2048² BC7
texture, 256 MiB): 128 MiB pool, peak 132 MiB; request → uploaded mean 15–19 ms; upload
pass for 5.3 MiB p99 417 µs (RTX, unpaced) / 573–607 µs (Radeon); worst GPU frame / median
1.59 (Radeon), 0 frames over twice the median.

**Tests:** Sponza in a 12 MiB pool (`engine_streaming/tests/budget.rs`, validation on):
settles in 80–109 frames at bias 2 within the grant, recovers to 42–45 MiB of every
sampled level at 4 GiB, no frame over the 8 MiB frame budget, no validation errors.
Golden images: Sponza streamed with 4 GiB equals the `sponza` golden image (mean FLIP
0.00001 / 0.00002); `sponza_streamed_tight` (3 MiB) is a new shot, bit-identical across
runs. Settled streaming frames allocate nothing (`tests/zero_alloc.rs`). Pack: Sponza
361 → 526 entries, 71.5 → 72.7 MB.

## Decision

- **`engine_rhi`:** `Allocator::budget()` → `MemoryBudget` (per heap: size, OS budget,
  OS usage, the allocator's bytes; `device_local()`), `device_local_bytes()`.
- **`engine_render::budget::GpuBudget`:** `BudgetSettings { share, cap, streaming_limit,
  query_interval }` (`for_adapter`), `register`, `report(resident, Demand { required,
  visible, cached })`, `update(&allocator)`, `grant`, `stats`.
- **`engine_render::feedback::TextureFeedback`** (`FeedbackSettings { divisor 8,
  interval 1, slices 4 }`): `add_pass` after a view's forward passes (the frame renderer's
  hook gets `SceneTargets::passes`), `poll`, `results` (per material slot: level, pixels).
  `ForwardPasses` carries the draw lists for it.
- **`TexturePool`:** `add_streamed`, `set_base`, `info`, `memory`, `set_frame_budget`,
  `pending_moves`, `last_frame_bytes`.
- **`GeometryPool`:** `add_streamed_mesh`, `upload_mesh_level(_encoded)`,
  `evict_mesh_levels`, `mesh_residency`, `streamed_bytes`.
- **`engine_render::streaming`:** `TextureStreamer` (feedback → wanted levels, plan,
  evictions, `LevelRequest`s), `MeshStreamer` (`observe` instances from a `LodView`,
  `MeshLevelRequest`s), both clients of the budget (`StreamerSettings`: visible 60 frames,
  hold 30, 32 MiB of requests a frame).
- **`engine_assets`:** pack version 3 with mesh level entries (`order_for_streaming`,
  `streamed_vertex_counts`, `open_scene_streamed`, `read_mesh_level_entry`);
  `StreamThread::spawn_with` (a post-process on the streaming thread).
- **`engine_streaming` (new crate, the "home for the glue" ADR 0057 asked for):**
  `SceneStreamer` opens a pack, creates textures with their tails and meshes with their
  coarsest levels, and each frame folds feedback in, observes the scene's instances,
  plans both streamers, starts reads into the staging ring or reused buffers and uploads
  what arrived. `stream_bench` measures it.
- The golden test bed streams a shot with `Shot::texture_budget`; the perf gate has
  `sponza_streamed`.

## Consequences

- A scene shows at once (tails and coarsest meshes, 3.3 MiB for Sponza) and keeps what its
  views sample, within the OS budget; under pressure every texture loses its finest level
  together instead of some missing; streaming costs ~0.05 ms a frame on the frame's thread
  and 10–165 µs of GPU for feedback.
- Views that do not stream pay nothing: no shader or pipeline of theirs changed.
- Not done, in order of expected value:
  - memory priorities (`VK_EXT_pageable_device_local_memory`): blocks per priority class,
    streamed textures lowest, for when another process takes memory faster than we evict;
  - feedback for several views (one `TextureFeedback` per view; the streamer folds each);
    shadow-only and reflection-probe views do not report;
  - screen-size prefetch (behind the camera, ahead of a fast camera), pending the patent
    question;
  - LOD cross-fades (ADR 0053's follow-up) and impostors;
  - the GPU scene's instances observed by the mesh streamer without a CPU loop over them
    (today `SceneStreamer` walks the scene's instances: 103 for Sponza);
  - sparse residency if copies ever dominate the frame budget.
- Packs must be cooked again for mesh streaming; older packs load with whole meshes.

## Questions for the team

- **Patents:** the Graphine "prediction system for texture streaming" family (US10748241,
  US11195250, US11893656, EP3216011, active) claims combining several prediction schemes'
  sets of sub-regions into a working set. We use one source (GPU feedback) to stay clear
  and dropped the screen-size combination the brief asked for. Is that reading acceptable,
  or should counsel look at it before prefetching is added?
  **Answered (owner, 2026-10-10):** the reading is acceptable; what the engine has (one
  source per kind of object, a budget that only divides bytes) does not fall under the
  family. Combining several schemes' sets for the same objects stays out (claims read in
  `docs/research/streaming-fast-turns.md`).
- Default budget share on integrated GPUs (50 % of a heap the driver makes mostly of system
  memory): fine as a default, or should games pick it per platform?

## Addendum (2026-10-16): the allocator's free room is the pool's (core audit 3, §3.3)

In the flight, the RTX's device-local blocks grew from 320 to 576 MiB while their
allocations grew from 230 to 248 MiB. The OS counts all of it (787 MiB). The blocks show
where the free room is (the flight's report now lists them):

| | the RTX, end of the flight | the Radeon |
|---|---|---|
| buffers, MiB used of each block | 20 of 32, 3 of 64 | 20 of 32, 3 of 64, 64 of 64 |
| images, MiB used of each block | 31 of 32, 59 of 64, 126 of 128, **9 of 256** | 127 of 128, **59 of 256** |

No block was empty. The image pool grows as VMA's pools do: a new block is twice the
largest, from 1/8 of the preferred 256 MiB. A few megabytes of textures past three full
blocks took a fourth block of 256 MiB. The budget counted its free room as the rest of the
process's usage, so the streaming pool lost it. Streamed data could have filled that room
without any more of the OS's memory.

- **`HeapBudget::unused`:** the bytes of the allocator's blocks no allocation holds
  (`Allocator::device_local_unused_bytes` between queries).
- **The rest of the process** is now `usage - unused - resident`, and the pool grows by
  the free room. As VMA's documentation puts it, allocations take the free space of
  existing blocks before they need new memory. Fragmented free space counts too. A texture
  that fits no hole takes a new block, which the next reading sees.
- **Of two empty blocks the larger is released.** Before, the block that became empty went
  and the other stayed, which could be the larger one. The larger can be 256 MiB, so a pool
  now keeps its smaller empty block against thrashing.
- **Measured** (the flight, end):
  - the rest of the process: RTX 360 MiB, before 688 (the blocks' 328 MiB of free room
    counted as the rest's);
  - the Radeon 174 MiB, before 445.

  The streaming pool grows by as much. Neither GPU is short of memory in the flight, so
  what streams does not change. Under a tight budget (a 4 GiB card, a low setting), the
  room would have gone unused.
- **Tests:**
  - two blocks of a pool (32 and 64 MiB): the budget's unused bytes are their free bytes,
    and freeing everything keeps the smaller block (both GPUs);
  - the budget's pool counts the free room (a unit test).
- **Not changed:** the blocks' growth (VMA's), so the OS still sees the reserved block. A
  slower growth (half the pool rather than all of it) would halve the reserve and double
  the new blocks a growing pool makes, each a `vkAllocateMemory` while streaming (733 µs in
  the flight). It waits for a game short of video memory.
