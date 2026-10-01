# 0034. GPU frustum culling and indirect instanced draws

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/gpu-driven-rendering.md](../research/gpu-driven-rendering.md)
- Extended by [ADR 0035](0035-occlusion-culling.md): two-phase occlusion culling;
  `add_passes` takes a `ForwardView`.
- Next step 1 done by [ADR 0064](0064-gpu-scene-extraction-runtime.md): a persistent GPU
  scene, drawn by `add_scene_passes`.

## Context

The forward renderer (ADR 0014) recorded two `vkCmdDraw` calls and two push-constant
updates per object per frame, one for the shadow pass and one for the forward pass. For
each object the CPU also:

- inverted a 3×3 matrix;
- wrote 160 bytes into the upload ring;
- transformed eight box corners to fit the light.

Nothing was culled.

The stress scene has about 12k objects, and the target is 240 FPS, a 4.17 ms frame
(CLAUDE.md §5). At that size, recording alone costs milliseconds of CPU time. CLAUDE.md §6
asks for GPU-driven rendering: GPU culling (frustum, then HiZ occlusion), meshlets and
indirect draws. The team asked for GPU culling and indirect draws first.

## Options

1. **Keep per-object commands and cull on the CPU.**
   - Fewer draws, but the CPU still pays a driver call per visible object and per shadow
     caster.
   - The cost grows with the scene.
2. **GPU culling with one indirect command per visible object (niagara).**
   - A compute pass appends a command per visible object behind one atomic counter.
   - Drawn with `vkCmdDrawIndirectCount`.
   - No batching on the CPU, but the GPU front end processes one command per object.
   - The device needs `drawIndirectCount`.
3. **GPU culling into per-batch instance lists, one instanced indirect command per
   batch (Bevy-like, chosen).**
   - A batch is a run of consecutive draws with the same mesh.
   - A compute pass tests each instance and appends the visible ones to its batch's
     range of a visible list, counting them in the batch's command.
   - Each drawing pass records one `vkCmdDrawIndirect` for all batches.
4. **Mesh shaders with cluster culling.**
   - This is the end goal (§6).
   - The AMD iGPU on ref-1 has no `VK_EXT_mesh_shader`, and meshlets need the
     meshoptimizer import pipeline. Option 3 is also the fallback path for it.

## Measurements

`crates/engine_render/examples/draw_bench.rs`: 1920×1080 HDR, 2048² shadow map, spinning
cubes on a field. Medians over 30 frames.

- **Overview:** nearly all cubes in view.
- **Inside:** near the ground with an 80 m far plane, as in an open world.

The container numbers below are from lavapipe (a CPU rasterizer, **not reference**). They
compare CPU costs on one driver. GPU times on lavapipe only show the proportions.

**CPU µs, main (per-object commands) → this ADR:**

| Cubes | `add_passes` | graph recording |
|---:|---:|---:|
| 1,000 | 31 → 12 | 291–305 → 43–55 |
| 10,000 | 422–466 → 121–132 | 3,844–4,336 → 95–98 |
| 100,000 | 4,950–5,002 → 3,040–3,363 | 44,044–46,764 → 145–159 |

**Culling** (inside view, lavapipe):

- 10k cubes: 1,733 visible, forward pass 24.3 → 18.9 ms;
- 100k cubes: 1,742 visible, forward pass 85.9 → 18.4 ms;
- the culling pass costs 1.1–1.3 ms on lavapipe at 100k (one thread per instance).

**Correctness** (`tests/culling.rs`, under synchronization validation):

- 557 instances of three meshes in six batches. Batch sizes 37, 1, 150, 64, 5 and 300
  put batch boundaries inside waves.
- The GPU visible counts equal the CPU reference (`engine_render::cull`).
- Culling off counts every instance.
- The images with and without culling are identical.
- 0 validation errors.
- The existing forward test gives the same pixels as before.

**ref-1** (GPU workflow on `9135f7b`, release). The culling test passes on both GPUs
(GPU counts equal the CPU reference, 0 validation errors).

