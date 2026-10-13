# Meshlets in the forward renderer: how to build, cull and draw them

Queue item 4, ADR 0085's option 1 (the owner, 2026-10-11): meshlets built at cook time and
culled per view on the GPU, drawn through indirect draws on GPUs without mesh shaders (the
Radeon iGPU) and through mesh shaders where they exist (the RTX). The earlier survey is
[visibility-buffer.md](visibility-buffer.md) (Wihlidal, The Forge, Nanite, Wicked, Bevy).
This note is about the implementation. Checked 2026-10-14.

## Sources

| Source | Version | License | What we read |
|---|---|---|---|
| meshoptimizer, README ("Mesh shading", "Clustered raytracing") and `clusterize` | 0.25 (`meshopt` 0.6.2, already a dependency) | MIT | sizes, cone weight, bounds, the cone test |
| Themaister, "Modernizing Granite's mesh rendering" (blog), and Granite | 2024-01-17 | MIT | culling meshlets without mesh shaders, measured on NVIDIA, RDNA2 and RDNA3 |
| zeux, niagara, `src/shaders/meshlet.task.glsl` | master, 2026-10-14 | MIT | frustum, cone and occlusion tests per meshlet; two-phase culling |
| Our ADR 0034 (GPU culling), 0035 (two-phase HiZ, the patent check), 0053 (detail levels), 0076 (mesh level streaming) | — | — | what meshlets plug into |

No code was copied.

## What they say

- **meshoptimizer:**
  - Sizes: 64 vertices and 126 triangles for NVIDIA; equal limits (64/64, 128/128) suit
    earlier AMD GPUs; 64/96 for real meshes. `max_triangles` must be divisible by 4 up to
    0.25.
  - `cone_weight`: 0.25 when cones are used.
  - `computeMeshletBounds` gives a sphere and a cone (apex, axis, cutoff). A meshlet facing
    away is rejected when `dot(normalize(apex − camera), axis) ≥ cutoff`.
  - `optimizeMeshlet` improves locality inside each meshlet.
  - Cull meshlets "less often than mesh shading": in task shaders or a separate compute
    dispatch.
- **Granite (measured):**
  - Culling: a compute pass culls meshlets (frustum, backface cone) and writes one indexed
    draw per visible meshlet into one multi-draw-indirect buffer (MDI).
  - RTX 3070, 63.6 M triangles: drawn whole 5.5 ms, per-object frustum 4.3 ms, meshlet MDI
    3.9 ms, mesh shaders 4.0 ms. With per-primitive culling 3.3 ms, with micro-triangle
    rejection 1.9 ms.
  - RDNA2 (Steam Deck): frustum 27.9 ms; MDI 8.9 ms; mesh shaders 9.3 ms at best (32/32).
  - RDNA3: MDI 5.0 ms against mesh shaders 2.2–2.5 ms.
  - Task shaders lost on both vendors: an indirect count from compute instead.
  - Two-phase occlusion keeps one visibility bit per meshlet from frame to frame.
- **niagara:**
  - Tests per meshlet in a task shader: the cone with the camera at the view origin,
    frustum sides from symmetry, near and far, and then (late phase) the sphere's screen
    box against the depth pyramid's level for its size.
  - Visible meshlets are compacted through a shared counter.
  - Its two phases also keep a bit per meshlet from the previous frame.

## What we take

- **Build at cook time, per detail level** (`meshopt::build_meshlets`, 64 vertices and 124
  triangles, cone weight 0.25; inside each meshlet the vertex cache optimizer's order,
  measured below):
  - each level's index buffer is reordered meshlet by meshlet, with global indices, so a
    meshlet is a contiguous range of it;
  - the vertex order stays, so a coarser level still uses a prefix of the vertices
    (ADR 0076's streaming);
  - 64/124 suits the RTX's mesh shaders later and costs nothing on the indirect path,
    which ignores the vertex limit.
- **Per meshlet:** the sphere, the cone (axis and cutoff; the apex for the exact test), and
  the index range.
- **Without mesh shaders, Granite's MDI:** a compute pass emits one indexed draw per visible
  meshlet (one instance) into a per-state list with a count (`vkCmdDrawIndexedIndirectCount`,
  core 1.2). It is the fastest path Granite measured on RDNA2 and equal to mesh shaders on
  NVIDIA. Mesh shaders come second, on the RTX, for per-primitive culling.
- **A hybrid with today's instancing:**
  - instances whose mesh has few meshlets, or whose screen size is small, keep the
    instanced whole-mesh draws (one command per batch and level);
  - the meshlet path is for the instances where culling inside the mesh pays: large and
    near, partly off screen or occluded.
  - The threshold is to be measured.
- **Two phases without a bit per meshlet** (ADR 0035's patent note; Microsoft US 10,672,182
  claims a visibility bit per primitive):
  - the instance's flag from the previous frame stays the only persistent state;
  - the early phase culls the meshlets of instances visible last frame by frustum and cone;
  - the late phase culls those of newly visible instances by frustum, cone and the pyramid
    built from the early phase.
  - Granite's and niagara's per-meshlet bits are not taken.
- **The cone test only for uniformly scaled instances:** a non-uniform scale bends the cone.

## Measured at cook time (2026-10-14)

The `meshlet_stats` example of `engine_assets`, on Sponza, FlightHelmet, ClearcoatWicker
and MetalRoughSpheres. Cache figures are meshoptimizer's models (FIFO 16, NVIDIA-like
32/32/32, AMD-like 14/64/128), in vertices transformed per triangle. The GPU numbers are
in ADR 0085's addendum.

- **Fill:** at 64/124, Sponza's meshlets average 76 triangles: 99 % of the vertex limit, 61 %
  of the triangle limit. Its UV and normal seams split vertices. The other scenes fill 71–78 %.
- **Size and order:**
  - The input order does not matter: the same results from the file's order and from
    the vertex cache optimizer's.
  - The size barely moves the cache. 32/32, 64/64, 64/124, 128/124 and 255/124 are within
    ±1 % of each other (255/256 a little worse). Larger meshlets only mean fewer of them:
    Sponza 3,460 at 64/124, 2,246 at 128/124.
- **The order inside a meshlet is what counts:**
  - With `optimize_meshlet`'s order, a whole level transforms 4–18 % more vertices than in
    the vertex cache optimizer's order.
  - With the vertex cache optimizer run on each meshlet's local indices, 1–9 %. The cook
    uses that.
  - `optimize_meshlet` orders for mesh shaders' rasterization. The mesh shader path
    (step 3) builds its own vertex lists anyway.

## Open questions, for measurement

- The threshold between the instanced and the meshlet path (meshlet count, screen size).
- 64/124 against smaller meshlets on the Radeon (Granite's RDNA2 liked 32/32). Sponza fills
  only 61 % of the triangle limit. On the indexed path a larger vertex limit (128/124) would
  halve the draws, but it would not suit the RTX's mesh shaders.
- The cost of thousands of indirect draws on the iGPU's command processor.
- Whether the early phase should test meshlets against the previous frame's pyramid too
  (it needs no per-meshlet state).
