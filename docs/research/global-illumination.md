# Global illumination

Research for queue item 3 (GI: "probe volumes, then DDGI-like"), 2026-10-11. Decision:
[ADR 0084](../adr/0084-global-illumination.md) (our own probe GI, held to beat
Brixelizer GI); phase 1: [ADR 0088](../adr/0088-probe-gi-volume.md).

## What is missing today

The forward shading's indirect light is the sky's: nine irradiance harmonics of the
environment (diffuse), its prefiltered cube or the reflection probes and SSR (specular),
darkened by ambient occlusion. Nothing carries light bounced off surfaces:

- **The acceptance check** (status, item 3): Sponza's metallic medallions on the curtains
  (roughness 0.28, metallic 0.74) must read gold seen from above in shade. Today they look
  dark brown there: the shaded floor they reflect gets no bounced light, so the reflection
  probes capture it nearly black.
- **Indoors** the diffuse ambient comes from the sky's harmonics, barely occluded, while
  the probes' specular sees the dark interior (status §6): dielectrics look too bright next
  to metals in shade.

A diffuse GI term that knows about occlusion and bounces fixes both: surfaces in shade
receive the light their surroundings reflect, the reflection probes then capture a lit
floor, and the medallions reflect it.

## Sources (checked 2026-10-11)

| Source | License | How | Notes |
|---|---|---|---|
| Majercik, Guertin, Nowrouzezahrai, McGuire, "Dynamic Diffuse Global Illumination with Ray-Traced Irradiance Fields" (JCGT 8(2), 2019); McGuire's DDGI articles | paper | A 3D grid of probes; each frame some rays per probe (spherical Fibonacci, randomly rotated) are traced, their hits shaded (direct light, plus the probes themselves for further bounces), and blended with hysteresis into small octahedral maps per probe: irradiance (6² + border) and the mean and mean square of hit distances (14² + border). Shading takes the 8 probes around a point, weighted trilinearly, by the backface and by a Chebyshev visibility test on the distances (as variance shadow maps) | The reference design; no light leaks through walls in most cases, infinite bounces over frames |
| Majercik, Müller, Keller, Nowrouzezahrai, McGuire, "Scaling Probe-Based Real-Time Dynamic Global Illumination for Production" (JCGT 10(2), 2021) | paper | Probe relocation out of geometry, classification (probes inside walls or far from surfaces turned off), cascaded volumes around the camera, a sampling of rays proportional to change | The production fixes we need |
| RTXGI-DDGI SDK 1.3 (NVIDIA) | NVIDIA RTX SDKs License (not open source) | DDGI over DXR and Vulkan ray tracing | Read for ideas only |
| Wicked Engine (turanszkij) | MIT | DDGI and surfel GI, by hardware ray tracing or its own GPU BVH ("software ray tracing" for GPUs without it); bordered octahedral irradiance, 16² depth | A software tracer is possible later for GPUs without ray queries |
| Godot 4: SDFGI, VoxelGI, lightmaps | MIT | SDFGI: cascades of signed distance fields around the camera traced by compute, probes per cascade; VoxelGI: a baked voxel scene, cone tracing; lightmaps baked | Works on any GPU; light leaks and noise are known issues of SDFGI |
| AMD FidelityFX Brixelizer GI (FidelityFX SDK 1.1.4) | MIT | Sparse distance fields (cascades of 64³ voxels with "bricks") built from triangles in real time, traced by compute; screen probes and a radiance cache; denoised diffuse and specular outputs. DirectX 12 and Vulkan; GCN and later | A complete compute GI we could build from source as FSR 3.1 (ADR 0060); needs G-buffer-like inputs (depth, normals, roughness, motion) |
| Bevy Solari; kajiya | MIT / Apache-2.0 | ReSTIR DI and GI with hardware ray tracing, world-space radiance caches (hashed grids) | Per-pixel path reuse: far beyond our iGPU's budget |
| Lumen (Epic) talks | closed | Surface cache, distance fields and screen traces | Ideas only |

No code was copied from any source.

## Patents (search 2026-10-11)

