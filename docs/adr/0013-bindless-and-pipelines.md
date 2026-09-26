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
