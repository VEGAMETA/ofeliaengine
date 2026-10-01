# 0046. Image-based lighting from an environment

- Status: Accepted
- Date: 2026-09-30
- Builds on: [ADR 0040](0040-materials-and-textures.md) (the BRDF),
  [ADR 0045](0045-local-lights.md) (specialization constants for shading variants)
- Research: [`docs/research/image-based-lighting.md`](../research/image-based-lighting.md)

## Context

The ambient term was a flat color times albedo:
- metals went black away from the sun;
- the background was the clear color.

PBR materials need light from the whole environment, diffuse and specular.

## Options

**Specular:**

1. Importance-sampling the environment per pixel: exact and far too slow.
2. **Split sum (Karis 2013) (chosen):**
   - a cube prefiltered per roughness (mips);
   - a DFG table in Filament's form;
   - Filament's multiple-scattering compensation.

**Diffuse:**

1. A small irradiance cube: a texture fetch per pixel.
2. **9 spherical harmonics (Ramamoorthi and Hanrahan 2001) (chosen):**
   - 144 bytes, read with loads shared by the wave, 27 multiply-adds;
   - the same accuracy for low-frequency skies.

**Where the environment comes from:**

1. HDR images: no decoder in the engine yet.
2. **A three-color sky (zenith, horizon, ground) filled on the GPU (chosen, first):**
   - the same cube a physically based atmosphere (Hillaire 2020, next) will fill.

## Decision

- **`engine_render::environment::Environment`**, shared across frames:
  - a 128² radiance cube with its mip chain (a 2 × 2 average per mip);
  - a 128² prefiltered cube with 6 mips, perceptual roughness mip / 5 (GGX importance
    sampling, 64 filtered samples);
  - 9 irradiance coefficients (projected from the 16² mip);
  - a 64² RG16F DFG table (512 samples, once).
  - They are rebuilt in compute passes of the frame whose sky changed:
    - "environment sky" (writes the cube, then its mips through storage views, with
      barriers between mips);
    - "environment prefilter";
    - "environment irradiance";
    - "environment dfg" on the first frame.
  - `set_sky`, `set_intensity`, `set_debug_directions` (the cube shows directions, for
    checking orientation).
- **`ForwardView::environment: Option<&Environment>`.** With one:
  - the ambient term becomes diffuse × SH irradiance × occlusion + prefiltered(reflect,
    roughness) × mix(dfg.x, dfg.y, f0) × (1 + f0 (1/dfg.y − 1)) × specular occlusion;
  - specular occlusion (Lagarde 2014) is evaluated only where the material's occlusion is
    below 1: it equals 1 otherwise, and skipping it takes the shader from 72 to 64 VGPRs
    on the Radeon;
  - a "sky" pass draws the cube and the sun's disk (0.27°, the light's illuminance over
    its solid angle) where depth is still 1;
  - the forward pass then keeps its depth.
  - Without an environment nothing changes.
- **Shaders:** `TextureCube` and `RWTexture2DArray` bindless arrays alias bindings 0 and 1.
  A cube's sampled view is a CUBE view, its storage views are 2D arrays, one per mip.
- **Specialization constant `ENVIRONMENT`** (next to `LOCAL_LIGHTS`): four shading
  variants. Frames without an environment pay nothing for it.
- **Pipeline statistics:**
  - `ENGINE_PIPELINE_STATS=1` enables `VK_KHR_pipeline_executable_properties`;
  - `Pipeline::statistics` returns the driver's numbers;
  - `ForwardRenderer::shading_statistics` gives them per variant;
  - `scene_bench --pipeline-stats` prints them.
  They are how this ADR's register numbers were measured.

## Measurements

ref-1, release, Sponza textured / factors only, 1920 × 1080, 4 shadow cascades, GPU µs
(medians; RTX over 3 runs).

| | RTX forward | Radeon forward | Radeon pixel shader VGPRs |
|---|---:|---:|---:|
| no environment | 317 / 213 | 7,734 / 4,617 | 48 |
| environment | 396 / 309 | 12,194 / 6,607 | 64 |
| environment + local lights | – | – | 80 (local lights alone: 64) |

- **Sky pass:** 3.1 µs on the RTX, 32.5 µs on the Radeon.
- **Rebuilding the environment** (only in a frame whose sky changed): 49 µs on the RTX,
  1.29 ms on the Radeon.
- **The cost is the lighting itself:** with the environment bound but a trivial ambient
  term, the forward pass is back at 317 / 7,701 µs.
- **Occupancy decides on the iGPU:**
  - its pixel shaders run in waves of 64: 48 VGPRs give 10 waves per SIMD, 64 give 8, 72
    gave 7;
  - measured variants:
    - specular occlusion only where needed: 72 → 64 VGPRs, 13,540 → 12,194 µs;
    - the image-based light computed before the shadow lookup: 85 VGPRs, 15,715 µs (not
      taken);
    - an analytic DFG instead of the table: 72 VGPRs, 13,158 µs (not taken).

**Correctness** (`tests/environment.rs`, under synchronization validation, both GPUs):

- **White furnace** (radiance 1 everywhere, no sun), measured against the CPU:

  | material | rendered | expected |
  |---|---:|---:|
  | white rough dielectric | 1.0146 | 1.0134 |
  | grey rough dielectric | 0.5142 | 0.5134 |
  | white rough metal | 1.000 (0.999 on the Radeon) | 1 |
  | grey metal, roughness 0.5 | 0.4790 | 0.4790 (the DFG integral computed here) |
  | smooth white metal | 1.000 | 1 |
  | background | 1.000 | 1 |

- **Gradient sky:** the diffuse light on an upward and a front-facing face matches the
  irradiance integrated on the CPU within 3% of the brightest channel. The worst is the
  red channel facing up, 4% high: 9 harmonics round the gradient's kink at the horizon.
- **Debug sky:** looking along ±X, ±Y, ±Z shows those directions, and "up" in the image
  is up in the world.

## Consequences

- **Scenes get sky light and a sky** with one object and one field.
- **Not done yet:**
  - a physically based atmosphere (Hillaire 2020) to fill the cube: next (done:
    [ADR 0069](0069-sky-atmosphere-and-fog.md));
  - HDR environment images;
  - local reflection probes;
  - one environment shared by several views in one frame (each view imports it; split
    screen needs one per view for now);
  - an empty draw list draws no sky.
- **Next, from measurements:**
  - half-precision shading math to cut registers on the iGPU (80 VGPRs with lights and
    environment);
  - measure horizon occlusion.
