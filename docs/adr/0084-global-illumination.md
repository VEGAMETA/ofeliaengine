# 0084. Global illumination: our own probe GI traced with ray queries, held to beat Brixelizer GI

- Status: Accepted (option 1, held to beat option 2: the owner, 2026-10-11). The bar was
  measured on 2026-10-11 ([ADR 0090](0090-brixelizer-gi-reference.md)): met on both GPUs;
  Brixelizer GI stays as the reference, not the fallback. Working name of our GI: **Echo
  GI** (the owner, 2026-10-11); the code keeps `ProbeGi`.
- Date: 2026-10-11
- Research: [docs/research/global-illumination.md](../research/global-illumination.md)
- Related: [ADR 0046](0046-image-based-lighting.md) (the sky's light it adds to),
  [ADR 0073](0073-reflection-probes-and-screen-space-reflections.md) (probes that will
  capture its light), [ADR 0064](0064-gpu-scene-extraction-runtime.md) (the scene acceleration
  structures would be built from), [ADR 0060](0060-fsr-3-1-upscaling.md) (how an AMD SDK is built from
  source), queue item 3

## Context

Queue item 3: "GI: probe volumes, then DDGI-like", with an acceptance check: Sponza's
metallic medallions must read gold from above in shade (the shaded floor they reflect has
no bounced light today). The forward shading's indirect light is the sky's alone (nine
harmonics, the prefiltered cube or the reflection probes, ambient occlusion).

Both reference GPUs expose `VK_KHR_ray_query` and `VK_KHR_acceleration_structure`; the
engine builds no acceleration structures yet.

## Options

1. **DDGI probes traced with ray queries** (Majercik et al. 2019 and 2021; NVIDIA's
   application on it was abandoned):
   - acceleration structures: one bottom level per mesh of the geometry pool, a top level
     rebuilt from the GPU scene every frame (skinned meshes later, refitted);
   - a grid of probes over the scene (cascades around the camera later), each with
     octahedral irradiance (6² + border, RGB10A2 or R11G11B10) and hit-distance moments
     (14² + border, RG16F);
   - **phase 1, the probe volume:** probes converged at load and when told (the light
     changed), then kept: no cost per frame;
   - **phase 2, DDGI:** a ray budget per frame (rays spread over probes, hysteresis),
     probe relocation and classification;
   - shading: the 8 probes around a point (trilinear, backface and Chebyshev weights) for
     diffuse indirect light, in entry points of its own (the register rule); reflection
     probes then capture lit surroundings;
   - GPUs without ray queries keep today's sky light (a software tracer is possible later,
     as Wicked Engine's).
2. **AMD FidelityFX Brixelizer GI** (MIT, DirectX 12 and Vulkan, built from source as FSR
   3.1):
   - compute only: sparse distance fields of the triangles traced without ray tracing
     hardware, any GPU;
   - denoised diffuse and specular indirect light at (or below) the render size;
   - needs depth, normals, roughness and motion of every pixel (a G-buffer-like set our
     forward renderer makes only in part: the prepass and the reflections' normal and
     roughness target), and the scene's geometry handed to its SDF builder;
   - a large external library; its cost on the 2-CU iGPU is unknown.
3. **A probe volume filled by rasterized captures** (as reflection probes): no ray
   tracing, any GPU, but six scene views per probe: affordable only once (a baked volume),
   never "DDGI-like". Rejected as the main path.
4. **Per-pixel ReSTIR GI** (Bevy Solari, kajiya): beyond the iGPU's budget. Rejected.

## Recommendation (to confirm)

Option 1, in its two phases: it is what the owner asked for, its probes serve both the
static and the dynamic case with one shading path, the patent risk is gone, and
acceleration structures are the base for later ray-traced shadows and reflections. GI then
needs a GPU with ray queries (every DirectX 12 Ultimate class GPU; both reference GPUs).
Option 2 if GI must run without ray tracing hardware. Measurements (ray budgets and costs
on both GPUs, the medallions from above) come with the prototype.

## Questions for the owner

1. Option 1 (own DDGI, ray queries required for GI) or option 2 (Brixelizer GI, compute,
   AMD's library)?
2. With option 1: acceleration structures become part of `engine_rhi` and the GPU scene
   (an architectural addition).

## Decision

**Option 1, our own probe GI traced with ray queries** (the owner, 2026-10-11: "if you can
make it better, faster and cheaper than AMD's, try"). Held to a bar: on both reference GPUs,
at comparable quality (the medallions from above, light leaking, response to moving
lights), it must cost less GPU time and memory than AMD's Brixelizer GI. Brixelizer GI is
built from the FidelityFX SDK (MIT; the SDK `engine_fsr` already builds) as the reference
measurement, and it is the fallback if ours does not beat it.

## Consequences

- Acceleration structures become part of `engine_rhi` and the GPU scene
  (`VK_KHR_acceleration_structure`, `VK_KHR_ray_query`); GI needs a GPU with ray queries
  (both reference GPUs; others fall back to the sky's ambient light).
- Two phases: a probe volume converged at load, then a ray budget per frame.
- The comparison with Brixelizer GI (cost and memory on both GPUs, the same scenes and
  shots) is part of the acceptance, in `docs/perf/history.md` and the research note.
