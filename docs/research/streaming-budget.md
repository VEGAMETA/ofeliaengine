# A GPU memory budget, texture streaming by GPU feedback, mesh detail-level streaming

Research for the next item of the core queue (CLAUDE.md §17): the GPU memory budget and
texture streaming step 2 (ADR 0057 left "which levels to load" and "a memory budget"),
with mesh detail levels streamed the same way. Decision:
[ADR 0076](../adr/0076-gpu-memory-budget-and-streaming.md). Checked 2026-10-02 on ref-1
(RTX 4060 Ti, driver 616.92; Radeon RDNA2 2-CU iGPU, driver 25.10.36). No code was
copied from any source below; the GPL/closed products were studied from their
documentation only.

## Sources

| Source | Version / date | License | What we take |
|---|---|---|---|
| `VK_EXT_memory_budget` (Khronos registry, refpage) | rev. 1, checked 2026-10-02 | spec | per-heap budget and usage of the process |
| `VK_EXT_memory_priority`, `VK_EXT_pageable_device_local_memory` (refpages) | rev. 1 | spec | what the OS pages out first under pressure; not used yet (below) |
| Vulkan Memory Allocator, "Staying within budget" | 3.x docs, checked 2026-10-02 | MIT | query every frame, estimate between queries from own allocations, `WITHIN_BUDGET` for non-critical resources |
| Unreal Engine: "Texture Streaming Overview", "Texture Streaming Configuration" | UE 5 docs, checked 2026-10-02 | product docs | wanted mips from texel factor × screen size; load and retention priorities; drop one mip at a time until the plan fits the pool |
| Unreal Engine virtual texturing: `r.VT.FeedbackFactor` | UE 5 docs/forums | product docs | the feedback buffer is the screen over a factor (16 by default), a different pixel each frame |
| Unity "Mipmap Streaming" manual, `QualitySettings.streamingMipmapsMemoryBudget` | 2023.1 / 6000 docs | product docs | a memory budget; mips reduced until it fits; per-texture priority as a mip offset |
| J.M.P. van Waveren, "Software Virtual Textures" (2012); id Tech 5/6 descriptions | paper | paper | feedback rendered to a small buffer: page coordinates, mip, texture id; read back a few frames late |
| D3D12 sampler feedback (MinMip / MipRegionUsed feedback maps) | DirectX spec | spec | the hardware version of the same measurement; D3D12 only |
| meshoptimizer README ("LOD chains with a shared vertex buffer") | `master`, checked 2026-10-02 | MIT | order the vertices by first use from the coarsest level (`optimizeVertexFetch` over the levels concatenated coarsest first), so each level is a prefix |
| ADR 0053 (our detail levels), ADR 0056/0057 (packs, the I/O ring, the staging ring) | — | ours | the parts streaming builds on |

## The budget: what the OS says and what to do with it

- **`VK_EXT_memory_budget`** gives, per memory heap, `heapBudget` (what this process can
  use before allocations fail or the OS starts moving memory out) and `heapUsage` (what
  the process uses, every allocator and the driver included). On Windows the budget comes
  from the WDDM video memory manager: it shrinks when other processes need video memory
  (another game, a browser's video decoder), so it is queried continuously, not once.
- **Over budget** the OS demotes allocations to system memory (discrete GPUs: they are then
  read over PCIe) or allocations fail; the visible symptom is hitches. Engines keep
  themselves under it: VMA queries every frame and estimates between queries from its own
  allocations; UE and Unity size a texture pool and reduce mips until the plan fits.
- **Measured on ref-1** (our `Allocator::budget`, which also counts its own blocks):

  | | device-local heaps | budget | other heaps |
  |---|---:|---:|---|
  | RTX 4060 Ti | 15.73 GiB | 14.98 GiB | system memory 29.79 GiB |
  | Radeon iGPU | 22.36 GiB + 256 MiB (BAR) | 21.24 GiB + 243 MiB | system memory 11.18 GiB |

  The Radeon's "device-local" heap is not the 2 GB carve-out: the AMD driver exposes the
  carve-out plus most of the shared system memory as device-local on this APU. Its budget
  is therefore mostly memory the CPU side of the game also needs; an integrated GPU must
  take a smaller share of it (ADR 0076: half by default, 90 % on discrete GPUs).
- **Accounting matches the OS:** allocating 48 textures of 16 MiB moved the OS usage by
  +992 MiB on both GPUs, exactly the allocator's own count (`tests/memory.rs`,
  `budget_accounting_follows_the_os_usage`).
