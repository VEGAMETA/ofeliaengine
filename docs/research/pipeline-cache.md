# Pipeline caches and warm-up

Research for the second priority (2026-09-30): shader and pipeline caches, warm-up, no
hitches. Decision: [ADR 0055](../adr/0055-pipeline-cache-and-warm-up.md).

Checked on 2026-09-30 on ref-1:

- NVIDIA 616.92 (RTX 4060 Ti);
- AMD Adrenalin 25.10 (Radeon iGPU).

The drivers' behavior was measured, not read from their code. Engines are described from
their public documentation.

## Sources

| Source | Version | License | What we take |
|---|---|---|---|
| Vulkan specification: `VkPipelineCache`, `VkPipelineCacheHeaderVersionOne`, `pipelineCreationCacheControl` | 1.3 / 1.4 | spec | cache data format header; `FAIL_ON_PIPELINE_COMPILE_REQUIRED`; caches are internally synchronized unless created with `EXTERNALLY_SYNCHRONIZED` |
| `VK_KHR_pipeline_binary` | rev. 1 (1.3.295) | spec | per-pipeline binaries and keys; internal cache control |
| `VK_EXT_graphics_pipeline_library` | rev. 1 | spec | fast linking of pre-compiled stages |
| Arseny Kapoulkine, "Robust pipeline cache serialization" (2019) | blog | — | validate the header and a checksum before handing data to a driver; drivers have crashed on bad data |
| Unreal Engine (PSO caching, bundled PSO cache, precaching), Unity (`GraphicsStateCollection`, shader warm-up) | public docs | products | record the pipelines used, warm them up at load |

## What the drivers do (measured)

- **Both drivers keep their own disk caches**, keyed by program:
  - NVIDIA: renaming the executable starts cold;
  - AMD: moving it to another folder starts cold.
  - A game hits the cold case after a patch that replaces the executable, a reinstall,
    a move, a cleared or evicted driver cache, and after every driver update.
- **What an application `VkPipelineCache` from a file brings:**
  - NVIDIA: a renamed copy that loads the file creates the 53 pipelines in 49 ms instead
    of about 300;
  - AMD: a moved copy (same name) takes 12 ms. A renamed copy finds nothing in the file:
    AMD keys the entries by executable name, like its application profiles.
- **Lookups do not parallelize on NVIDIA:** 41 lookups from a warm cache take 6 ms one
  after another and 38 ms on 16 threads.
- **`FAIL_ON_PIPELINE_COMPILE_REQUIRED`:**
  - both drivers honor it;
  - NVIDIA also finds pipelines only its own cache holds, at about 1 ms each (0.1 ms when
    the application cache has them);
  - a miss returns at once.
- **Compiles scale with threads:** 53 cold pipelines take 1,433 → 270–299 ms on NVIDIA and
  1,015 → 307–324 ms on AMD. The time summed over threads doubles (2.6–3.1 s): the
  drivers slow each compile down when many run at once, but the wall time falls 3–5×.
- **Cache data:** 1.6 MB (NVIDIA), 0.67 MB (AMD) for 53 pipelines. Its bytes are the same
  from run to run, so an unchanged cache is not rewritten.
- **`VK_KHR_pipeline_binary`:** both drivers list it (`pipelineBinaryInternalCache`
  true). Only NVIDIA lets the application switch its internal cache off. ash 0.38 has no
  bindings for it.

## Engines

- **Unreal:**
  - records the pipeline states a playtest uses (PSO cache) and ships the list;
  - precompiles it at load, or in the background with a progress bar;
  - newer versions precache pipelines when assets load.
- **Unity:**
  - `GraphicsStateCollection` records the variants and states used;
  - `WarmUp` creates them during a loading screen.
- Both need recording because their materials create pipeline states on first use.

Ours are all known when a renderer is constructed (bindless, specialization constants,
dynamic cull state). Recording is needed only once the material graph produces shaders
at run time.

## Measured

See ADR 0055 for the table: cold 270–324 ms, warm 6–7 ms, 12–49 ms with only our file.
