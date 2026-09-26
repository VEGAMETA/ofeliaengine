# GPU-driven rendering: culling and indirect draws

Date: 2026-09-26. Input for [ADR 0034](../adr/0034-gpu-culling-and-indirect-draws.md). We
read the code for ideas only. No code was copied.

## Sources

| Source | Version checked | License | What we looked at |
|---|---|---|---|
| [niagara](https://github.com/zeux/niagara) (A. Kapoulkine) `src/shaders/drawcull.comp.glsl`, `depthreduce.comp.glsl`, `math.h`, `src/niagara.cpp` | `master`, fetched 2026-09-26 | MIT | per-draw culling, commands with an atomic counter, `vkCmdDrawIndexedIndirectCount`, two-phase occlusion, depth pyramid with a min-reduction sampler |
| [Bevy](https://github.com/bevyengine/bevy) `crates/bevy_render/src/batching/gpu_preprocessing.rs` | `main` (0.20.0-dev), fetched 2026-09-26 | MIT / Apache-2.0 | GPU preprocessing and culling per view, per-batch instance counters, batch sets, `multi_draw_indirect(_count)`, early/late counts for occlusion |
| U. Haar, S. Aaltonen, "GPU-Driven Rendering Pipelines", SIGGRAPH 2015 Advances in Real-Time Rendering | talk | — | instance → cluster → triangle culling, two-phase HiZ occlusion, multi-draw indirect |
| G. Wihlidal, "Optimizing the Graphics Pipeline with Compute", GDC 2016 | talk | — | cluster and triangle culling in compute, compaction, indirect draws |
| B. Karis et al., "Nanite: A Deep Dive", SIGGRAPH 2021 | talk | — | two-pass hierarchical-Z occlusion culling |
| G. Gribb, K. Hartmann, "Fast Extraction of Viewing Frustum Planes from the World-View-Projection Matrix", 2001 | paper | — | frustum planes from rows of the matrix |
| J. Arvo, "Transforming Axis-Aligned Bounding Boxes", Graphics Gems, 1990 | book chapter | — | exact box around a transformed box |
| M. Mara, M. McGuire, "2D Polyhedral Bounds of a Clipped, Perspective-Projected 3D Sphere", JCGT 2013 | paper | — | screen rectangle of a sphere, for the occlusion test (next step) |

## How they work

### niagara

- **One draw per object.** A draw holds a mesh index, position, scale and orientation. A
  mesh holds a bounding sphere and levels of detail.
- **Culling** is one compute thread per draw:
  - moves the sphere into view space;
  - tests it against the left/right and top/bottom planes at once, using the frustum's
    symmetry (`center.z * f1 - abs(center.x) * f0 > -radius`);
  - tests near and far on view-space z.
- **Occlusion** (late pass only):
  - the sphere's screen rectangle comes from Mara and McGuire's formula;
  - one fetch from the depth pyramid at the mip where the rectangle covers at most 2×2
    texels;
  - the pyramid is R32F, built with a sampler in `VK_SAMPLER_REDUCTION_MODE_MIN`.
  - Two phases: an early pass draws what was visible last frame (`drawVisibility`),
    the pyramid is built from that depth, and a late pass tests the rest.
- **LOD** by projected error.
- **Output:**
  - every visible draw appends its own command, `atomicAdd` on a single counter;
  - drawn with `vkCmdDrawIndexedIndirectCount`, one command per visible object;
  - there is no instancing;
  - a task-shader (meshlet) variant exists.
- Pros:
  - very simple;
  - per-object LOD falls out naturally;
  - no batching on the CPU.
- Cons:
  - the GPU front end processes one command per visible object;
  - every visible object hits the same atomic;
  - needs `drawIndirectCount`.

### Bevy (`gpu_preprocessing`)

- The CPU sorts phase items into **bins** (pipeline, material, mesh). A **batch** is one
  mesh drawn with instancing. **Batch sets** are batches that one multi-draw call can
  draw.
- **Preprocessing** is a compute pass per view:
  - reads compact per-instance input (transforms, flags) from persistent buffers that
    receive sparse updates;
  - expands it into full per-instance data, including the normal matrix;
  - culls against the frustum (the default mode; "preprocessing only" when a camera
    turns indirect drawing off);
  - increments the batch's instance count in per-batch metadata;
  - writes the instance at `base_output_index + slot`.
- **Indirect parameters:** a second compute pass (`build_indirect_params`) turns the
  metadata into draw commands, so the counters can be reset independently. Drawing uses
  `multi_draw_indirect_count` where the device has it, and `multi_draw_indirect`
  otherwise.
- **Occlusion culling:** separate early and late instance counts per batch implement
  two-phase occlusion culling.
- Pros:
  - instancing: one command per batch, not per object;
  - the CPU uploads less data than the GPU uses;
  - persistent buffers.
- Cons:
  - two compute passes plus metadata buffers;
  - a lot of machinery for wgpu's portability limits (no push constants everywhere,
    dynamic uniform alignment).

### Literature

- **Haar and Aaltonen (Assassin's Creed Unity):**
  - culling runs at three levels: instances, then 64-triangle clusters, then triangles;
  - occlusion is two-phase against a hierarchical-Z buffer, with reprojection of last
    frame's depth for the first phase;
  - one multi-draw indirect call per material "bucket".
- **Wihlidal (Frostbite):**
  - cluster culling and per-triangle culling in compute (backface, small primitive,
    frustum, depth);
  - compaction with wave intrinsics: one atomic per wave instead of per thread.
- **Nanite:** two-pass HZB occlusion. Pass 1 draws what was visible last frame, the HZB
  is rebuilt, and pass 2 tests only the rest. There is no reprojection error and no
  popping.

## What we take

- **Instancing per batch (Bevy).** A batch is a run of consecutive draws with the same
  mesh:
  - one indirect command per batch;
  - its instance count is filled by the culling pass;
  - its visible list lives in a fixed range whose capacity is the batch's size. No prefix
    sum is needed, and the visible slots are exactly `firstInstance + i`.
- **One compute pass.** It writes the instance counts straight into the commands. The
  commands are first copied in with zero instances (one small `vkCmdCopyBuffer`), which
  saves Bevy's second pass and metadata.
- **Box test, not sphere.**
  - Each instance's world-space box comes from Arvo's method: center through the
    transform, extents through the absolute linear part.
  - It is tested against planes from Gribb and Hartmann, with the box's effective radius
    per plane (`dot(|n|, extents)`).
  - This is tighter than a sphere for flat or long objects, such as a ground slab, where
    a sphere is almost the whole world.
  - It works for any projection: off-center, orthographic, the Y flip folded into ours.
- **Wave aggregation (Wihlidal).** When all lanes of a wave belong to one batch, which is
  almost always since batches are contiguous, one lane does the `InterlockedAdd` for the
  wave and lanes take `WavePrefixCountBits` offsets.
- **Fewer vertex-shader inputs.**
  - Normals use the cofactor matrix of the linear part, with the determinant's sign.
    This is the inverse transpose up to a positive factor, and the fragment shader
    normalizes. It replaces a 3×3 inverse per object on the CPU.
  - Material values reach the fragment shader as flat interpolants.
- **`vkCmdDrawIndirect` with the batch count from the CPU.** No `drawIndirectCount` is
  needed.
- **Shadow pass:** indirect from the upload ring, every instance. The light's projection
  contains the whole scene, so caster culling would reject nothing.

## What we do not take (yet), and why

- **One command per visible object (niagara).** With instancing, the front end sees one
  command per mesh, not per object.
- **Two-phase occlusion culling.**
  - It needs instances that keep their identity across frames, to know what was visible
    last frame. Our instances are rebuilt from the draw list every frame.
  - It comes after a persistent GPU scene (stable instance slots, uploads of changed
    instances only). That scene also removes the per-frame upload of static objects.
- **LOD selection, meshlets, cluster and triangle culling.** These need meshoptimizer
  (planned with asset import) and mesh shaders with a fallback. The AMD iGPU on ref-1
  has no `VK_EXT_mesh_shader`.
- **Visibility buffer vs clustered forward+.** This is a separate ADR (CLAUDE.md §6),
  once materials and lights need it.

## Where this leads

1. **Persistent GPU scene.**
   - Stable instance slots and dirty-only uploads.
   - Static objects then cost nothing per frame on the CPU. Today every instance costs
     about 13 ns plus 80 bytes of upload per frame (lavapipe container).
2. **Two-phase HiZ occlusion culling.**
   - Depth pyramid with a min/max reduction sampler (`samplerFilterMinmax`, supported on
     ref-1's GPUs and lavapipe).
   - Screen rectangles from boxes or Mara–McGuire spheres.
3. **Cascaded shadow maps with GPU caster culling per cascade.**
   - The draw bench shows the shadow pass dominating at 100k objects. The whole scene
     goes into one map fitted to the scene.
4. **Meshlets and mesh shaders**, with the indirect path as the fallback.
