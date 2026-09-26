# 0015. 2D renderer: bindless quad batching, SDF shapes, deterministic order

- Status: Accepted
- Date: 2026-09-26
- Builds on: [ADR 0011](0011-render-graph.md), [ADR 0013](0013-bindless-and-pipelines.md),
  [ADR 0014](0014-pbr-and-shadows.md) (upload ring)
- Research: [docs/research/renderer-2d.md](../research/renderer-2d.md)

## Context

CLAUDE.md §17 step 2 continues with the 2D batcher, before viewports. §6 asks for:
- batching through bindless textures and instancing;
- sprites, vector primitives through SDF (lines, rounded shapes, outlines);
- sorting by layers, y-sort and a user key, deterministic.

It has to meet §5:
- zero allocations per frame;
- no locks;
- measured costs.

## Options

### Batching

1. **Break batches on texture change and rely on atlases** (Godot, Bevy). Batch count
   depends on content order and atlas packing.
2. **Reorder for batching** (Bevy sorts equal z by image). Changes how overlapping sprites
   look.
3. **Bindless: every quad carries its texture and sampler index** (chosen):
   - batches break only on the blend pipeline and the clip rectangle;
   - draw order is never changed.

### Shapes

1. **Geometry on the CPU** (Wicked's triangle fans, Godot's polygons and styleboxes).
   Costs vertices, and the edges are aliased or need MSAA.
2. **A separate SDF pipeline for UI shapes** (Bevy UI). Splits batches between sprites and
   shapes.
3. **One quad primitive with an optional SDF edge** (chosen):
   - a rounded rectangle with per-quadrant radii and an outline thickness covers
     rectangles, circles, capsule lines and outlines;
   - sprites and shapes share the pipeline and the batch.

### Vertex generation

All three are implemented, selected by `QuadMode`, and measured:
1. **Indexed** (chosen): a shared 16-bit index buffer `0 1 2 2 1 3 + 4k` (16384 quads,
   192 KiB). The draw's vertex offset selects the first quad. 4 vertex invocations per
   quad.
2. **List**: no index buffer, 6 invocations per quad.
3. **Instanced**: one 6-vertex instance per quad. AMD's guidance is that tiny instances
   pack poorly into vertex waves.

### Sorting and upload

1. `slice::sort` (stable): allocates scratch memory, so it breaks the zero-allocation rule.
2. `sort_unstable` on `(key, index)`: no allocation, deterministic because indices are
   unique, but slower on large inputs.
3. **LSD radix sort, 11-bit digits, trivial passes skipped, in-order input detected**
   (chosen).

Moving the sorted instances to the GPU:
1. Gather 64-byte instances in draw order: random reads.
2. **Copy instances in submission order plus a 4-byte draw-order array** (chosen). The
   vertex shader reads `instances[order[i]]`; in-order canvases skip the array.

## Decision

- **Crate `engine_render2d`:**
  - `Canvas` records quads. `Quad` has constructors `sprite`, `rect`, `rounded_rect`,
    `circle`, `line`, and builders `color`, `uv`, `sampler`, `blend`, `order`, `rotated`,
    `outline`.
  - `Order { layer, key }` with `Order::y_sorted`; `set_clip`.
  - `Renderer2d::add_pass` declares one graph pass.
  - `Camera2d` gives a y-up world view; `View2d::pixels` gives y-down pixel space.
- **Instance: 64 bytes**:
  - half-extent axes, center;
  - color as `f16×4` (linear, straight alpha, HDR tints allowed);
  - UV rect;
  - bindless texture index (or none);
  - flags: sampler (3 bits), SDF, additive, and outline thickness as `f16`;
  - corner radii as `f16×4`.
- **Output is premultiplied.** Additive quads output alpha 0, so they share the alpha
  pipeline. Multiply has its own pipeline (`Blend::Premultiplied`, `Blend::Multiply` added
  to `engine_rhi`).
- **Antialiasing:**
  - width = `sqrt((|ddx p|² + |ddy p|²) / 2)` of the affine local position (no corner
    artifacts, any zoom);
  - SDF quads grow by one pixel (`View2d::units_per_pixel`);
  - sprites keep hard, pixel-exact edges.