- **DDGI:** NVIDIA's application US20210012562A1 ("Probe-based dynamic global
  illumination", claim 1: probes encoded with diffuse irradiance and the mean and variance
  of a distance distribution) is **abandoned**; no other member of its family was found.
- **NVIDIA US11790594B2** ("Ray-tracing with irradiance caches", active until 2041):
  caches stored at geometric locations of the scene (vertices, texture maps), updated by
  rank (distance to the camera, visibility). Avoid irradiance caches attached to surfaces
  with ranked updates; a world-space probe grid is not that.
- Surfel-based GI (EA SEED's GIBS) and Lumen's surface cache were not studied for patents:
  not candidates.

## What the engine has

- Both reference GPUs expose `VK_KHR_ray_query`, `VK_KHR_acceleration_structure` and
  `VK_KHR_ray_tracing_position_fetch` (the Radeon iGPU's two RDNA2 compute units with
  their ray accelerators; slow). `engine_rhi` detects ray tracing but builds no
  acceleration structures. (Since 2026-10-11 it does: ADR 0087,
  [acceleration-structures.md](acceleration-structures.md).)
- The GPU scene (ADR 0064) keeps every instance's transform and mesh; the geometry pool
  (ADR 0039) every mesh's vertices and indices in shared buffers: what bottom-level
  structures are built from.
- Reflection probes (ADR 0073) capture the scene by rasterizing cube faces: a probe volume
  could be filled the same way, at 6 scene views per probe.

## Conclusions

- **DDGI** fits the owner's "probe volumes, then DDGI-like" directly: the same probes,
  first converged once (at load, a volume of static light), then updated every frame on a
  ray budget. It needs acceleration structures (a new part of `engine_rhi` and the GPU
  scene) and GPUs with ray queries (both reference GPUs; others keep today's sky light).
- **Brixelizer GI** is the strongest alternative: compute only, any GPU, AMD-tuned, built
  from MIT source like FSR 3.1; but a large external C++ library, a G-buffer-like input
  set, and screen-space parts that our forward renderer would have to provide.
- **Rasterized probe captures** need no ray tracing but cost six scene views per probe:
  only a frozen (baked) volume would be affordable, never "DDGI-like".
- Per-pixel ReSTIR GI is beyond the iGPU.

## Phase 1: what we took (2026-10-11)

Wicked Engine's DDGI (MIT, commit `cee348b7135d4b6369f85a99ed5e9ed222b71bc7`:
`ShaderInterop_DDGI.h`, `ddgi_raytraceCS.hlsl`, `ddgi_updateCS.hlsl`,
`ddgi_rayallocationCS.hlsl`) was read for ideas next to the two DDGI papers; no code was
copied.

| | DDGI papers | Wicked Engine | Ours (ADR 0088) |
|---|---|---|---|
| Irradiance | 6² + border, per probe | 8², as spherical harmonics (no border) | 6² + border, RGBA16F |
| Distances | 14² + border, mean and mean square | 16² + border | 14² + border, RG16F |
| Rays | fixed count, random rotation | allocated per probe by the irradiance's inconsistency, a tenth outside the camera's view | fixed count (128), random rotation; converged at load |
| Back faces | radiance 0, distance shortened (negative marks it) | the same | the same; double-sided materials shaded on both sides |
| Relocation and classification | relocate out of geometry, turn off probes inside or far from surfaces | relocation (offsets stored as halves) | relocate during two updates, turn off what still sees mostly back faces in the third |
| Shading weights | trilinear, wrapped facing, Chebyshev on distances, crushed faint weights | the same, a smooth back-face switch | the same |
| Where it is read | per pixel, in the shading | per pixel | a resolve at a reduced resolution, upsampled by distance in the shading |

- **For phase 2:** Wicked Engine's ray allocation (more rays where a probe's light
  changed, fewer outside the view) fits a ray budget per frame. It needs the variance of
  each texel's updates (its "inconsistency" buffer).
- **Measured here, not in the sources:**
  - the cost of reading the volume per pixel on a two-CU GPU (14.1 ms at 1080p), hence
    the reduced resolution;
  - the register cost of branching the shading on GI on AMD (runtime branch and
    specialization constant both grow the variants without GI), hence entry points of
    their own.
