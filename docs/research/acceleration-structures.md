# Acceleration structures

Research for queue item 3's first step, 2026-10-11: the structures that GI's rays trace
(ADR 0084), and later ray-traced shadows and reflections. Decided and built:
[ADR 0087](../adr/0087-acceleration-structures.md).

## Sources (checked 2026-10-11)

| Source | License | How | Notes |
|---|---|---|---|
| Vulkan 1.4 specification: `VK_KHR_acceleration_structure`, `VK_KHR_ray_query`, `VK_KHR_ray_tracing_position_fetch` | spec | Bottom levels of triangles, top levels of instances (64-byte records: a 3 × 4 transform, a 24-bit custom index, an 8-bit mask, flags, a bottom level's address); builds and copies recorded on any queue with compute; compacted sizes through queries; ray queries from any shader stage | The rules for updates (below) |
| NVIDIA, "Best Practices for Using NVIDIA RTX Ray Tracing (Updated)", 2022-07-25 | article | Static bottom levels `PREFER_FAST_TRACE`; compaction ("up to about 50 %" less memory); top levels `PREFER_FAST_TRACE`, "perform only rebuilds"; cull the instances the effect does not need; bottom-level updates for limited deformation, rebuilds spread over frames; builds batched without barriers between them, each with its own scratch | Our flags and batching |
| AMD, "RDNA Performance Guide", ray tracing section, 2023-03-22 | article | "Smaller BVHs are always better"; high-quality build flags wherever possible; "rebuild your TLAS every frame on the compute queue if possible"; only rebuild or refit the detail levels you need; inline ray tracing (DXR 1.1) best in compute shaders on a compute queue; 8 × 4 thread groups; one ray query object in scope at a time | The async compute queue: next step |
| Bevy Solari (Bevy main at 90942be: `crates/bevy_solari/src/scene/blas.rs`, `scene/binder/mod.rs`) | MIT / Apache-2.0 | A bottom level per mesh and opacity (opaque geometry flagged `OPAQUE`), `PREFER_FAST_TRACE \| ALLOW_COMPACTION`; compaction queued and bounded to 400,000 vertices per frame; replaced structures deleted two top-level builds later; with raw builds, the bottom levels' references uploaded and the instances set up on the GPU by a compute pipeline; the top level rebuilt every frame | The compaction bound and the instances written on the GPU |
| Wicked Engine (cee348b: `wiScene.cpp`, `wiScene_Components.cpp`) | MIT | A bottom level per mesh detail level, the instance referring to its object's level; static meshes `PREFER_FAST_TRACE`, skinned ones `ALLOW_UPDATE \| PREFER_FAST_BUILD`; the top level `PREFER_FAST_BUILD`, its instance buffer grown to twice what is needed, its records written on the CPU by jobs into a mapped buffer every frame; masks from layers (bits for shadows and reflections); `TRIANGLE_CULL_DISABLE` for double-sided meshes, `FRONT_COUNTERCLOCKWISE` when the transform's determinant is positive; its own GPU BVH where hardware ray tracing is missing | Masks per kind of surface; a software tracer as a later fallback |

## The questions, and what each answer costs

- **Ray queries or ray tracing pipelines.** GI's probe rays, shadows and reflections need
  the nearest hit and the hit's surface, which compute shaders can shade themselves: ray
  queries (`VK_KHR_ray_query`) trace from compute passes the render graph already
  schedules, without shader binding tables or hit shaders. AMD recommends inline tracing
  in compute. Pipelines (`VK_KHR_ray_tracing_pipeline`) would be needed only for shading
  hits with every material's own shader (a path tracer); not planned.
- **How shaders find the top level.** A descriptor (`VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR`
  in the bindless set) or its device address turned into a structure in the shader
  (`OpConvertUToAccelerationStructureKHR`, part of `SPV_KHR_ray_query`). The address fits
  the engine's model: every pass already receives buffer addresses in push constants, and
  nothing in the bindless set changes when the structure is replaced. Slang has no
  intrinsic for it; a `spirv_asm` block on a `uint2` emits the instruction (no 64-bit
  integer capability needed). Works on both reference drivers.
- **Who writes the instance records.** The CPU (Wicked Engine: jobs writing into a mapped
  buffer) or the GPU (Bevy Solari's instance setup pipeline). The engine's GPU scene
  (ADR 0064) already holds every instance's transform on the GPU, and moving instances
  are placed by the GPU between ticks, so the CPU does not have their transforms: the
  records are written by a compute pass from the GPU scene, one per slot (a slot's index
  is the custom index; holes and meshes without a structure are inactive, a reference of 0).
- **Bottom levels.** One per mesh (not per instance), from the geometry pool's positions
  and indices; `PREFER_FAST_TRACE` and `ALLOW_COMPACTION` (both vendors). Compaction: the
  compacted size is queried in the build's command buffer, read once that frame has
  finished, then the structure is copied into one of that size (Bevy Solari's bound on
  the work per frame kept: one million triangles). Which detail level: the finest (exact
  shadows and reflections) or the coarsest (diffuse GI needs no more; about a fifth of the
  memory and build time).
- **Top level: flags and rebuild or refit.** Both vendors: rebuild with
  `PREFER_FAST_TRACE`; Wicked Engine builds with `PREFER_FAST_BUILD`. A refit (an update
  in place) keeps the hierarchy and moves its boxes: far cheaper, but the hierarchy
  degrades as instances move away from where it was built. The Vulkan rules for an
  update (VUIDs 03663, 03664, 03667, 03668, 03769): the source built with `ALLOW_UPDATE`,
  the same flags and instance count, source and destination the same structure or not
  aliased, and no instance turned active or inactive (a reference changing to or from 0).
  Measured below; decided in two steps (ADR 0087).
- **Facing, opacity, masks.** Facing is decided in object space: an instance's transform
  never flips it, so mirrored instances need nothing (unlike rasterization, where the
  engine flips the winding of mirrored instances). Facing culling is turned off per
  instance (shaders read the face); every instance is forced opaque (no candidate
  processing: alpha-tested surfaces trace as solid for now). The mask holds the batch's
  kind (opaque, alpha-tested, blended), so a pass chooses which surfaces its rays see.

## What we take, what we do not

- Taken: ray queries from compute; the vendors' flags; a bottom level per mesh with
  compaction bounded per frame (Bevy Solari); the instances written on the GPU (Bevy
  Solari's approach, from our GPU scene); masks by the kind of surface (Wicked Engine's
  idea, our kinds); the top level kept when nothing changed.
- Not taken (yet): `PREFER_FAST_BUILD` for the top level (Wicked Engine): slower rays on
  the RTX for no build gain; on the integrated Radeon, decided with refits (next step).
  Ray tracing pipelines; any-hit processing for alpha-tested surfaces; instance culling;
  Wicked Engine's software BVH for GPUs without ray tracing.
- No code was copied from Bevy or Wicked Engine; both were read for their choices only.
  The engine implements no hierarchy algorithm: the drivers build, refit and traverse.

## What was measured (2026-10-11)

ref-1, release, Sponza (Khronos glTF: 262,267 triangles in 103 meshes, one instance each),
`scene_bench test-assets/Sponza/Sponza.gltf --rays [coarsest] [--lods]`, 1920 × 1080:

| | RTX 4060 Ti | Radeon iGPU (2 CUs) |
|---|---:|---:|
| Bottom levels built in the first frame (finest) | 1.55–1.68 ms | 38.8 ms |
| Bottom levels built (coarsest: 47,269 triangles, `--lods`) | 0.65 ms | 8.7 ms |
| Bottom levels, finest: as built → compacted | 17.8 → 7.5 MiB | 40.7 → 15.8 MiB |
| Bottom levels, coarsest, compacted | 1.4 MiB | 2.9 MiB |
| Top level (103 instances) | 39.1 KiB | 29.4 KiB |
| Top level rebuilt: instance records + build | 1–2 + 78–85 µs | 1.5–2.4 + 222–231 µs |
| A ray per pixel from the camera, nearest hit (2,073,600 rays) | 0.89–0.97 ms (2.1–2.3 G/s) | 35.6–36.8 ms (57–60 M/s) |
| The same against the coarsest bottom levels | 0.95 ms | 34.6 ms |

The coarsest levels save memory and build time, not tracing time on these GPUs: the rays
spend their time in the upper levels of the hierarchy.

The top level's rebuild against its instance count (Sponza copied side by side, 40 m
apart: 103, 1,030 and 10,300 instances): RTX 85, 116 and 176–181 µs; Radeon 231, 507 and
3,716–3,798 µs (its instance records 91 µs of that at 10,300). At 10,300 instances the top
level takes 4,825 KiB on the RTX and 3,691 KiB on the Radeon.

Alternatives for the top level (medians of each run; camera rays against the finest
bottom levels; one instance moving 1 mm every frame):

| | RTX, 103 | RTX, 10,300 | Radeon, 103 | Radeon, 10,300 |
|---|---:|---:|---:|---:|
| Rebuild, `PREFER_FAST_TRACE`: top level µs | 79.9 | 181.0 | 230.4 | 3,798 |
| its camera rays, ms | 0.887 | 1.058 | 36.3 | 36.8 |
| Rebuild, `PREFER_FAST_BUILD`: top level µs | 78.8 | 180.2 | 67.9 | 1,497 |
| its camera rays, ms | 1.186 (+34 %) | 1.384 (+31 %) | 40.7 (+12 %) | 38.5 (+4.8 %) |
| Refit (`ALLOW_UPDATE`, updated in place): top level µs | 17.4 | 32.6 | 21.6 | 546 |
| its camera rays, ms | 0.968 | 1.059 | 36.3 | 36.4 |
| Top level KiB with `ALLOW_UPDATE` (without) | 45.1 (39.1) | 5,551 (4,825) | 29.4 (29.4) | 3,691 (3,691) |

The RTX's camera rays vary between 0.89 and 0.97 ms from run to run with the same
structures, so the refit's 0.968 is within that spread. A refit here follows one small
move: how fast its hierarchy degrades when many instances travel far is the open
question before it is used (ADR 0087, next step).
