# 0065. Async compute in the render graph: opt-in passes, scheduled by the graph, on by default where measured faster

- Status: Accepted
- Date: 2026-10-01
- Research: [docs/research/async-compute.md](../research/async-compute.md)
- Related: [ADR 0011](0011-render-graph.md) (the render graph; this is its next step 1),
  [ADR 0058](0058-ambient-occlusion.md) (GTAO, the pass that overlaps),
  [ADR 0062](0062-golden-images-and-perf-gate.md) (golden images and the perf gate)

## Context

The device creates a compute queue on a family without graphics when the GPU has one
(`Device::async_compute`), but nothing used it: the render graph recorded every pass into
the frame's one graphics command buffer. CLAUDE.md §6 asks the graph for async compute,
and ADR 0011 lists it first among its next steps.

The forward frame has a classic overlap: ground-truth ambient occlusion (compute, 0.46 ms
at 1080p High on the RTX 4060 Ti) needs only the depth prepass, and the shadow maps
(depth-only rasterization, 0.16 ms on Sponza) do not need it. On one queue they run one
after the other.

Requirements:

- results identical to the single-queue frame (golden images unchanged);
- no hazards under synchronization validation, which checks across queues;
- zero heap allocations per frame;
- a clean fallback on devices without a separate compute queue;
- per-pass GPU times on both queues, and the frame's time end to end;
- frame times must not regress on either reference GPU with the chosen defaults;
- `forward.rs` is being changed by another task (persistent GPU scene): keep changes
  there minimal.

## Options

### Which passes run on the compute queue

1. **Opt-in per pass, `PassBuilder::async_compute()`** (chosen; Unreal RDG, Frostbite):
   the pass author marks candidates; the graph decides whether they can run there (no
   attachments, only compute, transfer and indirect-argument stages) and whether async
   compute is on.
2. **The graph picks passes itself** (Frostbite's reachability idea): which pairs gain
   depends on their bottlenecks and on the GPU (both vendors say measure per task); our
   measurements below show two of four candidates losing. Automatic choice would need a
   cost model we do not have.

### Order

1. **Reorder within the declared dependencies** (chosen): a topological order of the
   passes' dependencies (read after write, write after read, write after write; a layout
   change counts as a write; with exclusive sharing, any access after an access on the
   other queue) that prefers the ancestors of earlier async passes. What an async pass
   needs runs as early as possible, and the graphics work that does not lead to it runs
   while it does. In the forward frame the prepass moves before the shadows, with no
   change to `forward.rs`.
2. **Declaration order** (no reordering): AO's input (the prepass) is declared after the
   shadows, so nothing would overlap AO without moving declarations in `forward.rs`.

### Synchronization and submission

1. **Segments joined by timeline semaphores, submitted by the graph** (chosen): the
   frame splits into batches where results cross queues; each batch signals its queue's
   timeline semaphore and waits for the other queue's batch it depends on. The graph
   submits every batch but the last graphics one, which it records into the caller's
   command buffer as before. The caller's submit call does not change: a wait in an empty
   batch before it holds back later batches on the queue (a semaphore wait's second scope
   includes every later command in submission order).
2. **The graph returns a submission plan to the caller:** every caller (editor, viewers,
   tests, tools; about 50 call sites) would change how it submits.

### Sharing resources between the queue families

1. **Exclusive with ownership transfers** (chosen by default): release on one queue,
   acquire on the other, layouts transitioned between. Imported resources always do this
   (their owners created them exclusive).
2. **Concurrent sharing for transients both queues use** (option,
   `AsyncSharing::Concurrent` or `ENGINE_ASYNC_SHARING=concurrent`): no transfers, reads
   on both queues at once, but drivers may stop compressing such images (the spec warns of
   "lower performance access"). Measured the same on the RTX (below); exclusive keeps
   compression on AMD, so it is the default.

## Measurements

ref-1, release, `VK_LOADER_LAYERS_DISABLE=~implicit~`, commit of this branch. GPU frame
end to end (first pass to last on the graphics queue), medians of alternating runs.

`scene_bench` Sponza 1920 × 1080 `--ao high --sky --post --lights 64`:

| GPU | off | on, exclusive | on, concurrent |
|---|---:|---:|---:|
| RTX 4060 Ti | 1,253–1,254 µs | 1,193–1,197 µs (−4.6 %) | 1,195–1,198 µs |
| Radeon iGPU (2 CUs) | 29,198–29,345 µs | 29,272 µs (+0.3 %) | 29,393 µs (+0.2 %) |

- The RTX hides about 60 µs of the 190 µs of graphics work beside AO; the shadow pass
  reads 162–176 → 207–215 µs while sharing the GPU.
- The Radeon's two compute units time-slice the queues: the shadow pass stretches from
  2.3 to 14.3 ms (as long as AO) and the frame does not shrink.
- Other candidates on the RTX (async on): light tiles 1,205 µs and exposure 1,200 µs vs
  1,197 for AO alone (13 and 19 µs passes do not pay for a split); environment rebuild
  every frame without AO 882 → 851 µs (−3.5 %), with AO 1,222 vs 1,214 for AO alone.
  Radeon, environment rebuild every frame: 22,347 → 22,323 µs (noise).
- Perf gate (`perf_gate`, new `sponza_ao` scene, 3 runs × 200 frames): RTX 1,178 → 1,134
  µs; Radeon 27,197 → 27,015 µs forced on (within its noise; off by default). The two
  existing scenes are the same as on `main` on both GPUs (procedural 457.6–458.2 vs
  457.9–458.0 µs, Sponza 713.2–713.5 vs 713.3–714.7 µs on the RTX; Radeon within
  ±0.5 % in alternating runs).
- CPU, `FrameGraph::execute` on the forward frame with AO (Sponza, RTX, median of 200
  frames, three alternating pairs): 62–67 µs off, 98–105 µs on. Of the 35–40 µs more:
  scheduling 3–4 µs (it runs only in frames with async passes; O(passes²) selection over
  the ~30 passes), recording three more command buffers 11–15 µs, and four batches
  submitted 28–30 µs (grouping the batches of one queue into one call did not change
  that, so each batch is submitted on its own).
- Correctness: golden images identical (FLIP 0) on both GPUs with the defaults; the
  ambient occlusion shot is byte-identical with async compute on and off on both GPUs;
  `tests/async_compute.rs` (transients and imports crossing queues, two frame slots, both
  sharing modes) reads back the same bytes as one queue with zero synchronization
  validation errors on both GPUs (removing the semaphore waits makes it report hazards);
  zero heap allocations over 100 async frames.

## Decision

- **`PassBuilder::async_compute()` / `ComputePass::async_compute()`** mark a pass. It runs
  on the async compute queue when the graph's switch is on, the device has the queue and
  the pass can (no attachments; accesses only in compute, transfer and indirect stages);
  otherwise on the graphics queue. Results are the same either way.
- **Switch:** `RenderGraph::set_async_compute(bool)`; default `async_compute_default`:
  `ENGINE_ASYNC_COMPUTE=1|0` if set, else **on for NVIDIA, off for AMD and others**
  (measured above; Intel unmeasured). Without a compute-only queue family it is always
  off.
