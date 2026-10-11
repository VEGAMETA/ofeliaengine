# 0090. Brixelizer GI built from the FidelityFX SDK as probe GI's reference

- Status: Accepted (2026-10-11, with the measurements: probe GI meets ADR 0084's bar on
  both GPUs; Brixelizer GI stays as the reference)
- Date: 2026-10-11
- Research: [docs/research/brixelizer-gi.md](../research/brixelizer-gi.md)
- Related: [ADR 0084](0084-global-illumination.md) (the bar: our GI must beat Brixelizer GI),
  [ADR 0088](0088-probe-gi-volume.md), [ADR 0089](0089-probe-gi-ray-budget.md) (probe GI),
  [ADR 0060](0060-fsr-3-1-upscaling.md) (how FSR 3.1 is built from the same SDK),
  queue item 3

## Context

ADR 0084 holds our probe GI to a bar. On both reference GPUs, at comparable quality, it
must cost less GPU time and memory than AMD's Brixelizer GI. Quality means the medallions
from above, light leaking, and the response to moving lights. Brixelizer GI is built from
the FidelityFX SDK as the reference measurement, and it is the fallback if ours loses.

Brixelizer GI is two effects of the SDK v1.1.4 (MIT), the version and commit FSR 3.1
already comes from (ADR 0060):

- **Brixelizer** builds sparse distance fields from the scene's triangles.
- **Brixelizer GI** traces them from screen probes. It takes:
  - G-buffer-like inputs: depth, normals, roughness and motion vectors, each with last
    frame's;
  - last frame's lit image;
  - blue noise and the environment cube.

  It returns diffuse and specular light at the view's size.

Three facts shape the build:

- **One library.** The SDK's shader blob dispatcher compiles one effect per `#ifdef`, so
  FSR 3.1 and Brixelizer must be one build of the SDK in one binary. Two builds would define
  the Vulkan backend twice.
- **Inputs.** Our forward renderer has no normals before the shading, keeps no history of
  depth, normals or the lit image, and has no blue noise.
- **Units.** Our GI shading reads a texture of irradiance with each pixel's distance,
  written after the depth prepass (ADR 0088).

## Options

1. **Where the SDK is built.**
   a. `engine_fsr` builds every effect and exposes Brixelizer too: no new crate, but a
      GI under "FSR".
   b. **A crate for the SDK, `engine_ffx`.** It holds:
      - the C++ build, with the shim that forwards the backend's Vulkan calls;
      - the shader generator, the generated SPIR-V and blob tables of every effect the
        engine uses;
      - the raw C interface.

      `engine_fsr` keeps FSR's Rust API over it. A new `engine_brixelizer` wraps
      Brixelizer GI.
   c. Two libraries, each with its own backend: duplicate symbols. Rejected.
2. **How the light reaches the shading.**
   a. **A `ScreenGi` trait in `engine_render`.** After the depth prepass, the forward
      renderer asks the view's screen GI for a texture of irradiance and distance. The GI
      shading entry points of ADR 0088 read it at full size. Probe GI is unchanged.
   b. Adding Brixelizer's light in a full-screen pass after the shading: needs each
      pixel's albedo, which a forward renderer does not keep. Rejected.
3. **Normals.**
   a. **From the depth**, as probe GI's resolve takes them: both GIs see the same flat
      surfaces (no normal maps), and nothing changes elsewhere.
   b. A normal target written by the depth prepass: new prepass variants and a cost even
      without Brixelizer. Not for a reference.
4. **The history.** Inverted depth and normals are written by one input pass into two
   persistent pairs of textures, swapped every frame (current and last). The lit image is
   copied after the view's scene passes.
5. **The distance field's cascades.**
   - **Static and dynamic, merged as in AMD's sample: 8 cascades from 0.2 m (24 cascade
     slots).** Our GI follows moving geometry too.
   - Static only (8 slots), measured as Brixelizer's floor.
6. **The internal resolution.** The one our resolve uses (ADR 0088): 50 % on discrete GPUs,
   25 % on integrated ones.

## Measurement protocol

- **Setup.** ref-1, release, 1920 × 1080. Sponza with the sun:
  - the `sponza_with_gi` camera;
  - the medallions from above (`sponza_medallions_with_gi`);
  - a sun turned off.
