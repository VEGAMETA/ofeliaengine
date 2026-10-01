# 0035. Two-phase occlusion culling

- Status: Accepted
- Date: 2026-09-26
- Builds on: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU frustum culling and
  indirect instanced draws)
- Research: [docs/research/gpu-driven-rendering.md](../research/gpu-driven-rendering.md)
- Extended by [ADR 0064](0064-gpu-scene-extraction-runtime.md): on the GPU scene path
  instances are known by stable slots, so the order caveat below does not apply there.

## Context

After ADR 0034 the GPU draws everything in the view frustum. In dense scenes (cities,
interiors, forests) most of that is hidden behind nearer geometry and still costs vertex
work, rasterization and depth tests. CLAUDE.md §6 asks for GPU culling with
"frustum + HiZ occlusion".

## Options

1. **Occlusion queries** (`vkCmdBeginQuery` per object, results read a frame later).
   - One query per object, and the CPU reads results back.
   - Latency causes popping, and the CPU cost grows with the object count.
2. **Hierarchical depth from the previous frame, reprojected.**
   - One pass, but the reprojection has holes.
   - Newly visible objects appear a frame late (popping). Hacks can hide this, but the
     result is not exact.
3. **Depth prepass of chosen occluders.** Needs content authoring (which objects are
   occluders) and draws them twice.
4. **Two-phase hierarchical-Z** (Haar and Aaltonen, SIGGRAPH 2015; Nanite; niagara)
   (chosen).
   - Draw what was visible last frame.
   - Build a depth pyramid from that depth.
   - Test everything against it, and draw what the first phase missed.
   - Nothing visible is ever missing. A wrong guess (a camera cut, a changed draw list)
     only costs time.

## Decision

**State.** An `Occlusion` object per view, created by the caller with a fixed capacity.
- It holds one visibility flag per instance (4 bytes, in device memory), written by the
  previous frame's late phase.
- Instances are known by their index in the draw list. Instances beyond the capacity
  always wait for the late phase.
- `reset()` forgets everything after a camera cut: the next frame tests every instance
  late.
- The flags are cleared with `vkCmdFillBuffer` on first use and after a reset.

**Frame with occlusion (`ForwardView::occlusion = Some(..)`):**

1. **cull early:** in the frustum and visible last frame. The commands are in the first
   half of the draw command buffer; the visible list is in the first half of its buffer.
2. **forward early:** draws them. Color and depth are cleared (or loaded), and depth is
   stored.
3. **hiz:** a depth pyramid in a graph buffer.
   - Level `k` holds the farthest depth of every `2^k × 2^k` pixel block, and is
     `⌈w / 2^k⌉ × ⌈h / 2^k⌉` in size.
   - Levels are built up to 1×1.
   - Level 1 reads the depth target through the bindless sampled index; each further
     level reduces the one below.
   - *Revised 2026-09-27* (`engine_render::hiz`): every workgroup reduces a 64 × 64 tile
     of the level below through group shared memory, up to six levels per dispatch.
     The first dispatch reads the depth and writes levels 1-2, the next ones six levels
     each: three dispatches at 1080p instead of eleven. Level 1 is computed but not
     stored (level 2 is the finest), unless it is the whole pyramid. The per-dispatch
     level offsets are indexed with constants (a run-time index into the push constants
     made the Radeon's compiler copy the array per thread). `tests/hiz.rs` compares the
     pyramid bit for bit with a CPU reference on seven sizes, 1 × 1 to 8192 × 3.
   - *First version:* one 8×8-thread dispatch per level, with a memory barrier between
     levels.
   - The last row and column repeat on odd sizes, so every texel covers its whole block
     (conservative).
4. **cull late:** in the frustum and not hidden.
   - The test: project the instance's world box (8 corners, from the same Arvo box as
     frustum culling) to a screen rectangle and its nearest depth.
   - Pick the level where the rectangle spans at most 2×2 texels (`2^level ≥ span` in
     pixels), never finer than the finest stored level (2).
   - The instance is hidden if its nearest depth is farther than the farthest of those 4
     texels by more than a margin of 1e-5 (NDC depth).
   - The margin: a surface lying on the box's face compares equal up to rounding (the
     rasterizer and the cull shader project differently). On ref-1's AMD iGPU a wall
     filling the view came out hidden behind its own depth, and the test caught it. The
     margin costs ≈1 mm of culling at 7 m and ≈20 cm at 100 m with the near plane at
     0.5 m; it grows with distance² / near.
   - A box with a corner behind the camera, or reaching the near plane, is kept.
   - The pass writes each instance's flag for the next frame. It appends visible
     instances that were not drawn early to the second halves of the buffers.
5. **forward late:** draws them. Color and depth are loaded.

Without an `Occlusion` state the frame is ADR 0034's single phase.
`set_frustum_culling(false)` disables both kinds of culling.

**Why a buffer pyramid, not an image with a min/max reduction sampler (niagara):**
- the graph gives passes one view per texture, and per-mip storage views would need
  graph support;
- the buffer is transient and aliased like any other graph buffer;
- the cull shader fetches 4 texels and takes the max, which a reduction sampler would do
  in one fetch;
- `samplerFilterMinmax` stays optional.

Profiling did say it mattered (2026-09-27, below); the buffer stayed, with a fused
reduction instead of an image.

**API.** `ForwardRenderer::add_passes` now takes a `ForwardView`: camera, light, targets,
clear color, and the optional occlusion state. This replaces six positional parameters.

## Measurements

