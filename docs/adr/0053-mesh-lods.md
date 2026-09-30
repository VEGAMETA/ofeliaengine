# 0053. Mesh detail levels: meshoptimizer at import, chosen per instance by the culling pass

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/mesh-lods.md](../research/mesh-lods.md)
- Related: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU culling, batches),
  [ADR 0039](0039-geometry-pool.md) (geometry pool)

## Context

The forward renderer draws every instance with its full mesh, however small it is on
screen. A field of 16k-triangle spheres costs 17.8 ms of forward shading on the RTX
4060 Ti and 239 ms on the Radeon iGPU, nearly all of it for triangles smaller than a
pixel. The team asked for LODs, with impostors as the farthest level later.

## Options

### Making the levels

1. **meshoptimizer's simplifier** through the `meshopt` crate (chosen): edge collapses
   minimizing a quadric error, a reported geometric error per level, index lists over the
   original vertices (no new vertex data); MIT OR Apache-2.0, builds the vendored C++ with
   `cc`.
2. **Our own simplifier:** no dependency, but a quadric simplifier that keeps borders and
   seams well is weeks of work; meshoptimizer is the industry reference.
3. **Artist-made levels only:** full control, but every asset needs them; the engine should
   make them when the asset has none.

### Choosing the level

1. **On the CPU per instance each frame:** simple, but O(instances) CPU per frame and it
   splits batches.
2. **In the GPU culling pass** (chosen): the pass already reads each instance's box; it
   picks the level and appends the instance to that level's draw command.

### The criterion

1. Distance bands per mesh: need tuning per asset and per resolution.
2. **Projected geometric error** (chosen): a level is used when its error (object units) ×
   the instance's scale × pixels per unit at distance 1 ≤ the box's nearest distance, for
   a threshold of 1 pixel. It adapts to resolution and field of view by itself.

## Numbers

`draw_bench --sphere 64` (16,384 triangles; levels of 8,192 / 4,095 / 2,047 triangles),
10,000 instances, frustum culling, forward µs:

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| overview, full meshes | 17,774 | 238,589 |
| **overview, LODs** | **2,015** | **34,364** |
| inside the field, full | 2,959 | 51,070 |
| **inside the field, LODs** | **370** | **8,815** |

Sponza (`scene_bench --lods`): levels made in 155 ms at load (218k triangles); RTX shadow
174 → 145 µs, forward 341 → 301 µs; the image differs by PSNR 46.5 dB (0.1% of pixels by
more than 15/255, on thin alpha-tested foliage and curtains).

Meshes without levels: RTX culling unchanged (11.7–24.8 µs at 100k cubes, was
12.4–26.6); Radeon culling 10–16% slower (121 → 140 µs, 299 → 329 µs, 590 → 635 µs):
the larger batch record and the level test per instance.

## Decision

- **`engine_assets::generate_lods(positions, indices, &LodSettings)`**: up to 3 levels,
  each about half the previous (from the full mesh each time, so errors are against it),
  at least 64 triangles, errors up to 5% of the mesh's size, stopping when a step saves
  under 15%; indices reordered for the vertex cache. Errors in object units.
- **`GeometryPool::add_mesh_with_lods(&MeshData, &[LodIndices])`**: up to `MAX_LODS` (4)
  levels in one index range over the same vertices; `MeshInfo::lods`, `lod_count`.
- **Draw commands**: one per level of each batch, a batch's levels adjacent and batches in
  state order (each pipeline state is still one indirect draw); each command has its own
  instance slots (in draw order: without LODs a draw's slot is its index, as before).
- **The culling pass** picks each instance's level (coarsest passing the rule) and appends
  it to that level's command, in every phase and for every shadow cascade (shadows use the
  camera's level).
- **`ForwardRenderer::set_lod_threshold(pixels)`**, default 1; 0 draws full meshes;
  orthographic views draw full meshes.
- `ForwardPasses::draw_commands` gives the commands per list.

## Consequences

- High-poly content costs what its screen size warrants: 6–9× less forward shading in
  the sphere field.
- Levels pop when they switch; at a 1-pixel threshold it is hard to see, and TAA smooths
  it. Dithered cross-fades are a follow-up if needed.
- Level generation costs load time (155 ms for Sponza); it belongs in the derived data
  cache next, with the textures.
- Next:
  - impostors (octahedral) as the farthest level for dense, distant content;
  - LODs in the glTF import path and the derived data cache;
  - attribute-aware simplification (normals, UVs) when artifacts show on hard edges;
  - recovering the Radeon's culling cost for meshes without levels.