- **GPU time per frame**, once each GI has converged:
  - **Brixelizer:** the distance field update, the GI dispatch and the inputs (the input
    pass, the lit copy, the conversion to irradiance), each reported apart;
  - **probe GI:** the updates and the resolve.
- **Load-time cost:** every cascade built against the volume converged.
- **Memory:** the SDK's resources, through the backend, plus ours, against the volume and
  the acceleration structures.
- **Picture:** the same shots side by side, diffuse against diffuse. Brixelizer's
  diffuse is scaled to irradiance by an open floor under a uniform sky (irradiance π × the
  sky's radiance).
- **Response:** frames until a sun turned off leaves 10 % of the bounce.

## Decision

- Options 1b (`engine_ffx`, `engine_brixelizer`), 2a (`ScreenGi`, `ViewGi::Screen`), 3a,
  4, 5 (static and dynamic cascades, 8 from 0.2 m), 6.
- **The vendored SDK carries three fixes for AMD's compiler** (`third_party/README.md`):
  the distance field's edge tests take edges from the world-space triangle (AMD's compiler
  left residues where zeros are needed, and the Radeon built Sponza's floor with holes);
  edges stored as halves rounded to the nearest; device-scope fences in the scans'
  last-group handoffs.
- **The GI waits for the distance field.** The first updates build every cascade once (the
  frame indices the SDK's schedule maps to each, the coarsest last); a view's GI runs from
  then on. Otherwise its rays take the sky's light through the unbuilt cascades and the
  caches keep it for hundreds of frames.
- **Views without motion vectors** get the camera's motion from the depth.
- **The SDK's diffuse light is irradiance** (π under a uniform sky of radiance 1): no
  scale.
- Specular GI is left out, as planned for a diffuse comparison.

## Results (2026-10-11, ref-1, `gi_bench`; the research note has the details)

Sponza at 1920 × 1080, the courtyard and the medallions; Brixelizer GI warmed up 256 frames.

| | probe GI, RTX | Brixelizer GI, RTX | probe GI, Radeon | Brixelizer GI, Radeon |
|---|---:|---:|---:|---:|
| GI passes, ms | 0.33–0.34 | 1.55–1.63 | 2.38 | 32.2–33.0 |
| Memory, MiB | 20.4 | 594 | 28.7 | 532 |
| Ready | converged at load: 10 frames, 22 ms of GPU | field in 8 frames; 90 % of its light after 116 | 10 frames, 1.0 s | field 0.85 s; 90 % after 138 |
| Sun turned off: frames to half | 15 | 180 | 123 | 203 |
| Before a sunlit wall, 0.4 m / 4.9 m (physically about 1.14 / 0.20) | 1.19 / 0.20 | 0.22–0.23 / 0.08 | 1.19 / 0.20 | 0.22 / 0.08 |

- **ADR 0084's bar is met** on both GPUs: probe GI costs 4.5 / 14 times less GPU time and
  29 / 18 times less memory, its bounce has the physical size (Brixelizer GI's a fifth:
  its rays read a brick's radiance through a cosine lobe), and on the RTX it follows a
  change of light twelve times sooner.
- **Brixelizer GI is not the fallback** for cost or quality. It stays as the reference:
  `engine_brixelizer`, its tests, the golden shot `sponza_brixelizer_gi` and `gi_bench`.
  On GPUs without ray queries it would be the only one of the two to run.
- **Found for probe GI:** on the iGPU its far probes wait for their turn of the ray budget
  (ADR 0089), so a sun turned off leaves a third of its bounce after 400 frames (Brixelizer
  GI: a quarter). Large changes of light should call `ProbeGi::refresh` (ten frames, as
  at load): queue.

## Consequences

- A new crate for the SDK; `engine_fsr` loses its build script but keeps its API.
- The vendored SDK is no longer unmodified: an update of it carries the three fixes over
  (or drops those AMD fixed).
- Brixelizer GI is a screen-space GI: reflection probe captures (ADR 0073) cannot use it, so
  probes captured in a Brixelizer GI view see only the sky's light.
- Brixelizer GI costs 364 MiB for the distance field whatever the scene and 170–230 MiB
  per view: a reference, not something to ship by default.
