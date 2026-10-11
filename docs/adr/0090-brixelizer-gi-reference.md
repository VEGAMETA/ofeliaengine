# 0090. Brixelizer GI built from the FidelityFX SDK as probe GI's reference

- Status: Proposed (measurements to come)
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

To be taken with the measurements. Proposed:

- Options 1b, 2a, 3a, 4, 5 (static and dynamic), 6.
- Specular GI is left for the medallions' second step: Brixelizer's specular into the
  reflections' composite.

## Consequences

- A new crate for the SDK; `engine_fsr` loses its build script but keeps its API.
- Brixelizer GI is a screen-space GI: reflection probe captures (ADR 0073) cannot use it, so
  probes captured in a Brixelizer GI view see only the sky's light.
