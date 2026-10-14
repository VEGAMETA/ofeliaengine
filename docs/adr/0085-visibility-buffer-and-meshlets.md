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

### Step 2, as built and measured (2026-10-14)

- **The pool.** A level's meshlets follow its indices in its index range, from the next
  32-byte boundary. Ranges, streaming by level, eviction and growth stay as they were.
  `MeshLod` gains `first_meshlet` and `meshlet_count`.
- **The pack.** A mesh level entry's meshlet count is in its `meta[5]`: the streaming
  thread sizes the level's decoded form by it.
- **Culling.**
  - `cullMainMeshlets`, a twin of `cullMain`, sends the instance to the meshlet cull's
    work list. That happens when it is visible in its phase, of a batch neither blended
    nor of a material type, its level has meshlets, and its pipeline state's lists have
    room for all of them (`MAX_MESHLET_DRAWS`, 2^17 a phase). Otherwise it keeps its
    instanced draw.
  - `cullMeshlets` is dispatched indirectly, one group per work item. Each meshlet's
    sphere is tested against the frustum. Its normal cone is tested too (single-sided
    batches, uniformly scaled instances, not meshoptimizer's "no cone" of axis 0 and
    cutoff 127). In occlusion culling's late phase, the depth pyramid is tested.
  - Each visible meshlet is one indexed draw of one instance in its state's list.
  - Nothing about a meshlet is kept between frames (ADR 0035's patent note).
- **Drawing.** Every forward-phase pass draws the lists after its state's instanced draws:
  the depth prepass, the shading, texture feedback. It uses
  `vkCmdDrawIndexedIndirectCount`, with the count written by the meshlet cull.
- **Only with a depth prepass.** Without one, the draws' GPU order undoes the
  front-to-back order. Sponza's shading without a prepass ran +15 % slower on the Radeon
  and +35–50 % on the RTX.
- **Needs** `drawIndirectCount` and `shaderInt64`, optional device features that both
  GPUs have. **Off by default:** `ForwardRenderer::set_meshlet_culling`.
- **Measured** (1920 × 1080, depth prepass, frustum culling; prepass + shading + culling
  µs, medians, off → on):

| scene | RTX 4060 Ti | Radeon iGPU |
|---|---|---|
| Sponza (`scene_bench --prepass`) | 297 → 313 (+5 %) | 10,598 → 10,319 (−2.6 %) |
| FlightHelmet | 80 → 92 (+15 %) | 2,166 → 2,263 (+4.5 %) |
| 10,000 spheres of 4,096 triangles, overview (`draw_bench --sphere 32 --prepass`) | 8,617 → 8,383 (−2.7 %) | 136.0 → 121.9 ms (−10.4 %) |
| the same, inside the field | 1,421 → 1,253 (−12 %) | 29.6 → 24.1 ms (−19 %) |

  - It pays where vertex work counts: dense meshes, and the iGPU with its 2 CUs.
  - Where pixels dominate (Sponza at 1080p: 0.13 triangles a pixel; FlightHelmet), each
    draw's cost outweighs the meshlets culled.
  - The Radeon's memory-bound passes also move with where its resources land
    (`docs/perf/reference-hw.md`), and the path changes buffer sizes.
- **Tried and dropped:**
  - Merging consecutive visible meshlets into one draw. On Sponza it cut 2,568 draws
    to 163 and the RTX's prepass from 73 to 65 µs. But culling rose 13 → 22 µs on the
    RTX and 19 → 53 µs on the Radeon, and the Radeon's shading lost its gain (9,306 →
    9,567 µs).
  - Only instances crossing the frustum. That removed most of the dense scenes' gain
    (overview on the RTX: +0.7 %).
- **Found on the way:** meshoptimizer gives meshlets whose normals spread over more than a
  half space a zero axis. Normalized, it made NaN, which culled 922 visible meshlets of
  Sponza. The test `tests/render/meshlets.rs` now draws a cube, a meshlet without a cone.
- **Open:**
  - Whether to turn it on by default per device class (a question for the owner).
  - A rule per instance that keeps pixel-bound instances whole (triangles per pixel?).
  - Room beyond 2^17 draws: in the dense overview only 29 % of the instances fit.
  - Step 3 (mesh shaders on the RTX, without a draw per meshlet) and step 4 (shadows: the
    largest pass of the dense scenes, unchanged here).

### Step 4, the shadow cascades (2026-10-14)

- **Work items per cascade.** With meshlets on, the shadow phase of `cullMainMeshlets`
  makes a caster of a cascade the work item of that cascade's meshlet lists. Each cascade
  has its own lists in the shadow command buffer after the cascades' command lists, and
  its draws' slots after the cascades' casters.
- **One indirect dispatch of `cullMeshlets` per cascade**
  (`push.phase = PHASE_SHADOW_MESHLETS + k`). It tests each meshlet's sphere against the
  cascade's square and far depth in light view space.
