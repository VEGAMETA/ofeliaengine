# 0088. Probe GI, phase 1: a volume of probes converged at load

- Status: Accepted
- Date: 2026-10-11
- Research: [docs/research/global-illumination.md](../research/global-illumination.md)
- Related: [ADR 0084](0084-global-illumination.md) (the decision: our own probe GI, held
  to beat Brixelizer GI), [ADR 0087](0087-acceleration-structures.md) (the structures its
  rays trace), [ADR 0058](0058-ambient-occlusion.md) and
  [ADR 0060](0060-fsr-3-1-upscaling.md) (the register rule),
  [ADR 0065](0065-async-compute.md) (the fence found here),
  [ADR 0073](0073-reflection-probes-and-screen-space-reflections.md) (probes that capture
  its light), [ADR 0046](0046-image-based-lighting.md) (the sky's light it replaces),
  queue item 3

## Context

ADR 0084 chose a DDGI-like volume of probes traced with ray queries, in two phases.
Phase 1, this record: probes converged at load (and when told), then kept, so later
frames pay only for reading them. Phase 2 will trace a ray budget every frame. The
acceptance check is Sponza's metal medallions on the curtains, seen from above in shade:
they must read gold, which needs a lit floor for them (and the reflection probes) to
reflect.

The forward renderer shades once per pixel after a depth prepass (equal depth test). Its
shading variants are entry points with literal features. On the Radeon a feature behind
a runtime branch or a specialization constant still costs registers in every variant
(CLAUDE.md §5, ADR 0058, ADR 0060).

## Options

1. **Where the volume's light is read.**
   a. In the forward shading itself: eight probes per pixel, each with two filtered reads
      and a Chebyshev weight, inside every GI variant.
   b. **A resolve pass** after the prepass writes each pixel's irradiance from the depth
      (its position and normal); the shading reads it.
   c. A resolve pass at reduced resolution (one sample per 2 × 2 or 4 × 4 pixels), then
      a full-resolution pass that upsamples it, edge-aware, for the shading.
   d. **A resolve pass at reduced resolution, upsampled in the shading itself**: each
      pixel reads the four samples around it, weighted by how close their surfaces'
      distances are to its own.
2. **How the shading branches on GI.**
   a. A runtime branch on a push constant.
   b. A specialization constant.
   c. **Separate entry points** (34 GI twins of the variants used after the prepass).
3. **Hit shading.** DDGI shades a ray's hit with direct light and the volume's own light
   at the hit (the next bounce). A material's textures at the hit cost reads; its average
   color is enough for diffuse light.

## Measurements

ref-1, release, 2026-10-11, Sponza at 1920 × 1080 (`scene_bench --gi`). 8,680 probes
(31 × 14 × 20, about 1 m apart), 128 rays each per update, 11 updates (3 bounces, then
8 averaged), the coarsest bottom levels:

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| Converging, 11 updates: trace / irradiance / moments / relocation | 7.37 / 1.41 / 6.18 / 0.05 ms | 710 / 116 / 288 / 14 ms |
| Resolve, one sample per 1 / 4 / 16 pixels (scale 1 / 2 / 4) | 303 / 91 / 44 µs | 14.1 / 3.5 / 1.5–1.7 ms |
| Forward shading: without GI / GI, 4 reads / GI, 1 read where it can | 236 / 276 / 264 µs | 9.72 / 10.48 / 9.96–10.00 ms |
| Per frame at the default scale (2 / 4): resolve + shading | 91 + 28 µs | 1.5–1.7 + 0.24–0.29 ms |
| Memory kept: irradiance + moments + probes | 4.2 + 8.5 + 0.13 MiB | same |
| Memory while converging: the rays (a transient buffer) | 8.5 MiB | same |

- **1a** costs the same probe reads as **1b** at full resolution, inside the shading
  (registers in every GI variant). **1b** at full resolution costs 14.1 ms on the iGPU.
  **1c** adds a full-resolution pass (the depth and four samples read, a full-size
  texture written per pixel): slower than 1d on the iGPU, dropped. **1d** at a scale of
  4 costs 1.5–1.7 ms there in all.
- The shading's four reads are what costs: returning a constant costs nothing (236 µs on
  the RTX), one read 252 µs, four reads with distance weights 276 µs. Reading the four
  samples' filtered value first, and the four only where its distance is more than 1 %
  away from the pixel's, takes 264 µs on the RTX and saves 0.5 ms on the iGPU. The
  picture changes at silhouettes only: a mean of 0.02 and at most 27 levels in 8-bit
  sRGB over the medallion view, in 0.1 % of its pixels.
