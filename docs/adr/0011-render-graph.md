# 0011. Render graph

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/render-graph.md](../research/render-graph.md)

## Context

CLAUDE.md §6 asks for a render graph with:
- automatic barriers;
- transient resource aliasing;
- async compute;
- culling of unused passes.

Viewports are graph nodes whose cost is visible separately, and every pass has a debug
label.

§5 adds:
- zero heap allocations per frame;
- no locks on the render thread;
- measurements for every decision.

The engine has Vulkan 1.3 with dynamic rendering, synchronization2 and maintenance4
(ADR 0006), and the TLSF memory allocator (ADR 0010).

## Options

1. **A baked graph (Granite).** Very little per-frame work, but any change to the frame
   needs a rebake. Viewports appearing and disappearing, and effects toggled per view,
   make that frequent.
2. **A per-frame graph (Frostbite, Unreal RDG, Filament, kajiya)** (chosen). The frame is
   declared, compiled and executed every frame. This is flexible, but it must be cheap and
   allocation-free, which we measured.
3. **No graph, hand-written barriers.** Rejected: CLAUDE.md requires automatic barriers,
   and hand-written sync does not scale.

## Decision

`engine_render::graph` implements a per-frame graph.

- **Declare.** A frame declares:
  - transient textures and buffers (`create_*`);
  - imported resources with initial and final states (`import_*`);
  - passes, in execution order. Each pass lists its attachments (`color`, `depth`,
    `depth_read_only` with load/store operations) and other accesses (`Access::Sampled`,
    `StorageRead/Write`, `Transfer*`, `Uniform`, `VertexBuffer`, `IndexBuffer`,
    `Indirect`), plus a callback.
- **Compile.** Compilation is pure logic, unit-tested without a GPU.
  - Culling walks backwards from imported resources and passes marked with side effects.
    A write that discards the old contents (a cleared or not-loaded attachment) ends the
    need for earlier writers.
  - Lifetimes are the first and last needed pass. Usage flags are the union of accesses.
  - Barriers are derived per resource from synchronization2 state. The state tracks:
    - the last write's stages and access;
    - the reads since that write;
    - which stages the write is already visible to;
    - the layout.
  - The resulting barriers cover:
    - RAW, WAR and WAW hazards;
    - layout transitions (discarding when possible);
    - an aliasing barrier from the previous occupants of a transient's memory;
    - final transitions of imports (for example to `PRESENT_SRC`).
  - Barriers are batched per pass: image barriers plus one global memory barrier for
    buffers.
- **Memory.**
  - Transient resources are grouped by kind and memory types, then placed greedily
    (largest first, lowest free offset over their lifetime) in a heap. There is one heap
    set per frame in flight, allocated from the TLSF allocator with 25% headroom.
  - Placement and predecessor lists are cached while the frame's shape repeats.
  - Images, views and buffers are cached per slot by (description, usage, heap,
    offset), so a steady state creates nothing. Unused objects are destroyed when their
    slot comes around again.
  - Requirements come from `vkGetDevice*MemoryRequirements` without creating objects.
- **Execute.**
  - Each pass is wrapped in a debug label.
  - Batched barriers go before each pass.
  - `vkCmdBeginRendering` is set up from the attachments.
  - The pass callback receives a `PassContext` (command buffer, device, and handle →
    image, view, buffer, address).
  - Final transitions go last.
- **Allocation-free:**
  - pass callbacks live in a frame arena (`engine_core::arena`) and are dropped in place
    after execution, or if the frame is abandoned;
  - all bookkeeping is in vectors cleared and reused every frame.
- **Proof token:** `RenderGraph::begin` takes the `engine_rhi::Frame` from
  `Frames::begin`. It proves the slot's previous GPU work is done, so transient objects
  may be reused or destroyed. `begin_slot` is the `unsafe` variant for tests and tools.
- **Sync validation:** `InstanceDesc::sync_validation` enables Khronos synchronization
  validation. The graph's GPU test runs with it and must report zero hazards.
- **GPU time per pass:**
  - Every needed pass is bracketed by two timestamps in a per-slot query pool.
  - When the slot comes around again, results are read without waiting.
  - `RenderGraph::gpu_pass_times` returns them (for the editor profiler's per-viewport
    view).
  - In profiling builds they become Tracy GPU zones (`engine_profiler::GpuContext`,
    calibrated once with a timestamp submission).

## Measurements

Container (lavapipe, **not reference**), release, `benches/graph.rs`: declare + compile +
record, steady state, empty callbacks.

| passes | µs per frame | µs per pass | transient memory, aliased / unaliased |
|---|---|---|---|
| 16 | 6.2 | 0.38 | 1.0 / 4.3 MiB |
| 64 | 27.3 | 0.43 | 1.0 / 17.8 MiB |
| 256 | 112.4 | 0.44 | 1.0 / 71.8 MiB |

- The first version was superlinear: 501 µs at 256 passes. Profiling the phases led to
  the placement cache, predecessor pruning and the cache cursor (details in the research
  note).
- `tests/graph_zero_alloc.rs`: 0 heap allocations over 100 steady-state frames of a
  13-pass deferred-style frame.
- `tests/graph.rs`: correct pixels and buffer contents over 6 frames on 2 slots. Culled
  passes never run, aliased memory is shared, objects are reused, and synchronization
  validation reports 0 errors.