- **Priorities** (`VK_EXT_memory_priority`, `VK_EXT_pageable_device_local_memory` with
  `vkSetDeviceMemoryPriorityEXT`): both GPUs expose both. They tell the OS which
  allocations to page out first when the process is over budget. Our allocator shares
  blocks between resources of one memory type, so priorities would need blocks per
  priority class (streamed textures low, render targets high). Not done: the budget
  keeps the game under the OS budget, where priorities change nothing; they matter when
  something else takes memory faster than streaming can evict. Deferred (ADR 0076).

## Which texture levels are needed

### Screen-size estimates (UE, Unity)

- UE computes a "texel factor" per mesh at cook time (UV area against world area per
  triangle), and at run time the wanted mip of each texture of each component from its
  bounds, the view's distance and screen size. Unity's mipmap streaming does the same
  with a mesh UV distribution metric. Cheap, works before anything is drawn, and can
  prefetch for objects about to come into view.
- But it estimates: the sampled level depends on the pixel's UV derivatives (tiling,
  stretching, grazing angles, the sampler's anisotropy and LOD bias, the upscaler's
  bias). UE corrects it with per-component "streaming distance multipliers" and a build
  step ("Build Texture Streaming") that walks the materials.

### GPU feedback (virtual texturing, sampler feedback)

- id Tech 5/6 and UE's virtual texturing render a **feedback buffer** at a fraction of the
  screen (UE: 1/16 per side by default, a different pixel of each block every frame):
  each pixel writes the page and mip it needs; the CPU reads it back a few frames later
  and loads what is missing. Exact for what was drawn, blind to what was not.
- D3D12 **sampler feedback** records the same in hardware (MinMip and MipRegionUsed
  maps per texture). There is no Vulkan equivalent; we use none of it.
- **Our case is simpler than virtual texturing**: whole mip levels of whole textures, not
  pages. The finest level a texture is sampled at is all we need, and every texture of
  a material uses the mesh's one set of coordinates (ADR 0040), so **one value per
  material slot** carries it: the finest level in texture-coordinate units (a texture of
  size S is sampled at that plus log2 S). A few bits per material, kept with one atomic
  minimum per wave.

### Where to write it: the register rule

- Writing feedback in the forward shading would add code, and registers, to every
  shading pipeline (CLAUDE.md §5: 64 → 72 VGPRs cost 9–13 % on the Radeon), or a variant
  of each. Views that do not stream would pay, or the permutations double.
- A **separate pass** (ours) draws the view's culled commands again, after its forward
  passes, into a depth target of its own at 1/8 per side, with `depthVertexMasked` and an
  8-VGPR fragment shader. No existing SPIR-V changed (the Radeon's forward variants stay
  at 48/64/64/80 VGPRs, measured). Its cost is the vertex work of the visible instances;
  drawing a quarter of the commands per pass (rotating) brought it from 204 to 48 µs a
  frame on the Radeon and 15 to 10 µs on the RTX for Sponza, the same memory resident.
- The level is computed like the sampler: from the coordinates' derivatives, the shorter
  axis but at most 16 times finer than the longer (16× anisotropic), plus the
  materials' LOD bias, minus log2 of the divisor. Verified analytically (quads at known
  levels: within 0.004 levels) and against what the hardware sampled (the level asked
  for and the next; NVIDIA's "trilinear optimization" reaches the coarser level halfway
  where AMD blends linearly).

## Fitting a budget and degrading gracefully

- UE: when the plan does not fit the pool, drop one mip at a time from the least
  important textures until it fits; keep visible mips first, then small character
  textures, drop non-visible mips least-recently-seen first. Unity: reduce every
  texture's mip until it fits, with per-texture priority offsets.
- Ours, per streaming client and frame: required levels (a texture's tail, a mesh's
  coarsest level) always; visible textures at their sampled level; when that does not
  fit, a **common bias** (every visible texture one level coarser, then two) and then a
  level back for textures covering the most pixels while room remains: the picture
  degrades evenly, finest levels first. Levels resident beyond the plan stay cached while
  there is room (most recently seen first) and are evicted only under pressure.
- **Hitches** come from doing too much in one frame: the texture pool records at most a
  byte budget of copies and uploads per frame (one larger level alone); mesh levels are
  decoded on the streaming thread, and only a vertex budget of them is copied per frame.