- **Registers** (Radeon, pixel shader VGPRs; `scene_bench --pipeline-stats`):

  | variant | without GI | 2a, a runtime branch | 2b, a specialization constant | 2c, GI twin |
  |---|---:|---:|---:|---:|
  | plain | 48 | 48 | 48 | 48 |
  | local lights | 64 | 64 | 64 | 65 |
  | environment | 64 | 71 | 70 | 53 |
  | lights + environment | 80 | 80 | 80 | 75 |
  | decals + environment | 64 | 80 | 64 | — |

  With 2a and 2b the variants *without* GI grew (64 → 71 and 70, 64 → 80). With 2c they
  are as before, and the GI twins of the environment variants are smaller, since the
  sky's harmonics drop out.
- **Accuracy** (`tests/render/gi.rs`, both GPUs, 0 validation and synchronization
  validation errors):
  - A white furnace (uniform sky) gives π L within 2 %.
  - Inside a closed black box: under 5 % of the sky's light.
  - A sunlit grey floor lights a surface above it: 1.35–1.55 for 1.5.
  - A black trench 4 wide and 4 deep: its floor 1.46 against 1.40 for an infinite slot's
    view factor; a wall facing across 0.85 against 0.87.
  - A double-sided glowing box seen from inside: π e exactly.
- **Sponza:** the courtyard's floor in shade gets about a tenth of the irradiance the
  sky's harmonics gave it (it sees a narrow strip of sky). Shade is darker than before,
  and lit by what the sunlit floor and walls reflect.

## Decision

Options **1d, 2c** and hit shading at a coarse mip, as follows (`engine_render::gi`,
`gi.slang`).

- **The volume:** `ProbeGrid::covering(min, max, spacing, max_probes)` puts probes on a
  grid over a box (at most 32,768; the spacing grows to fit). Each probe has two maps,
  each inside a one-texel border (bilinear filtering across the octahedron's seams):
  - irradiance in 6 × 6 octahedral texels (RGBA16F: a running mean of eight updates keeps
    its precision);
  - the mean and mean square of hit distances in 14 × 14 texels (RG16F).

  A probe's offset from its rest position and its state live in a buffer.
- **Updates** (`ProbeGi::add_update_passes`, declared by the forward renderer's
  `add_scene_passes` while the volume converges):
  - **Trace:** one ray per thread, in spherical Fibonacci directions turned by a random
    rotation per update. The ray traces the opaque and masked instances (forced opaque).
    A front-face hit (or a back face of a double-sided material) is shaded with its
    material at mip 5:
    - albedo times the base-color texture;
    - glTF metalness, the factor times the texture's blue channel;
    - emission;
    - the sun, with a shadow ray;
    - after the first update, the volume's irradiance at the hit (one more bounce per
      update).

    A miss sees the sky: the environment's cube times its intensity, or the flat ambient
    radiance. A back-face hit keeps zero light and its distance, negated (the probe is
    inside something).
  - **Blend:** irradiance and moments are blended per texel from the update's rays. The
    first three updates replace the maps (each adds a bounce); the next eight are
    averaged. A back face counts as near (a fifth of its distance).
  - **Relocation:** updates 0 and 1 move probes out of geometry (through the nearest
    back face) or away from surfaces they nearly touch, within 45 % of a cell. Update 2
    turns off probes that still see mostly back faces.
  - The rays live in a transient graph buffer of the frames that update; nothing is
    traced once converged. `ProbeGi::refresh` converges again.
