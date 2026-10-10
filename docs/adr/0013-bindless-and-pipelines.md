# 0013. Bindless resources, pipelines and the first mesh path

- Status: Accepted
- Date: 2026-09-26
- Builds on: [ADR 0006](0006-vulkan-baseline-and-device.md) (descriptor indexing and buffer
  device address are required), [ADR 0011](0011-render-graph.md),
  [ADR 0012](0012-math-library.md)

## Context

The renderer needs a way to give shaders their resources. CLAUDE.md §6 says "descriptor
indexing (bindless)", GPU-driven rendering and meshlets. Pipelines must be created for
dynamic rendering (no render passes). The first 3D path must prove the whole chain:

graph → pipeline → descriptors → shader → pixels.

## Options

### Descriptors

1. **Descriptor sets per material or draw.** Classic Vulkan: many layouts, many binds,
   and allocation churn. Incompatible with GPU-driven rendering.
2. **One bindless set plus buffer device addresses** (chosen). All textures, storage
   images and samplers sit in one update-after-bind set. Shaders index them with integers
   carried in push constants or buffers, and buffers are plain pointers. One pipeline
   layout serves everything, so nothing is rebound between draws.
3. **`VK_EXT_descriptor_buffer`.** Lower overhead, but optional in our baseline (ADR 0006).
   It can replace option 2 behind the same interface later.

### Vertex input

1. **Fixed-function vertex input.** Per-pipeline vertex formats. It does not fit meshlets
   or GPU-driven culling.
2. **Vertex pulling** (chosen). Shaders read vertices through buffer device addresses. No
   vertex input state, so one pipeline works for any vertex buffer, and it is the layout
   the meshlet path will need.

## Decision

- **`engine_rhi::Bindless`:**
  - **Set 0:**
    - binding 0: up to 65 536 sampled images;
    - binding 1: up to 16 384 storage images;
    - binding 2: six built-in samplers (linear or nearest × repeat or clamp, 16×
      anisotropic, shadow compare).
    - Counts are clamped to the device's limits.
  - **Binding flags:** `PARTIALLY_BOUND | UPDATE_AFTER_BIND | UPDATE_UNUSED_WHILE_PENDING`.
  - **Index recycling:** indices are retired with the last frame that may use them and are
    recycled only after that frame finishes (`collect`), so adding a texture never waits
    for the GPU.
  - **Pipeline layout:** one layout for all pipelines, with this set and 128 bytes of push
    constants for all stages.
- **Required feature added:** `descriptorBindingUpdateUnusedWhilePending`, which amends
  ADR 0006's list. It is supported by every Vulkan 1.2+ desktop driver we target, and by
  lavapipe.
- **`engine_rhi::Pipeline`:**
  - graphics pipelines use dynamic rendering formats, dynamic viewport and scissor, and no
    vertex input;
  - compute pipelines are supported;
  - blend presets: opaque, alpha, additive;
  - destroyed on drop.
- **Matrix layout:** Slang is told to use **column-major matrix layout** for every compile
  (`spSetMatrixLayoutMode`). Slang's default is row-major. With this setting, glam matrices
  go into push constants and buffers unchanged, and `mul(M, v)` in shaders matches `M * v`
  in Rust. The shader cache format version was bumped, because the output changes.
- **Projection:** `engine_core::math::vulkan_perspective` (glam's right-handed Vulkan
  projection: depth 0..1, clip-space Y down) produces upright images. Front faces are
  counter-clockwise.
- **`engine_render::mesh`:**
  - `Mesh` stores vertices in memory the GPU reads directly (`MemoryLocation::Dynamic`).
    Staged uploads come with asset streaming.
  - `MeshRenderer` draws with 120 bytes of push constants: clip-from-object matrix,
    normal matrix rows and the vertex buffer address.
  - The shader is precompiled SPIR-V. A test recompiles it and fails if the committed
    file is stale.
  - `engine shaders build --out` writes the per-entry `.spv` files.

## Measurements