- **Churn**: a texture whose sampled level flickers across a boundary, or a plan at the
  grant's edge, would be evicted and read again over and over. Measured on a Sponza
  flight in a 24 MiB pool: 1,005 MiB read for 91 MiB of textures. Hysteresis (coarser
  only after 30 frames), keeping the finer levels with the textures that hold them, and
  a 1/32 margin for levels that must be read: 141 MiB.

## Mesh detail levels

- Our levels (ADR 0053) are index lists over one vertex array. meshoptimizer's advice
  for such chains is to order the vertices by first use from the coarsest level: every
  level then uses a **prefix** of the vertices. Stored that way (pack version 3), a mesh
  streams as its coarsest level (its prefix and indices) plus, per finer level, the
  vertices it adds and its indices.
- Growing a mesh copies its vertex prefix into a larger range of the geometry pool on the
  GPU (a buffer copy within the pool's buffers) and uploads the new vertices; eviction
  copies the shorter prefix into a smaller range. Levels not resident draw the finest
  resident one: their entries in the mesh's level table repeat its indices, so the
  culling pass, which picks each instance's level by screen size, is unchanged.
- **No GPU feedback for meshes:** the culling pass's choice is a deterministic function of
  the instance's box, transform and the camera (projected error ≤ a pixel), so the CPU
  computes exactly what the GPU will pick (`cull::choose_lod`, shared with the renderer's
  `lod_scale`). Unlike a texture's sampled level, nothing per pixel is unknown.
- **Crossfades and dithering** (UE's dithered LOD transitions, Unity's LOD crossfade):
  ADR 0053 found 1-pixel pops hard to see and smoothed by TAA; streaming does not change
  that (a level is drawn only when resident). Not done.

## iGPU and discrete

- On the Radeon the device-local heap is system memory: textures and meshes compete with
  the CPU side of the game, and uploads cost memory bandwidth the GPU shares. The default
  share is half the budget; the per-frame upload budget matters more there (the 2-CU
  GPU's copies of 8 MiB take ~0.3 ms).
- On the RTX the budget is the card's VRAM minus what the OS and other processes hold.

## Patents (checked 2026-10-02; a search, not legal advice)

| Patent | Holder | Claims, in short | Our design |
|---|---|---|---|
| US10748241, US11195250, US11893656, EP3216011, EP3016075 "Prediction system for texture streaming" | Graphine NV (Unity) | a **plurality of predictor units**, each applying a different prediction scheme to predict a set of sub-regions (tiles) of streamable objects, and a streaming manager deriving a working set from those sets | **one** source of wanted levels: measured feedback. No camera prediction, and the screen-size estimate is **not** combined with it (the brief's "combined with screen-size heuristics" was dropped for this reason). Distance and pixels only order work. Question for the team below |
| US8587602 "GPU texture tile detail control" | Microsoft Technology Licensing | the texture unit clamps the LOD to resident tiles (at most one coarser) and reports feedback to the shader | not used: our views cover resident levels only; no per-tile clamp in hardware or shader |
| US12412332 "Native sampler feedback technology" | not checked | `WriteSamplerFeedback` operations dispatching work for regions a sample would access | not used: feedback is computed by our own shader from derivatives, per material, in a separate pass |
| US10388058 "Texture residency hardware enhancements" | not checked | residency maps sampled by the hardware, a feedback loop updating them | not used: no residency map; whole images per residency |
| US9734598 "Engine for streaming virtual textures" | — | a DMA/decoder engine feeding decompressed textures to a texture buffer | not used: CPU decompression on a streaming thread (ADR 0056/0057) |
| US9704213 "Texture element streaming" | Qualcomm (expired, fees) | progressive stages of texture elements sent from a source device to a sink | not relevant |

Rendering a feedback buffer itself is old (van Waveren's 2012 paper describes id Tech 5's,
shipped 2011; Sean Barrett's "Sparse Virtual Textures", GDC 2008). We found no patent on
per-material minimum-level feedback; we report it under the ADR's questions all the same.

## What we take and what we leave

- Take: the OS budget with a share for the game, one owner handing out grants class by
  class (required, visible, cached); feedback measured by a separate low-resolution pass
  with rotating slices; levels from the coarsest up with per-frame byte limits; LRU
  caching of unseen levels; vertex-prefix mesh levels; the culling rule on the CPU.
- Leave (for now): memory priorities; sparse residency (eviction by copy is cheap enough:
  Sponza's moves cost a few hundred µs on the Radeon at most per frame, within the frame
  budget); screen-size prefetch (patent above); LOD crossfades; GPU decompression.