- **The cone, from infinitely far along the light.** The shadow pass draws single-sided
  geometry's faces away from the light and subsurface materials' faces towards it. A
  meshlet all of whose faces it would cull is skipped:
  `dot(±to_light, axis) ≥ cutoff`.
- **The shadows need no depth prepass** (depth only: any order will do). They draw by
  meshlet whenever meshlet culling is on.
- **Measured.** The shadow passes (culling included) and the frame's prepass + shading +
  culling, off → on:

| scene | RTX 4060 Ti | Radeon iGPU |
|---|---|---|
| spheres, overview: shadow | 4,294 → 4,123 µs (−4 %) | 67.4 → 60.2 ms (−11 %) |
| spheres, inside the field: shadow | 3,865 → 3,619 µs (−6 %) | 64.7 → 53.9 ms (−17 %) |
| spheres, overview: shadow + prepass + shading + culling | 12,932 → 12,526 µs (−3 %) | 203.4 → 182.2 ms (−10 %) |
| spheres, inside the field: all four | 5,287 → 4,875 µs (−8 %) | 94.4 → 78.0 ms (−17 %) |
| Sponza: shadow | 161 → 152 µs | 2,868 → 2,738 µs |
| Sponza: all four | 458 → 464 µs (+1.3 %) | 13,464 → 13,106 µs (−2.7 %) |
| FlightHelmet: shadow | 23 → 32 µs | 441 → 362 µs |
| FlightHelmet: all four | 103 → 124 µs (+20 %) | 2,580 → 2,629 µs (+1.9 %) |

  - Small scenes pay the fixed cost of the cascades' dispatches: about 7–9 µs on the
    RTX.
  - One dispatch for all the cascades' work items would save most of it (later).
  - Local lights' shadow views still draw whole instances.

### The meshlet cull in chunks (2026-10-14)

- **Where small scenes paid.** Per pass, off → on (scene_bench, 1080p, prepass, 4
  cascades):
  - FlightHelmet on the RTX: cull 4.5 → 11–12 µs, shadow cull 3.1 → 9–9.6, prepass 26 →
    30, shadow 20 → 23, shading 49 → 50.
  - Sponza on the RTX: the two culls +15 µs, prepass +14 (2,568 draws), shading −6,
    shadow −18.
  - Sponza on the Radeon: cull 9.6 → 21.5 µs, shadow cull 1.2 → 40.9.
  - The culls were latency-bound. One group per work item looped over all of an
    instance's meshlets, 64 at a time, so a large mesh's chain of dependent loads and
    atomics ran serially: FlightHelmet's 1,083 meshlets are in 6 instances.
  - It was not the number of dispatches: the cascades' four run back to back, without
    barriers between them.
- **Now a work item is a chunk of 64 meshlets.** `take_meshlets` appends one work item
  per chunk (`uint2`: the instance, its level | state << 8 | chunk << 16), and each of
  the meshlet cull's lanes tests one meshlet; a group never loops. The lists' room for
  work items is the instances plus the draws over 64: the chunks of the instances that
  fit fill at most one more work item each than their draws. It is in the header's last
  word (`MeshletHeader.work`); the draws follow the work items.
- **Measured,** off → on, shadow (its culling included) + prepass + shading + culling:

| scene | RTX 4060 Ti | Radeon iGPU |
|---|---|---|
| FlightHelmet | 103.6 → 118.3 µs (+14 %; was +20 %) | 2,583 → 2,548 µs (−1.4 %; was +1.9 %) |
| Sponza | 457.7 → 455.8 µs (−0.4 %; was +1.3 %) | 13,488 → 13,170 µs (−2.4 %; was −2.7 %) |
| 10,000 spheres, overview | 12,907 → 12,502 µs (−3.1 %) | 203.7 → 182.3 ms (−10.5 %) |
| 10,000 spheres, inside the field | 5,280 → 4,849 µs (−8.2 %) | 94.5 → 77.9 ms (−17.5 %) |

  - The culls of small scenes: FlightHelmet on the RTX 8.8 and 7.6 µs (was 11–12 and
    9–9.6), Sponza on the Radeon 14.3 and 35.9 µs (was 21.5 and 40.9).
  - What the RTX still pays on small scenes (about 9 µs) is the barrier and the dependent
    dispatch of each cull. Mesh shaders (step 3) cull in the task shader instead.
  - The dense scenes are unchanged: their instances are small.
- **Tried and dropped: drawing pixel-bound instances whole.** An instance wholly in view
  with fewer than D triangles per pixel of its bounding sphere's projection kept its
  instanced draw. With D = 1:
  - RTX: Sponza −5 µs, FlightHelmet −3 µs (its prepass back to 26.6 µs); but the spheres
    inside the field +40 µs (their near spheres' shading 622 → 658) and the city blocks
    +14 µs.
  - Radeon: Sponza +430 µs (prepass 1,022 → 1,117, shading 9,300 → 9,636), FlightHelmet
    slower too. The iGPU's few vertices and primitives a clock make culling meshlets pay
    even where pixels dominate.
  - So every instance that may take the path takes it.

