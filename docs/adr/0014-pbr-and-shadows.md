# 0014. Forward PBR, directional shadows, per-frame GPU data

- Status: Accepted
- Date: 2026-09-26
- Builds on: [ADR 0011](0011-render-graph.md), [ADR 0013](0013-bindless-and-pipelines.md)

## Context

CLAUDE.md §17 step 2 asks for basic 3D: meshes, PBR and shadows. The render graph,
bindless set and pipelines exist. Three things were missing:
- shading;
- a place for per-frame and per-draw data (camera, light, draw list; 128 bytes of push
  constants do not fit it);
- a way for passes to sample transient textures (the shadow map) through the bindless
  set.

## Options

### Shading model

1. **Blinn-Phong.** Cheap, but not physically based, and materials would have to be
   re-authored later.
2. **Metallic-roughness PBR, as in Filament** (chosen):
   - GGX distribution, height-correlated Smith-GGX visibility, Schlick Fresnel, Lambertian
     diffuse;
   - the same parameterization as glTF 2.0 (§13), so imported materials map directly.

### Shadows

1. **Cascaded shadow maps** from the start: the right end state for large outdoor scenes,
   but more code before anything is visible.
2. **One directional shadow map fitted to the scene's bounds, 3×3 PCF with hardware
   comparison** (chosen):
   - front-face culling and depth bias against acne;
   - cascades come next, with the same pass structure.

### Per-frame data

1. **Uniform buffers per frame with descriptors.** Conflicts with the bindless-only layout.
2. **Push constants only.** Too small for a camera, a light and a draw list.
3. **An upload ring per frame slot, read through buffer device addresses** (chosen):
   - the GPU counterpart of the CPU frame arena;
   - no descriptors;
   - one pointer in push constants reaches everything.

### Sampling transient textures

1. **The pass registers views in the bindless set itself.** It would repeat the work every
   frame and leak indices.
2. **The render graph registers them** (chosen):
   - a transient image with sampled or storage usage gets its bindless index when its
     cached Vulkan object is created;
   - the index is returned when the object is destroyed;
   - `PassContext::sampled_index` exposes it;
   - imported textures carry their owner's index.

## Decision

- **`engine_render::forward::ForwardRenderer::add_passes`** declares:
  - a **shadow** pass (depth-only, from the light, into a transient shadow map, 2048² D32
    by default);
  - a **forward** pass (PBR plus shadows into the caller's color and depth).
- **Data layout:**
  - frame constants (208 bytes) and draws (160 bytes each) are written into the
    `UploadRing`;
  - push constants (24 bytes) carry the frame and draw-list addresses, the draw index and
    the shadow map's bindless index;
  - struct layouts are checked against the SPIR-V member offsets and asserted in Rust.
- **Light projection:** orthographic, fitted to the bounding sphere of every draw's
  transformed bounds (`Mesh::bounds`).
- **`engine_render::upload::UploadRing`:** one `Dynamic`-memory buffer per frame slot,
  16-byte aligned bump allocation returning device addresses, reset by `begin(&Frame)`.
- **Graph changes:**
  - `FrameGraph::execute_with_bindless(allocator, Some(&bindless))` registers transient
    textures;
  - `RenderGraph::destroy(allocator, Some(&bindless))` returns their indices.
- **`Bindless` now takes `&self` everywhere** (a `RefCell` for index bookkeeping), so the
  graph and renderers can share it during a frame.
- **Pipeline:** `GraphicsPipelineDesc::depth_bias`.
- **Math:** `engine_core::math::vulkan_orthographic`.
- **Tonemapping** (`engine_render::post::Tonemap`):
  - the scene renders into an `R16G16B16A16_SFLOAT` target (`HDR_FORMAT`);
  - a fullscreen-triangle pass applies exposure and the Khronos PBR Neutral tone mapper
    (implemented from its published description) into the display format;
  - it samples the HDR target through the graph-assigned bindless index.

## Measurements

- `tests/forward.rs`, a ground slab and a red cube under a light from above, 128×128,
  lavapipe, validation plus synchronization validation:

  | Point | RGBA | Expected |
  |---|---|---|
  | Ground under the cube (in shadow) | 10, 10, 10 | ambient only: 0.05 × 0.8 |
  | Ground outside the shadow | 209, 209, 209 | lit |
  | Top of the cube | 235, 56, 56 | lit red |
  | Background | 0, 0, 64 | exact clear color |

  0 validation errors. The shadow map's depth-write → sampled-read transition and its
  bindless registration are checked by the sync validator.
- The same scene through HDR and tonemapping gives:
  - shadowed ground 3, 3, 3 (the tone mapper's toe);
  - lit ground 198 (compressed, not clipped);
  - cube top 214, 45, 45;
  - background exact.
- ref-1 (GPU workflow on the PR, commit 9b64af0):
  - `tests/forward.rs` gives the same pixels on the RTX 4060 Ti (NVIDIA 616.92) and on
    the Radeon iGPU (AMD 25.10.36) as on lavapipe, within one step: HDR shadowed ground is
    2 on NVIDIA, 3 elsewhere. 0 validation errors with synchronization validation.
  - The `cubes` example (26 draws, 2048² shadow map, 1280×720, debug build with
    validation), GPU time per pass:

    | Pass | NVIDIA | AMD iGPU |
    |---|---|---|
    | shadow | 11.1 µs | 194.9 µs |
    | forward | 46.1 µs | 1000.3 µs |
    | tonemap | 11.3 µs | 198.2 µs |
- The SPIR-V freshness test now covers every shader in `shaders/`.
- The `cubes` example renders 25 PBR cubes (alternating metal and dielectric, varying
  roughness) over a ground plane with shadows. The GPU workflow runs it on NVIDIA and AMD
  with validation.

## Consequences

- glTF materials (§13) map onto `Material` directly. Textures will add bindless indices
  to `Draw`.
- Next steps:
  - cascaded shadow maps;
  - image-based ambient light;
  - auto-exposure and bloom (post-processing, §6).
- `DrawItem` lists are CPU-side for now. GPU culling and indirect draws (§6) will build the
  same `Draw` records on the GPU.
