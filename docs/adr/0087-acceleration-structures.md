# 0087. Acceleration structures for ray queries, built from the GPU scene

- Status: Accepted
- Date: 2026-10-11
- Research: [docs/research/acceleration-structures.md](../research/acceleration-structures.md)
- Related: [ADR 0084](0084-global-illumination.md) (the GI whose rays they serve),
  [ADR 0064](0064-gpu-scene-extraction-runtime.md) (the GPU scene they are built from),
  [ADR 0039](0039-geometry-pool.md) (the positions and indices),
  [ADR 0053](0053-mesh-lods.md) (detail levels), [ADR 0011](0011-render-graph.md) and
  [ADR 0065](0065-async-compute.md) (passes and queues), queue item 3

## Context

ADR 0084 chose our own probe GI traced with ray queries; acceleration structures come
first ("in `engine_rhi` and the GPU scene"). Both reference GPUs expose
`VK_KHR_acceleration_structure`, `VK_KHR_ray_query` and
`VK_KHR_ray_tracing_position_fetch`. The engine's model: bindless resources, buffer
addresses in push constants, a persistent GPU scene whose moving instances are placed by
the GPU between ticks (the CPU does not hold their transforms), a render graph that
orders passes by what they share and culls passes without outputs, and zero heap
allocations per frame.

## Options

1. **How rays are traced.**
   a. **Ray queries** (`VK_KHR_ray_query`) from compute passes (any stage can): the
      nearest hit and its surface come back to the shader, which shades it itself; no
      shader binding tables; AMD recommends inline tracing in compute.
   b. Ray tracing pipelines (`VK_KHR_ray_tracing_pipeline`): hit shaders per material,
      binding tables; needed only to shade hits with every material's own shader.
2. **How shaders reach the top level.**
   a. **Its device address**, pushed like any buffer address and turned into a structure
      by `OpConvertUToAccelerationStructureKHR` (`accel_at` in `rays_common.slang`, a
      `spirv_asm` block on a `uint2`): nothing changes in the bindless set when the
      structure is replaced or grows.
   b. A descriptor in the bindless set (`VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR`),
      updated when the structure changes.
3. **Who writes the top level's instance records.**
   a. **A compute pass from the GPU scene's instance data**, one record per slot (Bevy
      Solari also sets its instances up on the GPU): instances placed on the GPU move in
      it too, and the CPU uploads only a 16-byte record per batch (its bottom level, mask
      and flags).
   b. The CPU, into a mapped buffer (Wicked Engine): it would need the transforms the
      GPU computes for moving instances.
4. **Bottom levels.** One per mesh, built when a mesh is first drawn, from a chosen detail
   level, with `PREFER_FAST_TRACE | ALLOW_COMPACTION` (both vendors' advice for static
   geometry), then compacted, or left as built (2.4–2.6× the memory, below).
5. **The top level's builds.**
   a. **Rebuilt with `PREFER_FAST_TRACE`** in frames where an instance or a structure
      changed, kept otherwise (NVIDIA: "perform only rebuilds"; AMD: rebuild every frame).
   b. Rebuilt with `PREFER_FAST_BUILD` (Wicked Engine).
   c. Refitted (built with `ALLOW_UPDATE`, updated in place) between rebuilds.

## Measurements

ref-1, release, 2026-10-11, Sponza (262,267 triangles in 103 meshes, one instance each),
`scene_bench --rays [finest|coarsest]`, 1920 × 1080, a ray per pixel from the camera
(2,073,600 rays, nearest hit, every instance):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| Bottom levels built, finest (all in the first frame) | 1.55–1.68 ms | 38.8 ms |
| Bottom levels built, coarsest (47,269 triangles, `--lods`) | 0.65 ms | 8.7 ms |
| Bottom levels, finest: as built → compacted | 17.8 → 7.5 MiB | 40.7 → 15.8 MiB |
| Bottom levels, coarsest, compacted | 1.4 MiB | 2.9 MiB |
| Top level rebuilt (103 instances): records + build | 1–2 + 78–85 µs | 1.5–2.4 + 222–231 µs |
| Camera rays, finest / coarsest bottom levels | 0.89–0.97 / 0.95 ms | 35.6–36.8 / 34.6 ms |

The top level's rebuild grows with its instances (103 / 1,030 / 10,300, Sponza copied
side by side): RTX 85 / 116 / 176–181 µs, Radeon 231 / 507 / 3,716–3,798 µs.

Option 5 measured (medians; one instance moving every frame):