- ref-1 (GPU workflow on commit 7c43e1e):
  - synchronization validation reports 0 errors on the RTX 4060 Ti and the Radeon iGPU;
  - 0 heap allocations per frame;
  - GPU times per pass are 1–12 µs.

  | passes | RTX 4060 Ti µs / frame (per pass) | Radeon iGPU µs / frame (per pass) |
  |---|---|---|
  | 16 | 13.5 (0.84) | 41.6 (2.60) |
  | 64 | 54.3 (0.85) | 139.8 (2.18) |
  | 256 | 216.2 (0.84) | 544.6 (2.13) |

  Real drivers dominate the recording cost (barriers, `vkCmdBeginRendering`). The AMD
  driver costs about 2.5× the NVIDIA one per pass.

## Consequences

- Renderers and viewports declare passes and never write barriers. A viewport becomes a
  set of passes in the frame's graph.
- Next steps, in this order:
  1. **Async compute:** a queue per pass and cross-queue timeline semaphores. Pass
     reordering can come with it. Done in [ADR 0065](0065-async-compute.md).
  2. Periodic Tracy clock resync (AMD GPUs reset their timers in low-power states) and
     calibrated timestamps (`VK_KHR_calibrated_timestamps`) instead of the one-off
     submission.
  3. Mips and layers per subresource in barriers. Today whole images transition
     together.
  4. Reads of imported resources that other queues or frames write.
- Pass names must be `&'static str` (no allocation). Names longer than 63 bytes are cut
  in debug labels.

## Addendum (2026-10-10): a pass's GPU time is its own

The second core review (`docs/research/core-audit-2.md`) found pass times that held other
passes' work: on the Radeon the texture feedback pass read 2.4 ms after the tone mapper
into the swapchain, and 49 µs after an offscreen one. Each pass is opened by a timestamp at
the top of the pipe, which the GPU writes as soon as it reaches the command: when the
previous pass is still running (no barrier between them, or one that only blocks later
stages), the span to the pass's closing timestamp holds that pass's tail. Measured on
ref-1 with the new test `a_pass_after_a_long_one_is_timed_alone` (a 256 MiB fill, then a
256-byte fill into another buffer, nothing between them):

| | long fill | short fill, its span | short fill, its own time |
|---|---:|---:|---:|
| RTX 4060 Ti | 929–983 µs | 1.5–2.0 µs | 1.5–2.0 µs |
| Radeon iGPU | 4,731–4,758 µs | 4,731–4,749 µs | 0.24–0.28 µs |

The NVIDIA driver does not start the next command before the timestamp; AMD's does.

Options:

1. **A pass's own time on its queue** (chosen): from its opening timestamp or from the
   previous pass's closing timestamp on the same queue, whichever is later, to its
   closing timestamp. The times then partition each queue's busy time; overlap is
   counted once, on the earlier pass. Free, always on, right for the profiler and the
   perf gate alike.
2. **A serialized mode for the gate** (a barrier before every opening timestamp): it
   measures another frame than the one players run, costs GPU time, and the profiler
   stays wrong.
3. **Opening timestamps at `ALL_COMMANDS`**: they wait for the previous work, which is
   option 2 in all frames.

Decision: `PassTime::gpu_us` is the pass's own time (option 1); `PassTime::span_us` keeps
the span between its two timestamps. Tracy's GPU zones keep the raw timestamps (they show
the overlap). Work on another queue (async compute) still overlaps: each queue's times
are its own, and `RenderGraph::gpu_frame_time` gives the frame end to end.

Effect on ref-1's Radeon: in `flight`, texture feedback 2,430 → 88 µs and light tiles
~100 → 7.5 µs; in the perf gate the culling passes read 0.3–1.6 µs instead of 4–76, the
depth prepass 12 % less in two scenes. Both perf baselines were re-measured; the RTX's
moved within a few µs.

## Addendum (2026-10-15): transient heaps the frame slots share

The second review (§3.9): each frame slot (frame in flight) held its own transient heaps.
`flight` reserved 65 MiB a slot on the RTX at 1080p, 20 MiB on the Radeon iGPU (its
render size smaller), so twice that in all.

- **Frames run on the graphics queue in order.** Transients only that queue touches can
  live in heaps every slot shares, if each frame's work on them starts after the
  previous frame's ends there.
  - Each frame's first graphics batch now begins with a global barrier (all commands,
    memory writes) on everything before it on the queue.
  - Measured first, alone (`flight`, 600 frames, alternated with main's): the frame
    interval moved within the noise, RTX 1,610–1,634 → 1,626–1,629 µs, Radeon 25.68–25.70
    → 25.70–25.87 ms. Frames hardly overlap on the queue anyway: the interval was the GPU
    frame's time.
- **The async compute queue runs beside the graphics queue** (ADR 0065), also beside the
  previous frame's graphics work. A group of transients (kind and memory types) that its
  passes touch keeps a heap per slot.
  - The group is not split by queue: its resources keep aliasing one another.
  - Split, the RTX (whose ambient occlusion runs on that queue) needed 33 MiB shared plus
    51 MiB a slot, 135 MiB in all instead of 130.
- **A shared heap replaced by a larger one** is freed once every slot has been realized
  since, when no frame in flight uses it any more. Each slot destroys its objects in it
  as it always did, as unused.
- **`GraphStats::transient_shared_bytes`** tells the shared part of
  `transient_heap_bytes`.

Measured (`flight` on Sponza's pack, 1080p, the device classes' defaults, two frames in
flight):

| | before | after |
|---|---|---|
| Radeon iGPU (no async compute): transient heaps in all | 2 × 20 = 40 MiB | 20 MiB, all shared |
| RTX 4060 Ti (AO on async compute) | 2 × 65 = 130 MiB | 1 MiB shared (buffers) + 2 × 64 = 129 MiB |

**Test.** `frames_in_flight_share_transient_heaps_without_hazards`: two frames in flight
submitted back to back, under synchronization validation, both GPUs. With the barrier
there are no errors; without it, 7.
