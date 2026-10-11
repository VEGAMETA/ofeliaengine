# Brixelizer GI

Research for the bar of [ADR 0084](../adr/0084-global-illumination.md): our probe GI must
cost less GPU time and memory than AMD's Brixelizer GI at comparable quality, on both
reference GPUs, and Brixelizer GI, built from the FidelityFX SDK, is the reference
measurement and the fallback. 2026-10-11. Decision: [ADR 0090](../adr/0090-brixelizer-gi-reference.md).

## Sources (checked 2026-10-11)

| Source | License | What we read |
|---|---|---|
| AMD FidelityFX SDK v1.1.4, commit `c6efa6bf7f2027b3ec94f28578bb5965eabb9e55` (2025-05-08): Brixelizer 1.0.0 (`sdk/src/components/brixelizer`, `sdk/include/FidelityFX/host/ffx_brixelizer*.h`, `sdk/include/FidelityFX/gpu/brixelizer`), Brixelizer GI 1.0.1 (the same under `brixelizergi`), their Vulkan GLSL passes (`sdk/src/backends/vk/shaders/brixelizer*`), the Vulkan backend (`sdk/src/backends/vk/ffx_vk.cpp`) | MIT | The code we build (ADR 0090), the same SDK and commit as FSR 3.1 (ADR 0060) |
| The SDK's `docs/techniques/brixelizer.md` and `brixelizer-gi.md` | MIT | The algorithm overviews, the resource tables |
| The SDK's sample, `samples/brixelizergi` (`brixelizergirendermodule.cpp`, `shaders/lightinggi.hlsl`, `lightinggicommon.h`, `config/brixelizergiconfig.json`) | MIT | How AMD configures and composites it |
| Lou Kramer, "Real-time Sparse Distance Fields for Games" (GDC 2023, gpuopen.com) | talk | The distance field builder's design |
| Boissé, Guidoux, Tanner et al., "GI-1.0: A Fast Scalable Two-Level Radiance Caching Scheme for Real-Time Global Illumination" (AMD, 2023, gpuopen.com) | paper | The GI that Brixelizer GI simplifies (screen probes over a world-space cache) |

## How it works

### The sparse distance field (Brixelizer)

- **Cascades.** A cascade is a cube of 64³ voxels centered on a point (the camera), and
  each voxel holds at most one brick of 8³ unsigned distances (`R8_UNORM`). All cascades
  share one 512³ atlas: at most 2^18 bricks. A cascade's voxel size is its brick size; AMD's
  sample uses 8 cascades from 0.2 m, doubling (12.8 m to 1.6 km across).
