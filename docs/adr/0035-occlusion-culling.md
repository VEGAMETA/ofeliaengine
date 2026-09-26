# 0035. Two-phase occlusion culling

- Status: Accepted
- Date: 2026-09-26
- Builds on: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU frustum culling and
  indirect instanced draws)
- Research: [docs/research/gpu-driven-rendering.md](../research/gpu-driven-rendering.md)

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
   - One graph pass records one 8×8-thread dispatch per level, with a memory barrier
     between levels.
   - The last row and column repeat on odd sizes, so every texel covers its whole block
     (conservative).
4. **cull late:** in the frustum and not hidden.
   - The test: project the instance's world box (8 corners, from the same Arvo box as
     frustum culling) to a screen rectangle and its nearest depth.
   - Pick the level where the rectangle spans at most 2×2 texels (`2^level ≥ span` in
     pixels).
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

This can move to an image with a single-pass downsampler when profiling says the pyramid
matters.

**API.** `ForwardRenderer::add_passes` now takes a `ForwardView`: camera, light, targets,
clear color, and the optional occlusion state. This replaces six positional parameters.

## Measurements

**Correctness** (`tests/occlusion.rs`, lavapipe, synchronization validation). A wall fills
the view, with 200 cubes behind it and 20 in front of it. The counts are instances drawn
early / late:

| Frame | Early | Late | Why |
|---|---:|---:|---|
| 1 | 0 | 221 | Nothing was visible yet |
| 2 | 221 | 0 | Everything visible last frame is drawn early |
| 3 | 21 | 0 | The wall and the cubes in front: the 200 behind it are skipped |
| after `reset` | 0 | 221 | Same as frame 1 |

Every frame is pixel-identical to frustum culling alone. 0 validation errors.

**Cost and gain** (`examples/draw_bench.rs`, 1920×1080, lavapipe: a CPU rasterizer, **not
reference**, so only the proportions mean something):

| Scene | Instances | Frustum only: drawn / forward ms | + occlusion: drawn / forward ms | Occlusion overhead ms (cull + pyramid) |
|---|---:|---:|---:|---:|
| City blocks (4 m walls every 10 m, camera in a block) | 10,042 | 1,769 / 29.2 | 73 / 19.5 | +0.17 + 1.27 |
| Inside the open field (nothing hides anything) | 10,000 | 1,735 / 17.9 | 1,735 / 18.7 | +0.37 + 1.30 |

On the reference GPUs the GPU workflow reports the same table. Those numbers decide the
default use (below) and go into `docs/perf/history.md`.

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
- **Small, far objects** with a 1–2 pixel rectangle use level 1 (2×2 pixels), which is
  exact enough. The pyramid never goes below level 1, so there is no full-resolution
  copy.