- **Resolve** (`GiFrame::add_resolve_pass`, after the prepass):
  - One sample per `GiSettings::resolve_scale`² pixels. `GiSettings::for_device` gives 2
    on discrete GPUs and 4 on integrated ones.
  - Each sample is taken at its block's nearest surface on one sample and its farthest
    on the next, in a checkerboard, so both sides of an edge have samples.
  - Its normal comes from the depth.
  - It is lit by the eight probes around it, offset from the surface along the normal
    and towards the eye. The probes are weighted trilinearly, by facing (a wrap, never
    zero) and by Chebyshev's bound on their distances; faint weights are crushed.
  - Outside the volume the sky's harmonics take over across its last cell.
  - Each sample stores the irradiance (rgb) and the surface's distance from the eye (a;
    0 for the sky).
- **Shading:** 34 GI twins of the entry points used after the prepass (the four
  variants × decals, ambient occlusion, contact shadows, local shadows; reflections'
  outputs; subsurface materials).
  - They take the diffuse ambient light from the samples: their filtered value where its
    distance is within 1 % of the pixel's, else the four around the pixel weighted
    bilinearly and by distance (no light across depth edges).
  - The specular light stays the environment's.
  - Not in multisampled views. A view reads the volume from its first update on.
  - Reflection probes capture the scene lit by the volume (their views read it without
    updating it).
- **`engine_frame`:** `SceneInput::gi` hands a view the volume and the frame's ray scene.
  GPUs without ray queries cannot make one and keep the sky's light.
- **The frame graph** (found here, merged as PR #166, ADR 0065 addendum): the texture
  pool's upload pass is a fence. Before the fix, the GI's first update after loading ran
  before the textures' copy and saw every material texture black.

## Consequences

- Sponza at 1080p, at the default scale, costs per frame about 0.12 ms on the RTX
  4060 Ti and 1.8 ms on the two-CU Radeon. Converging costs 15 ms on the RTX and 1.1 s
  on the Radeon, as 11 frames of about 100 ms with the default one update per frame.
  This is a load-time cost; phase 2 spreads rays over frames.
- Memory: 1.5 KiB per probe kept (Sponza: 12.8 MiB), and 1 KiB per probe while
  converging.
- Light is frozen once converged. Moving objects and lights, opened doors and a sun that
  sets need `refresh()` until phase 2.
- **Convergence sees what is resident.** A texture not uploaded counts as white. Hand a
  view `rays: None` until the scene's textures are resident (the golden shots do), or
  refresh once they are.
- **Known limits:**
  - The resolve's normal comes from the depth, so low-poly curved surfaces show facets
    of light. A later fix: the prepass's normals, or SH per sample.
  - Walls thinner than about a tenth of the spacing can leak.
  - Alpha-tested surfaces are solid to the rays.
  - Transparent surfaces do not take part.
- **Tests:** `tests/render/gi.rs` (above), `zero_alloc/gpu_scene.rs` with a volume (0
  heap allocations in 100 frames that update it and then only read it, both GPUs).
  Golden shots `sponza_gi` and `sponza_gi_medallions` on both GPUs: deterministic, FLIP 0
  between runs.
- **Perf gate scene `sponza_gi`** (the frame renderer, through the runtime, 1080p):
  - RTX: GPU frame 788 µs (resolve 101, forward 311) against `sponza`'s 708.
  - Radeon: 19.4 ms against 22.9. The depth prepass that GI turns on removes the
    shading's overdraw (forward 17.2 → 11.9 ms), more than the GI costs.
- **Next:**
  - Phase 2: a ray budget per frame, rays allocated by the probes' inconsistency (Wicked
    Engine's idea) and by the camera's view.
  - Brixelizer GI built from the FidelityFX SDK as the reference: cost and memory on
    both GPUs, the same shots. ADR 0084's bar: ours must cost less.