- **The tree.** A three-level AABB tree per cascade (the root, 4³ nodes, 16³ leaves over the
  bricks' bounds) skips empty space.
- **Building.** It is built from triangles by compute passes, from jobs (an instance's world
  bounds overlapping the cascade):
  1. culling the voxels already built;
  2. voxelizing triangles into per-voxel references;
  3. distances per brick from the references;
  4. compressing into the atlas;
  5. an eikonal pass filling the rest.
- **Updates.** The main API updates one cascade per frame (cascade 0 every second frame,
  cascade 1 every fourth, and so on). Static cascades rebuild only voxels that scrolled in
  or were invalidated. A dynamic cascade is rebuilt from its instances at every update and
  merged with the static one into the cascade that is traced.
- **Tracing.** A ray steps through the cascades from the most detailed, walks each
  cascade's tree and sphere-traces the bricks it enters. A hit gives a distance and a brick:
  no triangle, no material.

### The GI (Brixelizer GI)

- **The radiance cache.** It holds 4³ texels per brick (a 256³ `R11G11B10` 3D texture).
  Every frame it takes the previous frame's lit image, reprojected onto the bricks under
  1/16 of the pixels (a rotating 4 × 4 pattern). Only surfaces that were on screen get
  radiance. Off screen, a surface keeps what it last had, or stays black.
- **The screen probes.** One probe per 8 × 8 tile of the internal resolution, each with
  8 × 8 octahedral directions: one ray per internal pixel and frame, traced through the
  distance field. Hits are lit from the radiance cache, misses from the environment cube
  (times an intensity). The probes are reprojected from frame to frame.
- **The irradiance cache.** Second-order harmonics (9 coefficients) per brick, fed by the
  screen probes and spread to neighbouring bricks by a pass over the 64³ voxels of one
  cascade per frame. Pixels whose screen probes fail read it.
- **The outputs.** Diffuse and specular light (the specular from a quarter-resolution
  pre-trace of brick ids, then a trace). They are computed at an internal resolution
  (native, 75, 50 or 25 %), denoised (temporal, then two blur passes) and upsampled to the
  display size.
- **The sample's composite.**
  - Light: `diffuseGI × baseColor × 1.5` and `specularGI × (F0 · A + F90 · B) × 3.0`. The
    factors are ad hoc, the "GI factors" of its UI.
  - Lit history: direct diffuse light plus diffuse GI. With the GI in it, the light gains
    a bounce every frame.

### What it needs from the renderer

| Input | AMD's sample | Our forward renderer today |
|---|---|---|
| Depth and last frame's depth | G-buffer depth, copied each frame | Depth after the prepass; no history |
| Normals and last frame's | G-buffer normals, copied | None before the shading: the probe GI's resolve takes them from the depth; the reflections' normals are written by the shading, too late |
| Roughness | G-buffer | In the shading only |
| Motion vectors | G-buffer | Written by the prepass (current − previous, texture coordinates) |
| Last frame's lit image | Deferred lighting's diffuse output, copied | The view's HDR image, transient |
| Blue noise | 16 textures of 128² RG (Christoph Peters', loaded from files), one per frame | None |
| Environment | The prefiltered IBL cube, intensity 0.1 | The prefiltered cube and its intensity |
| Matrices | View, projection, last frame's, row-vector convention | Column-vector `glam` matrices, Vulkan's Y-down clip space, depth 0 at the near plane |

### Memory (the SDK's constants)

| Resource | Size |
|---|---:|
| Distance field atlas, 512³ `R8_UNORM` | 128 MiB |
| Brick bounds, 2^18 × 4 B | 1 MiB |
| Per cascade: brick map 64³ × 4 B, tree 18 KB | 1.0 MiB (8 cascades static only: 8.1 MiB; static, dynamic and merged as in the sample: 24.4 MiB) |
| Context buffers (counters, brick lists, eikonal and merge lists, instances, transforms) | a few MiB |
| Build scratch | set by the triangle, reference and brick limits (the sample: 1 GiB) |
| GI radiance cache, 256³ `R11G11B10` | 64 MiB |
| GI brick harmonics: two sets of 2^18 × 9 × 8 B, and a state of 2^18 × 16 B | 40 MiB |
| GI screen-sized targets at 960 × 540 (50 % of 1080p): ping-pong probes, GI, specular, downsampled inputs | ~50 MB |

That comes to about 300 MiB before the build scratch. Our probe GI over Sponza holds 13 MB
of probe maps, plus the acceleration structures its rays need (ADR 0087; measured in ADR
0090).

### Things found reading the code

- **Depth.** `FfxBrixelizerGIDepthCloserOp` (`ffx_brixelizergi_common.h`) tests
  `#ifdef FFX_BRIXELIZER_GI_OPTION_DEPTH_INVERTED`. The permutation always defines it (0 or
  1), so with standard depth it picks the farther depth. AMD's sample uses inverted depth.
  We feed it inverted depth too.
- **CPU sizes.** The contexts are large: `FfxBrixelizerContext` is 23.8 MB,
  `FfxBrixelizerBakedUpdateDescription` 8.4 MB, `FfxBrixelizerGIContext` 0.8 MB. Allocate
  them once on the heap, never on a stack.
- **Normalization.** The diffuse output is the harmonics of radiance × cos, evaluated with
  the clamped cosine and divided by 2π; the sample's factor 1.5 makes up for it by eye. We
  scale it to irradiance by measuring a known case: an open floor under a uniform sky,
  whose irradiance is π × the sky's radiance.
- **One library.** The SDK's shader blob dispatcher (`ffx_shader_blobs.cpp`) compiles in
  one effect per `#ifdef`. FSR 3.1 and Brixelizer must therefore be one build of the SDK
  in one binary: two builds would define the backend's symbols twice.
- **Accessor.** The SDK's own blob accessor never copies the blob of
  `FFX_BRIXELIZER_PASS_CASCADE_CLEAR_BRICK_STORAGE` (`ffx_brixelizer_shaderblobs.cpp`
  returns without its `memcpy`). Ours, generated, has no such case.

Found running it (2026-10-11, both GPUs, under the validation layer), worked around in
`engine_ffx`'s Vulkan layer without touching the SDK:

- **The global descriptor pool** (`ffx_vk.cpp`, `CreateBackendContextVK`) lists six
  descriptor types but is created with `poolSizeCount = 5`, which leaves out the storage
  buffers. FSR 3.1 never needed them; Brixelizer's passes do. Drivers allocated anyway,
  but the layer warns that they may not.
- **The bindless pool** has no `FREE_DESCRIPTOR_SET_BIT`, but its sets are freed one by
  one when an effect is destroyed.
- **Buffers made through the backend** get the transfer usages only if initialized, and
  the SDK copies from the build's scratch to read its counters back. Initializing one races
  with the first copy, so the scratch is created in the copy-source state.
- **Measured sizes.** A distance field context with our limits (1 M references, 16 MB of
  triangle swap, 16,384 bricks a build: 76.6 MB of scratch) holds 255 MiB of GPU memory. A
  GI context at 64 × 64 holds 104 MiB, almost all of it the radiance cache and the brick
  harmonics, which do not depend on the view's size.

## Patents (search 2026-10-11)

A web search found no AMD patent or application on Brixelizer's sparse distance fields
(bricks in cascades over a shared atlas) or on Brixelizer GI's and GI-1.0's caches. The MIT
license grants no patent rights. Google Patents' result pages could not be read from here,
so the search is not conclusive. This matters only if Brixelizer GI ships as the fallback
(ADR 0084); a proper search then goes to the humans.

## What we take

- **Build.** The SDK's Brixelizer and Brixelizer GI, built unchanged as FSR 3.1 is (ADR
  0060):
  - the C++ compiled with `cc`;
  - the GLSL compiled to SPIR-V by our generator with the SDK's glslang 11.12.0;
  - the shader blobs served from our tables.
- **Inputs.** What our renderer lacks is computed for it, as cheaply as we can:
  - normals from the depth (the same normals our probe GI's resolve uses, so both GIs see
    the same surfaces);
  - inverted depth;
  - a pair of textures for this frame and the last;
  - a copy of the lit image;
  - a generated blue noise tile.
- **Configuration.** AMD's sample, with fair settings:
  - the internal resolution our resolve uses (50 % on discrete GPUs, 25 % on integrated
    ones);
  - static and dynamic cascades, since our GI follows moving geometry too.

## What we leave

- Specular GI in the first comparison, which is diffuse against diffuse. Our specular comes
  from the reflection probes and screen-space reflections. Brixelizer's specular can feed
  the reflections' composite later, for the medallions.
- The debug visualizations, and the raw API (the main API is what AMD recommends).
