# 0039. Geometry pool: shared vertex and index buffers, 28-byte vertices

- Status: Accepted
- Date: 2026-09-27
- Builds on: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU culling and indirect
  draws)
- Research: [docs/research/meshes-and-materials.md](../research/meshes-and-materials.md)

## Context

Rendering real content (the team's priority: quality and speed on real scenes, not
cubes) needs meshes as artists make them:

- indexed, with shared vertices;
- with texture coordinates and tangents for materials.

Until now each mesh was its own host-visible buffer of 32-byte vertices (position and
normal as `float4`), drawn without an index buffer: three vertices per triangle. Every
measurement was taken on cubes.

## Options

**Storage:**

1. **A buffer per mesh, pulled through its device address** (as before).
   - Simple, but no index buffer can be shared by the draws of one indirect call.
   - A triangle list must then repeat shared vertices.
2. **One pool of vertices and one of indices, suballocated, growing by GPU copies**
   (Bevy's slabs, niagara) **(chosen)**.
   - One index buffer binding and one `vkCmdDrawIndexedIndirect` per pass; the draw's
     vertex offset selects the mesh.
   - Removal and growth need frame-aware retirement.

**Vertex layout:**

- **A (chosen):** position `f32 × 3` in its own stream (12 bytes) plus 16 bytes of
  attributes: normal and tangent octahedral at 16 bits per axis, texture coordinates
  `f32 × 2`.
- **B:** position and normal in one aligned 16-byte load, tangent and texture
  coordinates in 12 more.
- Quantized positions (box-relative 16-bit) and texture coordinates (range-relative
  16-bit) would save 10 more bytes. They crack shared edges between meshes and lose
  precision on tiled surfaces; not now.

## Measurements / arguments

ref-1, release. GPU µs, medians; 1920×1080 HDR, 2048² shadow map.

**Real scenes (`scene_bench`), the same meshes indexed in the pool vs. as a triangle
soup:**

| | vertices | pool | RTX shadow / forward | Radeon shadow / forward |
|---|---:|---:|---:|---:|
| Sponza (262k triangles), indexed | 192k | 8.1 MB | 67 / 52 | 774 / 1,308 |
| Sponza, triangle soup | 787k | 24.0 MB | 84 / 105 | 1,314 / 2,084 |
| FlightHelmet (95k triangles), indexed | 55k | 2.6 MB | 36 / 40 | 259 / 1,006 |
| FlightHelmet, triangle soup | 284k | 8.7 MB | 36 / 50 | 459 / 1,313 |

- Indexed: forward half as long on the RTX, and 37% (Sponza) to 44% (FlightHelmet
  shadows) less GPU time on the Radeon.
- Adding Sponza's meshes (encoding 192k vertices): 16 ms on the CPU.

**Cubes (`draw_bench`), before (main) → after:**

| | RTX shadow | RTX forward | Radeon shadow | Radeon forward |
|---|---:|---:|---:|---:|
| 10k, overview | 43 → 43 | 56 → 47 | 592 → 453 | 1,547 → 1,142 |
| 100k, overview | 244 → 264 | 309 → 269 | 5,806 → 4,620 | 9,451 → 5,951 |

- The one loss is the RTX shadow pass on 100k twelve-triangle cubes (+8%): with 24 unique
  vertices for 36 indices, indexing saves little and costs the index fetch. Real meshes
  share far more.
- Layout B changed nothing on the RTX (264 µs either way), so single aligned loads were
  not the cost. On the Radeon, where the shadow pass is bound by memory bandwidth, B was
  up to 7% slower (16 bytes per vertex instead of 12). A was kept.

**Correctness:**

- Octahedral normals and tangents at 16 bits per axis: at most 0.0025° of error
  (Cigolle's precise rounding, candidates compared in double precision).
- The pool's contents after growth, removal and reuse are read back and compared byte
  for byte (`tests/geometry.rs`, under synchronization validation).
- The culling and occlusion tests keep their exact instance counts and unchanged images
  on both GPUs.

## Decision

- **`engine_render::mesh::GeometryPool`:**
  - positions (12 bytes), attributes (16 bytes) and 32-bit indices in three device-local
    buffers, suballocated by `engine_rhi::RangeAllocator` (the TLSF of ADR 0010, now with
    a unit granule and growth);
  - buffers grow to twice their size (or what a mesh needs) by GPU copies;
  - removed meshes' ranges and replaced buffers are freed after their frames;
  - added meshes are copied through a staging buffer in the next `begin_frame`, which
    also imports the buffers into the frame's graph ([`GeometryFrame`]).
- **`DrawItem::mesh` is a `MeshId`;** the forward renderer draws with
  `vkCmdDrawIndexedIndirect` and pulls vertices by `SV_VulkanVertexID` (the vertex offset
  included).
- **Vertex encoding:** octahedral normal, octahedral tangent with the bitangent sign in
  bit 16, `f32` texture coordinates. Tangents follow glTF as exported: the bitangent
  `cross(n, t) · w` points towards decreasing v, like a normal map's green channel.
- The old per-mesh `Mesh` and the minimal `MeshRenderer` are gone.

## Consequences

- **Materials:** texture coordinates and tangents are in place for the next step
  (textures, normal maps).
- **Test scenes:** `scene_bench` measures real scenes from now on. Scenes are fetched
  into `test-assets/` by `tools/fetch-test-assets.ps1` and never committed.
- **meshoptimizer:** vertex cache and fetch order, meshlets and LODs still have to come
  from meshoptimizer (vendored C source).
- **Later measurements:** quantized positions and texture coordinates wait for them,
  like 16-bit indices for small meshes (one index type per indirect call).
- **Removal delays:** removing a mesh keeps its memory for the frames in flight;
  replacing a buffer keeps the old one as long.

[`GeometryFrame`]: ../../crates/engine_render/src/mesh.rs
