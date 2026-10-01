# Async compute in the render graph: research

Date: 2026-10-01. Input for [ADR 0065](../adr/0065-async-compute.md). Sources were read for
ideas; no code was copied (Granite is MIT, but nothing of it was adapted either).

## Sources

| Source | Version / date checked | License | What we took from it |
|---|---|---|---|
| S. Hodes (AMD), ["Leveraging asynchronous queues for concurrent execution"](https://gpuopen.com/learn/concurrent-execution-asynchronous-queues/), GPUOpen, 2016-12-01; the GDC 2016 "Async Compute: Deep Dive" material it summarizes | read 2026-10-01 | article | which passes pair well, the frame as a task graph, the cost of splitting command lists, toggle per task and measure |
| V. Bondarev, S. Niverty (NVIDIA), ["Advanced API Performance: Async Compute and Overlap"](https://developer.nvidia.com/blog/advanced-api-performance-async-compute-and-overlap), 2021-10-22 | read 2026-10-01 | article | pair different datapaths (math-bound compute with shadow rasterization), avoid long async work that blocks the other queue, avoid overlapping cache-heavy passes |
| Y. O'Donnell, "FrameGraph: Extensible Rendering Architecture in Frostbite", GDC 2017 ([GDC Vault](https://www.gdcvault.com/play/1024612/FrameGraph-Extensible-Rendering-Architecture-in)) | talk; the slides were not reachable on 2026-10-01 (SlideShare HTTP 451), from the talk and secondary summaries | — | async passes opt in per pass, sync points found from the graph's dependencies, resource lifetimes extended to the sync points |
| H.-K. Arntzen (Themaister), ["Render graphs and Vulkan — a deep dive"](https://themaister.net/blog/2017/08/15/render-graphs-and-vulkan-a-deep-dive/), 2017; [Granite](https://github.com/Themaister/Granite) `renderer/render_graph.cpp` | `master`, read 2026-10-01 | MIT | queue flags per pass, `VK_SHARING_MODE_CONCURRENT` only for resources both queues use, semaphores between queues |
| Epic Games, ["Render Dependency Graph in Unreal Engine"](https://dev.epicgames.com/documentation/en-us/unreal-engine/render-dependency-graph-in-unreal-engine), `FRDGPass` API (UE 5.3) | read 2026-10-01 | documentation | `ERDGPassFlags::AsyncCompute`; fork at the last graphics producer, join at the first graphics consumer; platform fallback to the graphics pipe; `r.RDG.AsyncCompute` switch |
| Khronos, Vulkan specification (`chapters/synchronization.adoc`, `cmdbuffers.adoc`, `resources.adoc`, `main`) | read 2026-10-01 | CC-BY 4.0 | semaphore scopes, queue family ownership transfers, `VK_SHARING_MODE_CONCURRENT` |
| Khronos validation layers, synchronization validation | Vulkan SDK on ref-1 | Apache-2.0 | checks hazards across queues at submission (it caught the deliberately removed wait, below) |
| K. Anagnostou, ["Async compute all the things"](https://interplayoflight.wordpress.com/2025/05/27/async-compute-all-the-things/), Interplay of Light blog, 2025 | read 2026-10-01 | blog | ambient occlusion and other depth-only consumers as candidates; measure on each GPU |

## What the sources say

### When async compute pays (AMD, NVIDIA)

- It fills units the graphics work leaves idle. The classic pairs:
  - math- or LDS-heavy compute beside **depth-only rasterization** (shadow maps, a depth
    prepass): the raster front end is busy, the shader cores are not;
  - **ambient occlusion** (needs the depth, independent of shadows and lighting) beside the
    shadow maps (AMD names AO explicitly);
  - post-processing of frame N beside the next frame's shadows or G-buffer.
- **Not:**
  - two passes bound by the same unit (two cache-heavy passes thrash each other);
  - long async work the other queue soon waits for;
  - small passes: every split costs a submission and a semaphore round trip, the
    overlap must be worth more ("splitting the command list and stalling on fences"
    costs, AMD).
- **Measure per task and per GPU** (both vendors): the result depends on the GPU's size;
  some GPUs only time-slice the queues.

### How engines schedule it

- **Frostbite FrameGraph:** passes opt into async compute; the compiler finds the
  synchronization points from the dependencies and extends the lifetimes of the resources
  an async pass uses to those points, so transient memory aliasing stays correct while the
  queues overlap.
- **Unreal RDG:** `ERDGPassFlags::AsyncCompute` on a pass. RDG forks after the last
  graphics producer of the async interval's inputs and joins before the first graphics
  consumer of its outputs. Platforms without async compute fall back to the graphics
  pipe; `r.RDG.AsyncCompute` turns it off.
- **Granite:** each pass has a queue flag. The baked graph reorders passes to spread
  dependent ones apart. It shares resources used by both queues with
  `VK_SHARING_MODE_CONCURRENT` ("dealing with EXCLUSIVE will be brutal": read-after-read
  needs ownership ping-pong) and signals semaphores between the queues. The post reports
  no measurements.

### Vulkan specifics

- **Timeline semaphores** (core in 1.2) carry the cross-queue dependencies: each batch
  signals a value on its queue's timeline; a batch on the other queue waits for it.
  - A wait's second synchronization scope is the batch's commands **and every command
    later in submission order** on that queue (`vkQueueSubmit2`). So an empty batch with
    only a wait holds back the batches after it: the graph uses this to make the caller's
    own submission wait without changing its submit call.
  - A signal's first scope is the batch and every earlier command on the queue; its access
    scope is all device memory accesses; a wait's second access scope too. Writes before a
    signal are therefore visible after the wait without further barriers; only layout
    transitions and ownership transfers need barriers.
  - A barrier on the waiting queue chains on the wait if its source stages intersect the
    wait's stage mask (the swapchain-acquire pattern).
- **Sharing:**
  - `VK_SHARING_MODE_EXCLUSIVE` resources belong to one queue family at a time. To keep
    the contents across families, a **queue family ownership transfer** is needed: a
    release barrier on the source queue and a matching acquire barrier on the destination
    (same layouts; the transition happens between). Without one the contents are
    undefined, unless they are not needed (a discarding first use needs no transfer).
    With `maintenance9`, buffers and linear images keep their contents without transfers.
  - `VK_SHARING_MODE_CONCURRENT` lists the families at creation; no transfers, but it "may
    result in lower performance access" (spec note): drivers may give up compression
    (AMD's DCC and HTILE) for such images.
  - Exclusive images cannot be read by both queues at once: concurrent reads need
    `CONCURRENT`, or are serialized by transfers.
- **Stages:** a compute-only queue supports compute, transfer and indirect-argument stages:
  barriers recorded there may not name graphics stages.

### Cost of synchronization across queues

- Each split adds a `vkQueueSubmit2` (CPU microseconds) and a semaphore round trip on
  the GPU (measured below: the GPU frame without any overlap is 19 µs longer than the sum
  of its passes on the RTX 4060 Ti; splitting light tiles (13 µs) or exposure (19 µs) off
  costs more than it saves).

## What we take

- **Opt-in per pass** (`PassBuilder::async_compute`, as RDG and Frostbite), plus a graph
  switch and an environment variable, defaulting per vendor by measurement.
- **Scheduling by the graph:** a topological order of the declared dependencies that
  prefers the ancestors of earlier async passes (Frostbite's "as early as the inputs
  allow"; RDG's fork at the last producer), so independent graphics work moves into the
  overlap: on our frame, the depth prepass moves before the shadow pass and ambient
  occlusion overlaps the shadows and light tiles.
- **Fork and join with timeline semaphores**, one per queue, a segment (batch) per run of
  passes between cross-queue edges (RDG's fork/join).
- **Lifetimes extended over the async window** (Frostbite), so aliasing stays correct.
- **Both sharing modes:** ownership transfers (exclusive) for everything by default, and
  concurrent sharing for transients as an option; imported resources always move with
  transfers (their owners created them exclusive). Measured equal on the RTX, so the
  default keeps compression (exclusive).
- **A frame time end to end** next to per-pass times: overlapped passes read longer while
  the frame gets shorter, so only the frame's own span shows the gain.

## What we do not take, and why

- **Baked reordering for pipelining distance** (Granite): our graph is per frame
  (ADR 0011); we reorder only when async passes exist.
- **Automatic choice of async passes** (Frostbite's reachability idea): which passes win
  depends on the pair and the GPU (both vendors say measure per task); opt-in with
  measurements is predictable.
- **Split barriers / events**: diminishing returns on desktop, out of scope.
- **Async compute across frames** (post-processing of frame N beside frame N+1's
  shadows): needs the frame's tail and the next frame's head in one schedule; possible
  later with frames in flight, see the ADR.

## Our measurements (ref-1, release, `VK_LOADER_LAYERS_DISABLE=~implicit~`)

`scene_bench` Sponza 1920 × 1080 `--ao high --sky --post --lights 64`, GPU frame end to
end, median of alternating runs (async off / on):

| GPU | off → on, exclusive sharing | off → on, concurrent sharing | AO passes | shadow pass off → on |
|---|---:|---:|---:|---:|
| RTX 4060 Ti | 1,254 → 1,193 µs (−4.9 %) | 1,253 → 1,215 µs, repeated 1,195–1,198 | 457–488 µs | 162–176 → 207–215 µs |
| Radeon iGPU (2 CUs) | 29,198 → 29,272 µs (+0.3 %) | 29,345 → 29,393 µs (+0.2 %) | 12.0 ms | 2.3 → 14.3 ms |

The perf gate's `sponza_ao` scene (no local lights, fixed exposure): RTX 1,178 → 1,134–1,138
µs (−3.6 %); Radeon 27,197 → 27,015 µs (−0.7 %, separate runs, within the iGPU's
run-to-run spread; `scene_bench` above read +0.2–0.3 %).

- On the RTX the overlap hides about 60 µs of the 190 µs of graphics work beside AO
  (shadows, light tiles); the shadow pass itself reads 25 % longer while sharing the GPU.
- On the 2-CU Radeon the queues time-slice: the shadow pass stretches over the whole AO
  pass and the frame does not get shorter (±0.5 %, the iGPU's noise): async compute off
  by default on AMD.
- Other candidates on the RTX (async compute on, `--ao high`): light tiles async 1,205 vs
  1,197 (worse), exposure async 1,200 (no gain); environment rebuild every frame without
  AO 882 → 851 µs (−3.5 %), with AO 1,222 vs 1,214 for AO alone (both on the compute
  queue, one after the other). Light tiles and exposure stay on the graphics queue.

## Patents

Nothing patented is used: scheduling with a topological sort, semaphores between queues
and ownership transfers are the API's own mechanisms, described in the specification and
in public talks.