- **Order:** layer, then key (lower first; `y_sorted` gives higher y first), then
  submission order. Batches: runs of equal (pipeline, clip).
- **Textures (`engine_render::texture::GpuTexture`):**
  - device-local, registered in the bindless set;
  - uploaded through the ring by a graph pass with its own barriers, ending in
    `READ_ONLY_OPTIMAL` for all stages;
  - textures sampled through bindless are never written outside that pass, so no
    per-frame tracking is needed.
- **Half-float conversion:**
  - `engine_core::half` uses F16C by runtime dispatch, else a scalar path;
  - shaders decode halves with integer and float operations, because Slang's `f16tof32`
    requires the `shaderFloat16` and `shaderInt16` features.

## Measurements

### CPU, container

Intel Xeon @ 2.8 GHz (4 vCPUs, shared), `-C target-cpu=x86-64-v2`, release;
`cargo bench -p engine_render2d --bench canvas`; ns per quad:

| quads | record | prepare, in order | prepare, y-sorted (radix) | std `sort_unstable` | std `sort` (allocates) | gather 64 B | copy + order |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 000 | 11.4 | 2.8 | 13.5 | 12.7 | 12.2 | 3.1 | 1.6 |
| 10 000 | 11.3 | 2.7 | 11.4 | 20.6 | 19.8 | 3.2 | 2.3 |
| 100 000 | 12.8 | 2.7 | 14.2 | 30.2 | 18.0 | 7.7 | 4.4 |
| 1 000 000 | 18.2 | 2.9 | 20.7 | 37.8 | 25.8 | 26.2 | 7.5 |

- The radix sort:
  - is 1.8–2.1× faster than `sort_unstable` from 10 000 quads up;
  - matches the allocating stable sort without allocating.
- Copy + order beats the gather by 1.7× at 100 000 quads and 3.5× at 1 000 000.
- In-order canvases cost 2.7 ns per quad to prepare: the order check plus batching.
- Recording (about 90 instructions per quad under callgrind) is the largest CPU cost. See
  Consequences.

### `sprite_bench`, 100 000 y-sorted 8-pixel textured sprites

Lavapipe on the same container, 1920×1080:

- prepare + upload into the ring: 3.72 ms before these two changes, 2.83 ms after:
  - gather → copy + order;
  - 8-bit → 11-bit digits.
- Lavapipe's GPU times are CPU rasterization and do not transfer to GPUs. Relative to
  indexed:
  - list is 2.1×;
  - instanced is 1.1–1.2×.

### Correctness

`tests/render2d.rs` on lavapipe, validation plus synchronization validation, 0 errors:

- texture orientation, a disc, an outline ring, two overlapping additive quads (128 → 255),
  multiply (64 → 32), layer order, a capsule line, a clip rectangle: all exact;
- 68 partially covered pixels on the disc's rim;
- the list and instanced modes produce images identical to indexed;
- an in-order canvas (direct path, no order array) is checked separately.

`tests/render2d_zero_alloc.rs`: 100 frames of 6 500 quads (y-sorted, layers, clip, all
blend modes) through ring and graph make 0 heap allocations.

### Still to measure

- ref-1 GPU times for the three modes (NVIDIA and AMD) come from the GPU workflow's
  `sprite_bench` run on this PR. The mode choice is revisited if indexed loses on either
  vendor.

## Consequences

- **Draw order never depends on textures.** Scenes can use many textures without atlases.
  Atlases remain useful for memory and streaming, not for batching.
- **Adding a blend mode or custom material** means one more pipeline and one bit pattern in
  the batch state: custom Slang materials (CLAUDE.md §6) will extend `state` with a
  material slot.
- **SDF quads assume orthogonal axes** (no skew). Skewed shapes would need a general
  distance.
- **Recording cost scales with the number of quads on one thread.** At 1 000 000 quads,
  recording plus preparing costs about 40 ns per quad on this CPU. Next:
  - record canvases on several jobs and merge before sorting;
  - avoid re-recording static content (retained chunks for tilemaps).
- **Next 2D steps:** 9-slice, tilemaps (chunked, GPU culling), particles, 2D lights and
  normal maps, MSDF text (`engine_text`), and viewports (the next step of §17).