- `tests/mesh.rs`: a cube rendered through the render graph (transient color and depth
  targets, then readback), under the validation layer with synchronization validation.
  The centre pixel matches the analytic Lambert value: 204 straight on, 150 turned 45°.
  The background is exact. There are 0 validation errors.
- Bindless capacities on lavapipe: 65 536 sampled and 16 384 storage images.
- ref-1: the same test runs in the GPU workflow on NVIDIA and AMD.

## Consequences

- Materials will be data (texture indices and parameters in buffers), not descriptor
  sets. This fits the material graph → Slang plan (§9).
- The meshlet and GPU-culling path (§6) reuses vertex pulling and the bindless set
  unchanged.
- Buffers need `SHADER_DEVICE_ADDRESS` usage. Our allocator already allocates buffer
  memory with `DEVICE_ADDRESS` (ADR 0010).
- Next: indexed meshes with glTF import (§13), PBR materials, shadow maps, and a
  pipeline cache with warm-up (§9).

## Addendum (2026-10-10): aligned accesses through buffer addresses

**Context.** Shaders read buffers through device addresses (`T*` in Slang), and Slang
declares every such access with the pointee's scalar alignment: an `OpLoad` of a `float4`
or of a 64-byte struct is `Aligned 4`. A driver may then split a 16-byte access into four
4-byte ones. Every vector load of the engine's shaders was so declared (the forward
pass's vertex and fragment shaders, culling, shadows, reflections, the atmosphere,
skinning, the scene's scatter), although the engine lays these buffers out 16-byte
aligned (ring allocations, buffer bases, 16-byte multiples per element). The scene's
motion passes of queue item 0f were the first to declare it.

**Options.**
1. *Leave it to the drivers:* the AMD driver already emits 16-byte loads (no change
   measured on the Radeon); NVIDIA's does not.
2. *A layout with 16-byte aligned vectors* (std430-like) for pointer types: changes every
   struct with a `float3` member (CPU and GPU layouts would have to change together).
3. *Declare it at the accesses (chosen):* `loadAligned<16>(p + i)` / `storeAligned<16>`
   for data laid out 16-byte aligned, whole structs or vector members; a single 4-byte
   value read by a dynamic index stays a plain (scalar) access, whose natural alignment
   is 4.

**Measurements** (ref-1, `scene_bench` Sponza 1920 × 1080, 64 lights of which 4 shadowed,
motion vectors, the depth prepass; main and branch builds in turns, three rounds, µs):

| | main | aligned |
|---|---:|---:|
| RTX: forward (sky) | 336 | 321 |
| RTX: GPU frame (sky) | 626 | 610 |
| RTX: atmosphere pass (atmosphere, fog, probes, SSR) | 56.3 | 44.0 |
| RTX: reflections composite | 128 | 118 |
| RTX: SSR temporal | 61 | 57 |
| RTX: GPU frame (atmosphere, fog, probes, SSR) | 1,478 | 1,445 |
| Radeon: SSR depth pyramid | 413 | 308 |

The other passes (shadows, prepass, skinning, local shadows) did not change, and nothing
else on the Radeon, whose registers stayed the same in every forward pipeline. Turning
the SSR trace's one 4-byte read of a pyramid offset (by a dynamic index, every step) into
a 16-byte load and a component select made the trace 7.5 % slower on both GPUs; the depth
pass, where every thread reads the same offset, is 25 % faster with it on the Radeon. Both
are kept as measured.

**Decision.** Option 3. The engine's shaders declare 16-byte alignment on every access to
such data (`read_*` helpers in `forward.slang`, `read4` and friends in
`reflections_common.slang`, `atmosphere_read4` in `atmosphere_common.slang`); a vector
member of 8 bytes (`uint2`) declares 8.

**Consequences.**
- A new shader reading through buffer addresses declares the alignment the data has
  (`docs/agent-rules.md`, `docs/dev-guide/shaders.md`); `spirv-dis` shows any vector access
  left `Aligned 4`.
- Declaring more alignment than the data has is undefined behaviour: only data laid out
  16-byte aligned may be read so (not packed `float3` positions).
