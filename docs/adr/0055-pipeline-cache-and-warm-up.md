# 0055. Pipeline cache and warm-up: a validated VkPipelineCache on disk, cache lookups first, compiles in parallel

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/pipeline-cache.md](../research/pipeline-cache.md)
- Related: [ADR 0009](0009-shader-hot-reload.md) (shader builds),
  [ADR 0013](0013-bindless-and-pipelines.md) (pipelines),
  [ADR 0052](0052-compute-passes.md) (compute shaders)

## Context

CLAUDE.md §9 asks that players never see compile hitches:

- persistent pipeline caches with header validation;
- warm-up on first launch and after a driver update.

The renderers create every pipeline in their constructors: the forward renderer (38
graphics and 3 compute), tone map, bloom, exposure, environment and TAAU make 53. No
pipeline is created while drawing, so start-up time is the problem to solve, not hitches.

Measured (`pipeline_bench`, ref-1):

- **Cold:** compiling the 53 pipelines one after another took **1,433 ms** on the RTX
  4060 Ti and **1,015 ms** on the Radeon iGPU. The forward renderer alone takes 1.3 s and
  0.83 s.
- **Warm:** with the driver's own cache warm, 7.1 ms and 5.5 ms.
- **The drivers' caches are per program:**
  - NVIDIA: a copy of the same executable under another name starts cold;
  - AMD: the same executable in another folder starts cold.

  Players hit the cold case after a patch or a reinstall, when the game moves, when the
  driver's cache is evicted or cleared, and after every driver update.

## Options

### Keeping compiled pipelines between runs

1. **`VkPipelineCache` saved to a file** (chosen):
   - core Vulkan 1.0, every driver;
   - one blob per device;
   - validated by our own header before the driver sees it.
2. **`VK_KHR_pipeline_binary`:**
   - per-pipeline binaries with keys, so it knows before creating whether a pipeline is
     stored;
   - can switch off the driver's own cache (NVIDIA; the AMD driver does not allow it);
   - both reference drivers expose it, but ash 0.38 (Vulkan 1.3.281) has no bindings: ten
     structures and five functions by hand.
   - The file cache already makes warm starts 6–7 ms, so the gain is control, not time.
     Next step.
3. **Only the driver's cache:** nothing to do, but cold after every move, patch or
   eviction (above).

### Creating many pipelines

1. **In parallel** (chosen for compiles):
   - drivers compile on the calling thread, so 53 pipelines on 16 threads take the time
     of the slowest few;
   - measured cold: 1,433 → 270 ms (RTX), 1,015 → 307 ms (Radeon).
2. **Lookups in parallel too:** worse.
   - On NVIDIA, 41 lookups from a warm cache on 16 threads took 38 ms, one after another
     6 ms: the driver serializes them.
   - So a batch first creates each pipeline with
     `VK_PIPELINE_CREATE_FAIL_ON_PIPELINE_COMPILE_REQUIRED_BIT` (core 1.3,
     `pipelineCreationCacheControl`) on the calling thread, and only the misses compile
     in parallel.
3. **`VK_EXT_graphics_pipeline_library`:** fast linking of pre-compiled parts, for pipelines
   that appear while drawing. None do yet; with the material graph they will.

### Where the threads come from

The engine's job pool when the application gives one (`Device::set_job_pool`), otherwise
scoped threads started for the batch (one per core). Start-up is not a frame; a batch
never runs while drawing.

## Numbers

`pipeline_bench` (all 53 pipelines of the shipped renderers), ms:

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| before: cold, one after another | 1,433 | 1,015 |
| before: driver's cache warm | 7.1 | 5.5 |
| **cold, parallel** | **270–299** | **307–324** |
| **our file only** (driver cold: renamed / moved executable) | **48.8** | **12.3** |
| our file + driver's cache | 6.8 | 6.3 |
| driver's cache only (no file yet) | 47.8 | 6.5 |

- The file: 1.6 MB (NVIDIA), 0.67 MB (AMD); loading 0.9–1.7 ms, saving 1.5 ms,
  "unchanged" check 0.6–1.0 ms.
- Editor cold start (`editor --frames 60`), second run: pipelines 213 → 8 ms, first
  frame 623 → 371 ms (NVIDIA); 202 → 8 ms and 568 → 363 ms (Radeon).
- A lookup through the flag costs NVIDIA about 1 ms when only its own cache holds the
  pipeline (47.8 ms for 52). It costs 0.1 ms when our file does. That is the price of
  the first run after this change on a machine whose driver cache was already warm.

## Decision

- **The device owns one pipeline cache**, internally synchronized. `Pipeline::graphics`,
  `compute` and batches always go through it; the `cache` parameter is gone.
- **`Device::load_pipeline_cache(path)`**, before the renderers:
  - never fails: `Loaded`, `Missing` or `Rejected(reason)`;
  - the envelope (magic, version, vendor, device, driver version, pipeline cache UUID,
    length, `state_hash64` checksum) and the driver's own header are checked before the
    driver sees the data;
  - what the cache gathered before the load is merged in.
- **`Device::save_pipeline_cache(path)`:**
  - written to `<path>.tmp` and renamed over the file, so a crash never leaves half a
    file;
  - skipped when the data's hash is unchanged.
  - `Device::pipeline_cache_file_name()` gives `pipelines-<vendor>-<device>.bin`, one file
    per GPU.
- **`Pipeline::create_many(device, layout, &[PipelineDesc])`:** cached pipelines first on
  the calling thread, then the misses in parallel (`Device::create_parallel`), results in
  order.
  - `ComputeSource<P>` checks a compute shader's push constants without creating it.
  - `ComputeSource::create_all` creates several; `desc`/`finish` put one into a larger
    batch.
  - The forward renderer creates its 41 pipelines as one batch; the environment, bloom,
    exposure and TAAU each as one.
- **Measuring:**
  - `creation_stats()`: created, cache hits and misses, time in the driver;
  - `ENGINE_PIPELINE_SALT=<n>` adds an unused specialization constant, so every cache
    misses (a cold compile on demand);
  - `pipeline_bench` prints the table above.
- **The editor** keeps its caches in `%LOCALAPPDATA%\ofeliaengine\editor`. It loads before
  creating renderers, saves after start-up and at exit, and reports hits in its cold-start
  line.

## Consequences

- A game starts its renderers in about 7 ms from the second run on. After a driver update,
  a move or a patch the first start costs about 0.3 s instead of 1–1.4 s.
- Games must call load and save (the dev guide shows where). There is no application
  layer yet to do it for them.
- Pipelines created at run time (hot reload today, the material graph later) go through
  the same cache; a record of them for warm-up lists comes with the material graph.
- Not done:
  - `VK_KHR_pipeline_binary` (hand-written bindings; control over the driver's duplicate
    cache);
  - `VK_EXT_graphics_pipeline_library` for pipelines that appear while drawing;
  - creating the renderers themselves in parallel (the forward renderer's 170–200 ms
    dominates a cold start; the others add 70–130 ms after it);
  - warm-up progress for a loading screen (counts are in `creation_stats`).
