# 0085. Visibility buffer and meshlets: meshlets in the forward renderer first

- Status: Accepted (option 1 now, the owner, 2026-10-11)
- Date: 2026-10-11
- Research: [docs/research/visibility-buffer.md](../research/visibility-buffer.md)
- Related: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU culling),
  [ADR 0035](0035-occlusion-culling.md) (two-phase HiZ; the per-triangle visibility bits
  patent), [ADR 0045](0045-local-lights.md) (clustered forward+), [ADR 0053](0053-mesh-lods.md)
  (detail levels), [ADR 0058](0058-ambient-occlusion.md) (the register rule), queue items 4
  and 3b

## Context

Queue item 4 asks for a visibility buffer and meshlets, with this ADR first and no
persistent per-triangle visibility bits (Microsoft US 10,672,182, ADR 0035). The renderer
is clustered forward+, culled per instance on the GPU, one indirect draw per pipeline
state, shading features as entry points of their own. The Radeon iGPU's forward pass is
two thirds of its frame; it has no mesh shaders (driver 26.8.1); both GPUs have 64-bit
image atomics.

## Options

1. **Meshlets in the forward renderer:**
   - clusters of up to 64 vertices and 124 triangles built at cook time (meshoptimizer,
     already a dependency), with their bounds and normal cones, per detail level;
   - culled per view every frame on the GPU (frustum, cone, the HiZ pyramid of ADR 0035;
     nothing kept from frame to frame per triangle);
   - drawn through a compacted index buffer and the existing indirect draws on GPUs without
     mesh shaders (the iGPU), through task and mesh shaders where they exist (the RTX);
   - the shading is untouched; shadows and probes reuse the cluster culling.
2. **A visibility buffer:** IDs (instance, meshlet, triangle) rasterized into 32 or 64
   bits, then materials shaded in compute per class of tiles with analytic derivatives;
   transparent surfaces stay forward. It replaces the forward opaque shading, changes how
   shading permutations and custom materials (item 3c) work, and needs a per-sample path
   for MSAA (or keeps MSAA on the forward path).
3. **Nanite-like software rasterization** of small triangles with 64-bit atomics into the
   visibility buffer: only on top of option 2.

## Recommendation (to confirm)

Option 1 now: it gains where geometry is dense without touching the shading, works on both
GPUs, and its clusters are what options 2 and 3 build on. Option 2 decided with item 3c's
ADR (custom materials decide the shading's shape) and with the meshlets' numbers on both
GPUs. Option 3 only if dense geometry is still the bottleneck then. A fresh patent check
comes with each step (the Intel tiled-deferred application in the research note before
option 2).

## Questions for the owner

1. Meshlets first (option 1), with the visibility buffer decided together with item 3c?
2. Or the visibility buffer now, before custom materials?

## Decision

**Option 1 now** (the owner, 2026-10-11): meshlets in the forward renderer, clusters built
at cook time and culled per view on the GPU, drawn through compacted indices where mesh
shaders are missing (the iGPU) and through task and mesh shaders where they exist. The
visibility buffer is decided together with custom materials (queue item 3c).

## Consequences

- The cook gains clusters per detail level (meshoptimizer), the pack a format addition.
- Culling per cluster on both GPUs; numbers on both before the visibility buffer's
  decision.

## Addendum (2026-10-14): the implementation plan

Research: [docs/research/meshlets.md](../research/meshlets.md) (meshoptimizer 0.25, Granite's
measurements, niagara's culling).

- **Step 1, the cook** (done 2026-10-14, `engine_assets::meshlet`):
  - Meshlets per detail level: 64 vertices and 124 triangles, cone weight 0.25. Skinned
    meshes get none (their bounds move).
  - Each level's index buffer is reordered meshlet by meshlet, with global indices. The
    vertex order stays, so streaming by level (ADR 0076) holds.
  - Inside a meshlet, the triangles take the vertex cache optimizer's order instead of
    `optimize_meshlet`'s: the instanced draws of whole levels read the same buffer (below).
  - A meshlet table per level: sphere, cone, first index, triangles (32 bytes each).
  - No new pack version: the tables end the mesh and mesh level entries, behind attribute
    bit 16. Engines from before read the same triangles, reordered, and never reach the
    tables. Readers check that a level's meshlets cover its indices, in order.
- **Step 2, culling and drawing on both GPUs:**
  - The geometry pool keeps the tables on the GPU.
  - Instances of meshes with enough meshlets (a threshold to measure) take the meshlet
    path. The cull pass emits one indexed draw per visible meshlet into a per-state list
    with a count (`vkCmdDrawIndexedIndirectCount`; Granite's fastest path on RDNA2, equal to
    mesh shaders on NVIDIA). The other instances keep today's instanced draws.
  - The two phases keep only the instance's flag from the previous frame. The early phase
    culls the meshlets of instances visible last frame by frustum and cone; the late phase
    culls those of newly visible instances by frustum, cone and the early phase's pyramid.
  - No visibility bit per meshlet (the patent note of ADR 0035).
- **Step 3, mesh shaders on the RTX:** the same culling's output through
  `vkCmdDrawMeshTasksIndirectCountEXT`, with per-primitive culling in the mesh shader, as
  separate entry points (the register rule).
- **Step 4, shadows:** the meshlets culled per cascade and per local light's view.
- **Measured at every step on both GPUs:**
  - Sponza and a dense scene (many instances of a high-polygon mesh);
  - the passes' times, the draws issued, the triangles drawn.

### Step 1, measured (2026-10-14)

- **Meshlets of the test scenes** (the `meshlet_stats` example): Sponza 3,460 for its
  262 k triangles (the full level) and 3,258 for its detail levels. They are 61 % full in
  triangles and 99 % in vertices: Sponza's seams make the 64-vertex limit bind first.
  FlightHelmet's are 71 % full, MetalRoughSpheres' 78 %.
- **The vertex cache of whole-level draws** (meshoptimizer's models, vertices transformed
  per triangle):
  - Against the vertex cache optimizer's order alone, meshlet order costs 1–9 %.
    With `optimize_meshlet`'s order inside the meshlets it cost 4–18 %, so that order is
    not taken.
  - Larger meshlets (128 or 255 vertices) do not change this. They only halve the
    meshlet count.
  - Against the file's order, which the cook kept until now for full levels: Sponza
    0.815 → 0.847 (FIFO 16), FlightHelmet 1.035 → 0.746, MetalRoughSpheres 0.846 →
    0.716.
- **GPU time, before any culling** (`scene_bench --prepass`, 1080p, 4 cascades, alternated
  runs with `--no-meshlets`):
  - RTX: Sponza shadow 160 → 161 µs, prepass 57 → 58, forward 233 → 234; FlightHelmet
    unchanged.
  - Radeon: Sponza shadow 2,858 → 2,887 µs (+1 %), prepass 1,038 → 1,033, forward 9,597 →
    9,522 (−0.8 %). FlightHelmet: shadow 504 → 439 µs (−13 %), prepass 220 → 213, forward
    1,949 → 1,904 (−2.3 %).
- **The cost of cooking:** Sponza's meshes take 1 → 17 ms on 16 threads (24 → 51 ms with
  detail levels). Its pack grows from 72.68 to 72.95 MB (+0.4 %).
- **Golden images:** unchanged on both GPUs.