- **Schedule** (`graph/schedule.rs`): dependencies from the declared accesses; the order
  prefers ancestors of earlier async passes; passes with no declared accesses keep the
  declared order; a fence (`PassBuilder::fence`) follows every pass declared before it
  and precedes every pass declared after it (addendum below). Segments:
  - a segment ends where the other queue needs its results; one starts where a pass needs
    the other queue's;
  - a compute segment always waits for the graphics work before it in the order (which
    also orders it after earlier frames' graphics work on that queue);
  - the last graphics segment (the caller's command buffer) waits for every compute
    segment, so the caller's frame completion (timeline, fence) covers both queues;
  - a frame whose acquired swapchain image would be touched outside the caller's command
    buffer runs on one queue (its submission waits for the image).
- **Lifetimes:** a resource used by an async segment lives over the segment's whole
  window (its start to the graphics pass that waits for it), so transient memory is
  never shared with work that may run at the same time.
- **Barriers:** per queue, from the same state tracking as before. Across queues:
  ownership release at the end of the releasing segment and acquire before the first use
  (exclusive), or a layout transition at the end of the releasing segment (concurrent);
  discarding first uses need neither. Imported resources return to the graphics queue
  before the frame's final transitions. Barriers on the compute queue only name stages it
  supports.
- **Sharing:** exclusive with ownership transfers by default; concurrent for transients
  with `AsyncSharing::Concurrent` (`ENGINE_ASYNC_SHARING=concurrent`).
- **Timing:** per-pass timestamps on both queues (`PassTime::queue`), Tracy GPU zones on
  an "async compute queue" context, and `RenderGraph::gpu_frame_time()` (graphics queue,
  first pass to last). `GraphStats` counts async passes, the graph's submissions and
  ownership transfers.
- **Passes marked async:** ambient occlusion (its three passes) and the environment
  rebuild (sky, prefilter, irradiance, DFG table). Light tiles and automatic exposure
  stay on the graphics queue (measured slower). `forward.rs` is unchanged.
- **Per-subresource barriers** (ADR 0011 step 3) are not needed for this overlap and are
  deferred.
- **The trade:** on the RTX the default spends 35–40 µs of render-thread CPU per frame
  with async passes for about 60 µs of GPU frame time. A GPU-bound frame gains; a
  CPU-bound one with spare GPU time would lose a little, and turns it off with the
  switch.

## Consequences

- With async compute on, `FrameGraph::execute` submits to the device's graphics and
  compute queues itself; callers submit the frame's command buffer to the device's
  graphics queue afterwards, as before, and must not use the queues from other threads
  during `execute`. Commands recorded into the frame's command buffer before
  `RenderGraph::begin` run after the graph's earlier segments.
- Per-pass GPU times of overlapped passes include the time they share the GPU: compare
  frames with `gpu_frame_time` (the perf gate's `(gpu frame)` row), not with sums of
  passes.
- Breadcrumbs (ADR 0061) mark graphics passes only; a hang inside async compute work
  shows as the graphics pass waiting for it.
- Revisit:
  - the defaults on new GPUs and drivers (Intel, larger AMD GPUs where the compute
    queue may win), with `scene_bench --async-compute on|off`;
  - more overlap: the next frame's shadows beside this frame's post-processing (needs
    scheduling across frames), HiZ and culling beside shadows in the two-phase occlusion
    path;
  - `VK_KHR_maintenance9`: buffers keep their contents across families without
    transfers;
  - per-subresource barriers when a pass reads one mip of an image another writes.

## Addendum (2026-10-11): fences for writes read without being declared

Found while building the probe GI (ADR 0084): with async compute on, its first update
after loading Sponza saw every material texture black. The texture pool's upload pass
declares only its staging buffers; shaders reach the textures through bindless indices
from the material table, which nothing ties to the upload. The schedule put the passes
that lead to an async pass first (the GI updates lead to the reflection probes' capture,
which reads the volume, and so to its async mips and prefilter), so they ran before the
upload in the frame the textures arrived. The same held for every frame that uploads
texture levels: a pass leading to an async pass could sample levels that arrive later in
that frame (a frame of black or stale texels where the mip fade starts, ADR 0078), and
the masked depth prepass could test the alpha of textures not copied yet.

- `PassBuilder::fence()`: the pass follows every needed pass declared before it and
  precedes every needed pass declared after it (one edge from each later pass; the fence
  has an edge to each pass since the previous fence). Unlike a pass that declares no
  accesses, it does not turn the whole frame back to the declared order: the passes
  between fences are still reordered.
- The texture pool's upload pass is a fence. `Texture::upload` declares no accesses
  (the declared order holds for its frames, as before).
- AMD devices run without async compute by default (above), so only NVIDIA (and
  `ENGINE_ASYNC_COMPUTE=1`) showed it.
- Test: `a_fence_follows_the_passes_declared_before_it_and_precedes_the_rest`
  (`graph/schedule.rs`): the upload runs after the AO pass and its ancestors without the
  fence, first with it, and last when declared last.