**Correctness** (`tests/occlusion.rs`, synchronization validation; lavapipe, and on ref-1
the RTX 4060 Ti and the Radeon iGPU). A wall fills the view, with 200 cubes behind it and
20 in front of it. The counts are instances drawn early / late:

| Frame | Early | Late | Why |
|---|---:|---:|---|
| 1 | 0 | 221 | Nothing was visible yet |
| 2 | 221 | 0 | Everything visible last frame is drawn early |
| 3 | 21 | 0 | The wall and the cubes in front: the 200 behind it are skipped |
| after `reset` | 0 | 221 | Same as frame 1 |

Every frame is pixel-identical to frustum culling alone. 0 validation errors. On the
Radeon the first version drew (20, 201) in frame 3: the wall hid behind its own depth
(see the margin above).

**Cost and gain** (`examples/draw_bench.rs`, 1920×1080, lavapipe: a CPU rasterizer, **not
reference**, so only the proportions mean something):

| Scene | Instances | Frustum only: drawn / forward ms | + occlusion: drawn / forward ms | Occlusion overhead ms (cull + pyramid) |
|---|---:|---:|---:|---:|
| City blocks (4 m walls every 10 m, camera in a block) | 10,042 | 1,769 / 29.2 | 73 / 19.5 | +0.17 + 1.27 |
| Inside the open field (nothing hides anything) | 10,000 | 1,735 / 17.9 | 1,735 / 18.7 | +0.37 + 1.30 |

**ref-1** (release, 1920×1080 HDR, shadow map 2048²; µs; "total" is cull + pyramid +
forward, the shadow pass is the same in both):

| GPU | Scene | Instances | Frustum only: drawn / forward / total | + occlusion: drawn / forward / cull / pyramid / total |
|---|---|---:|---:|---:|
| RTX 4060 Ti | City blocks | 10,042 | 1,769 / 67 / 71 | 73 / 65 / 7.6 / 13.3 / 86 |
| RTX 4060 Ti | City blocks | 100,128 | 1,827 / 83 / 93 | 122 / 69 / 18.4 / 13.3 / 101 |
| RTX 4060 Ti | Open field | 100,000 | 1,741 / 48 / 58 | 1,741 / 49 / 18.0 / 13.3 / 80 |
| Radeon iGPU | City blocks | 10,042 | 1,769 / 1,809 / 1,830 | 73 / 1,401 / 47.1 / 171.4 / 1,620 |
| Radeon iGPU | City blocks | 1,014 | 646 / 1,647 / 1,651 | 49 / 1,339 / 9.7 / 170.8 / 1,520 |
| Radeon iGPU | Open field | 10,000 | 1,735 / 1,056 / 1,074 | 1,735 / 1,072 / 45.8 / 160.8 / 1,279 |

With the revised pyramid (the first version's pyramid: RTX 23.6-24.6 µs, Radeon
208.8-220.1 µs).

- With cheap geometry (12-triangle cubes) the RTX draws the 1,700 frustum survivors in
  under 0.1 ms, so skipping 95% of them saves less than the pyramid and the second cull
  cost: occlusion is a loss there (+9-38%).
- On the iGPU the skipped draws are worth 0.3-0.4 ms, and occlusion wins 8-11% in the
  city.
- In the open field it is pure overhead on both.
- **The pyramid** (Radeon, 10k instances, µs): one dispatch per level 189-222 → fused
  levels with run-time push constant indexing 281-322 → constant indexing 187-224 →
  levels 1-2 first, then six per dispatch 173-211 → level 1 not stored 146-171. The
  first dispatch alone is 104-142 µs: it reads the 8.3 MB depth target at about the
  memory's 83 GB/s (DDR5-5200, two channels), a floor short of reading less depth. The
  RTX: 23.6 → 13.3 µs.

## Patent check (2026-10-01)

US 10,672,182 B2 (Microsoft, "Compact visibility state for GPUs compatible with hardware
instancing", filed 2018-04-19, active until 2038-05-25) claims a primitive visibility buffer
holding **one visibility bit per primitive (triangle) of each instance**, consulted to render or
skip primitives. This ADR keeps **one flag per instance** from the previous frame, and that idea
predates the filing (Haar and Aaltonen, "GPU-Driven Rendering Pipelines", SIGGRAPH 2015).
Assessed as low risk; not legal advice. Future meshlet / visibility-buffer work must not keep
persistent per-triangle visibility bits without a fresh check.

## Consequences

- **Opt-in per view.**
  - Scenes where things hide things (cities, interiors, dense forests) gain the most.
  - Open or top-down views pay the pyramid and a second culling pass for nothing.
  - The editor and games choose per view. A heuristic (enable when last frame's late
    phase skipped enough) can follow the ref-1 numbers.
- **The forward pass is split in two.**
  - Two render-pass instances and two indirect draws instead of one.
  - The depth target must be stored between them, which costs bandwidth on tiled GPUs
    (not a target) and nothing measurable on desktop GPUs.
- **The draw list's order matters for speed, not for correctness.** The dev guide says
  to keep it stable. A persistent GPU scene (next) gives instances stable slots, so this
  stops depending on the caller.
- **Shadow casters are not occlusion-culled.** The shadow map needs every caster in the
  light's view. Culling them against a pyramid of the shadow map is part of the cascaded
  shadow map work.
- **Small, far objects** with a rectangle of up to 4 pixels use level 2 (4 × 4 pixels),
  slightly more conservative than level 1 was; the draw counts of the benchmark did not
  change. There is no full-resolution copy.