| | RTX, 103 | RTX, 10,300 | Radeon, 103 | Radeon, 10,300 |
|---|---:|---:|---:|---:|
| a. Rebuild, fast trace: top level µs / camera rays ms | 79.9 / 0.887 | 181.0 / 1.058 | 230.4 / 36.3 | 3,798 / 36.8 |
| b. Rebuild, fast build | 78.8 / 1.186 | 180.2 / 1.384 | 67.9 / 40.7 | 1,497 / 38.5 |
| c. Refit in place | 17.4 / 0.968 | 32.6 / 1.059 | 21.6 / 36.3 | 546 / 36.4 |

Fast build saves nothing on the RTX and slows its rays by 31–34 %; on the Radeon it
builds 2.5–3.4× faster and slows rays by 5–12 %. A refit costs a fifth (RTX) to a tenth
(Radeon) of a rebuild, but this bench moves one instance by a millimetre: how its
hierarchy degrades when many instances travel far is not measured yet.

Correctness: `tests/render/rays.rs` casts rays at cubes in a GPU scene (scaled,
mirrored, removed, transparent) over four frames (built, compacted and moved, settled,
kept) and checks distances, slots, faces and masks on both GPUs, with 0 validation and
synchronization-validation errors; Sponza's distance image, compared by eye with its
rasterization, matches it.
`zero_alloc/gpu_scene.rs` with a ray scene: 0 heap allocations in 100 frames on both
GPUs.

## Decision

Options **1a, 2a, 3a, 4 with compaction, 5a**, as follows.

- **`engine_rhi`:** the device enables `VK_KHR_acceleration_structure`,
  `VK_KHR_deferred_host_operations`, `VK_KHR_ray_query` (and
  `VK_KHR_ray_tracing_position_fetch` where present) when the adapter has them and
  `ENGINE_RAY_QUERY` is not `0`; `Device::ray_query` gives the loader and limits.
  `AccelerationStructure`: a structure in a device-local buffer of its own.
- **`engine_render::rays::RayScene`**, fed every frame by the geometry pool and the GPU
  scene's frame (nothing exists before it is made: a renderer without rays pays
  nothing):
  - **bottom levels** per mesh at `BlasDetail::Finest` (default) or `Coarsest`, opaque,
    `PREFER_FAST_TRACE | ALLOW_COMPACTION`, built in one pass (one scratch buffer, a
    region per build, no barriers between them); their compacted sizes queried in the
    same command buffer and read once that frame has finished; at most one million
    triangles compacted per frame; replaced structures freed when their frames are done;
  - **the top level**: one record per GPU-scene slot written by a compute pass (the custom
    index is the slot; holes and meshes without a structure inactive), its mask the
    batch's kind (`RAY_MASK_OPAQUE`, `RAY_MASK_MASKED`, `RAY_MASK_BLENDED`), facing culling
    off and every instance forced opaque; built with `PREFER_FAST_TRACE` in frames where
    an instance or a structure changed, kept in a still scene; its capacity grows by
    powers of two;
  - `RayFrame` gives the frame's top level (a graph buffer and its address) and two
    passes: rays cast in a batch (`add_cast_pass`: tests, picking) and a ray through
    every pixel (`add_camera_pass`: the scene's distance image, throughput).
- **The render graph:** accesses for structure builds (`AccelerationStructureInput`,
  `…Scratch`, `…Write`, `…BuildRead`) and for shaders that trace (`…Read(stages)`);
  transient buffers aligned to at least 256 bytes (instance records must be 16-byte
  aligned; the Radeon got an unaligned one); the bottom levels' passes declare side
  effects (the structures they write are not graph resources) and order the top level's
  build through a token buffer they write.
- The top level is built by calling `vkCmdBuildAccelerationStructuresKHR` itself: ash's
  wrapper collects the range pointers into a vector, a heap allocation every frame.

## Consequences

- GI (ADR 0084) traces from compute passes that read `RayFrame::tlas`; GPUs without ray
  queries get no `RayScene` and keep the sky's light.
- Memory: Sponza's structures take 7.5 / 15.8 MiB (RTX / Radeon) at the finest level and
  1.4 / 2.9 MiB at the coarsest, which diffuse GI can use.
- **Next step, measured above but not yet taken** (its own change, then GI): a refit of
  the top level between rebuilds, with a bound on how far its hierarchy may degrade
  (studied by tracing a refitted and a rebuilt top level in the same frames while many
  instances move); the top level built on the async compute queue (AMD's advice; the
  structures' buffers then shared by both queue families); a per-frame budget for
  bottom-level builds (all of Sponza at once costs 38.8 ms on the iGPU: meshes appearing
  during play would hitch), with waiting meshes inactive until built; and `PREFER_FAST_BUILD`
  on integrated GPUs decided together with the refit.
- Known gaps: alpha-tested surfaces are solid to rays (no candidate processing);
  skinned meshes keep the shape of their first build (a refitted bottom level from the
  pre-skinned positions, ADR 0068, later); every instance is in the top level (no
  culling by distance or effect).