| RTX 4060 Ti, 100k cubes | overview | in-field, 1,742 visible |
|---|---:|---:|
| draw list (caller) | 1.12 ms | 1.12 ms |
| `add_passes` (10 ns per instance) | 1.00 ms | 1.00 ms |
| graph recording | 13 µs | 12 µs |
| GPU cull | 10.9 µs | 9.5 µs |
| GPU shadow | 244 µs | 244 µs |
| GPU forward, culling on / off | 310 / 311 µs | **47 / 267 µs** |

- **Radeon iGPU, 10k cubes, in-field view:**
  - forward 1,634 → 1,081 µs with culling;
  - cull 20 µs;
  - shadow about 600 µs.
- **Cubes example** (26 draws, debug build with validation): 2,401 FPS on NVIDIA, graph
  CPU 102 µs per frame.

## Decision

**Data per frame, in the upload ring:**

- **Instance, 80 bytes:**
  - the rows of the affine `world_from_object` (the bottom row is ignored);
  - base color and metallic;
  - roughness;
  - batch index.

  This replaces 160 bytes with a normal matrix.
- **Batch, 48 bytes:**
  - vertex address and count;
  - first instance;
  - the mesh's object-space box.
- **Two `VkDrawIndirectCommand` per batch:**
  - the forward pass's, with no instances;
  - the shadow pass's, with all of them.
- **Frame constants.** These add the six frustum planes (Gribb–Hartmann from
  `clip_from_world`, any projection) and the culling switch.

**Passes (render graph):**

1. **cull setup:** `vkCmdCopyBuffer` of the forward commands from the ring into a graph
   buffer.
2. **cull:** a compute shader, 64 threads per group, one thread per instance.
   - It builds the instance's world box from the mesh box (Arvo) and tests it against
     the planes (`dot(n, c) + dot(|n|, e) + w >= 0`).
   - It appends visible instances at `batch.first + slot` of the visible list.
   - When all lanes of a wave share a batch, one lane does the `InterlockedAdd` for the
     wave, and lanes take `WavePrefixCountBits` offsets. Otherwise each visible lane
     adds 1.
3. **shadow:** `vkCmdDrawIndirect(ring commands, batches)`. The instance index is
   `SV_VulkanInstanceID`.
4. **forward:** `vkCmdDrawIndirect(culled commands, batches)`. The instance is
   `visible[SV_VulkanInstanceID]`.
   - Normals use the cofactor matrix of the linear part with the determinant's sign. The
     fragment shader normalizes.
   - Material values arrive as flat interpolants.

**Required device features.** These are added to ADR 0006's required set:

- `multiDrawIndirect`;
- `drawIndirectFirstInstance`;
- subgroup basic, vote and ballot operations in compute.

All three are universal on Vulkan 1.3 desktop drivers and lavapipe.

**API:**

- `ForwardRenderer::add_passes` keeps its `&[DrawItem]` input.
- It now returns `Option<ForwardPasses>`: instances, batches, and the graph buffer of
  culled commands for readback. `None` means the ring is full.
- `set_frustum_culling(false)` counts every instance as visible, for comparisons and
  debugging.
- `engine_render::cull` provides `Frustum` and `transform_box` as the CPU reference.

## Consequences

- **CPU recording no longer grows with the number of objects.**
  - What remains per object is writing 80 bytes and one box transform, about 13 ns in
    the container.
  - The caller must keep draws of a mesh together to get one batch per mesh.
    Interleaved meshes still render correctly, as more batches.
- **Order within a batch varies from frame to frame** (atomics). Opaque rendering with a
  depth test does not see it, except for exactly coplanar overlapping instances.
- **Mirrored instances:**
  - normals stay correct;
  - triangle winding flips, as before this change;
  - back-face culling of mirrored instances needs a separate pipeline or batch, when
    that becomes necessary.
- **Shadow pass.** It draws every instance, because the light's projection contains the
  whole scene. The draw bench shows it becoming the main GPU cost with many objects.
- **Next steps, in order:**
  1. a persistent GPU scene: stable instance slots, only changed instances uploaded;
  2. two-phase HiZ occlusion culling, which needs the stable slots;
  3. cascaded shadow maps with per-cascade caster culling in the same compute pass;
  4. meshlets with mesh shaders, keeping this path as the fallback.
