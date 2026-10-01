# 0069. Physically based sky and atmosphere, aerial perspective and exponential height fog

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0046](0046-image-based-lighting.md) (the environment and its image-based
  light, which named a physical atmosphere as next), [ADR 0052](0052-compute-passes.md),
  [ADR 0065](0065-async-compute.md), [ADR 0062](0062-golden-images-and-perf-gate.md)
- Research: [`docs/research/atmosphere.md`](../research/atmosphere.md)

## Context

The sky was a three-color gradient filling the environment's radiance cube; the sun a
directional light of fixed color; no aerial perspective, no fog (see the research note).
The register rule (agent rules; ADR 0058's measurements) forbids adding code to the forward
shading of views that do not use a feature: on the Radeon iGPU opaque shading sits at
exactly 64 VGPRs in the environment variant.

## Options

**Atmosphere model:**

1. Bruneton 2017 precomputed 4D scattering: accurate, but slow to rebuild (no run-time
   changes), horizon artifacts, awkward aerial perspective.
2. Analytic skies (Preetham, Hosek-Wilkie): cheap, Earth from the ground only, poor
   sunsets, no aerial perspective.
3. **Hillaire 2020 (chosen):** four small tables, the multiple scattering approximation,
   rebuilt in well under a millisecond on the RTX, from any altitude, with aerial
   perspective; MIT reference code; Unreal and Bevy ship it.

**Where aerial perspective and fog meet opaque surfaces** (measured, Sponza 1920 × 1080,
µs, forward pass / atmosphere pass):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| none (sky pass) | 395 / 3 | 12,214 / 33 |
| in the forward shading (an environment+atmosphere variant) | 433 / 15 | 15,966 / 369 |
| same, with fog | 461 / 15 | 17,881 / 381 |
| **a full-screen pass after the opaque surfaces (chosen)** | 395 / 34 | 12,250 / 1,056 |
| same, with fog | 394 / 57 | 12,228 / 1,981 |

- In the forward shading it costs every shaded fragment (overdraw) and registers: Radeon
  pixel shader VGPRs 64 → 80 (environment) and 80 → 90 (with lights).
- A separate pass costs one read of the depth and a blend per pixel, and nothing at all to
  views without an atmosphere: opaque shading keeps exactly its code (48/64/64/80 VGPRs).
- The same pass draws the sky where depth is 1, replacing the "sky" pass for these views.

**Composite pass, measured on the Radeon (aerial perspective only):** a pass that
discards everything 118 µs; + the depth read 365; the first full version 1,457 (two
matrix transforms per pixel); rays as `base + ndc.x · x + ndc.y · y` and the view depth
from `p32 / (depth + p22)` 1,056; the aerial perspective's lookup skipped within the
distance where it adds under 1e-5 and hides under 0.1 % (computed on the CPU per frame);
pixels with nothing visible to add discarded (no blend). Fog with a branch on its start
distance cost 2,330; branch-free 1,772 (fog over the gradient sky).

**Aerial perspective froxels (32³):** one thread per column marching 32 slices: 26.8 µs
(RTX); one thread per froxel, each integrating its slice's segment, then a parallel
prefix scan per column: 12 µs (RTX), 63 µs (Radeon); Hillaire's per-froxel full march is
16× the steps.

**Volumetric (froxel) fog:** at about 1 ns per froxel step on the Radeon (the aerial
perspective's rate), a 160 × 90 × 64 grid is ≥ 0.9 ms before its shadow lookups, scan and
temporal filter, on top of the 2 ms the composite pass already costs there. **Deferred.**

## Decision

- **`engine_render::atmosphere`**: `Atmosphere` (planet and atmosphere radii; Rayleigh,
  Mie, ozone; ground albedo; multiple scattering scale; sun's angular radius; the world's
  altitude and metres per unit; aerial perspective distance scale; `drives_sun`),
  `Atmosphere::earth()` the default, CPU `transmittance` / `sun_light`; `HeightFog`
  (density at a base height, falloff, albedo, sun anisotropy, start distance, maximum
  opacity) with CPU `optical_depth` / `transmittance`.
- **They belong to the `Environment`** (as Godot's environment holds sky and fog):
  `Environment::set_atmosphere(.., Option<Atmosphere>)` (creates the tables and five
  compute pipelines the first time; an environment that never uses it pays nothing) and
  `set_fog(Option<HeightFog>)`. `ForwardView` and the gradient `Sky` API are unchanged;
  the default is unchanged (gradient, no fog), so existing images are identical.
- **Tables (RGBA16F), compute passes marked async (ADR 0065):**
  - transmittance 256 × 64 and multiple scattering 32 × 32: rebuilt when the atmosphere's
    parameters change;
  - sky view 192 × 108: when the sun's elevation moves by 0.02° or the camera's altitude
    by 10 m (the azimuth is free: the table is relative to the sun's);
  - the environment's radiance cube drawn from the sky-view table (then its mips,
    prefilter and harmonics as in ADR 0046): when the sun moves by 0.1° or its color
    changes, or the camera's altitude by 100 m;
  - aerial perspective 32³ every frame.
- **The sun:** with `drives_sun` (default), the directional light's color is taken as the
  light outside the atmosphere; the shading, the sky and the fog use it times the
  transmittance towards the sun at the camera (CPU, 512 steps, cached while nothing
  changes), times the part of the disk above the horizon. The sun's disk (its angular
  radius) is drawn with that color over its solid angle.
- **One full-screen pass, "atmosphere"**, after the opaque and subsurface surfaces and
  before the transparent ones, with premultiplied blending: target = light + target ×
  transmittance. It samples the depth (no depth attachment).
- **Transparent surfaces** under an atmosphere or fog use their own entry points
  (`fragmentBlendedAtmosphere`, `…Reactive`) in four pipelines made with the others; the
  other shading entry points do not change.
- **Fog's light:** the sky's average radiance (band 0 of the environment's harmonics) and
  the sun at the camera through Henyey-Greenstein; not shadowed.
- Units: the atmosphere in km, the scene in world units (`meters_per_unit`, default 1).

## Measurements

ref-1, release, `VK_LOADER_LAYERS_DISABLE=~implicit~`, 1920 × 1080.

**Per frame** (Sponza, `scene_bench --atmosphere [--fog]`, µs):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| forward, gradient sky → atmosphere (+ fog) | 396 → 395 (394) | 12,214 → 12,250 (12,228) |
| sky pass → atmosphere pass | 3.1 → 34 | 33 → 1,056 |
| atmosphere pass with fog / fog over the gradient sky | 57 / 54 | 1,981 / 1,772 |
| aerial perspective | 10–12 | 63 |
| CPU `add_passes` | 6 → 6 | – |

Perf gate (`sponza_atmosphere`, new scene; GPU frame end to end): RTX 803 µs (Sponza 730),
Radeon 18,509 µs (Sponza 16,454).

**Rebuilds** (`--atmosphere-every-frame`, µs RTX / Radeon): transmittance 5 / 87, multiple
scattering 24 / 710, sky view 11 / 280, sky into the cube 3 / 55, then the environment's
mips 10 / 27, prefilter 32 / 621, harmonics 10 / 621 (ADR 0046's passes).

**Registers** (Radeon, `ENGINE_PIPELINE_STATS=1 scene_bench --sky --pipeline-stats`): opaque
shading unchanged at 48/64/64/80; atmosphere pass 35; transparent under atmosphere 78, with
lights 91.

**Correctness** (`tests/atmosphere.rs`, sync validation, both GPUs identical to 4 digits):

- transmittance table vs 512 steps in double precision: largest error 0.0029;
- sky-view table (single scattering, 54 texels across zenith, horizon, ground, azimuths)
  vs 2,000 steps: within 2.3 %;
- aerial perspective froxels vs 2,000 steps: light within 0.3 %, opacity within 0.0003;
- multiple scattering table vs Hillaire's approximation recomputed with 5× the steps:
  within 1.7 %; f_ms < 1; multiple scattering brightens the zenith by 1.36 / 1.44 / 1.67
  (r, g, b);
- the sun: table vs CPU at 60° and 4° within 0.2 %; noon (0.94, 0.86, 0.75) vs 4°
  (0.52, 0.22, 0.05); set below the horizon: 0;
- the composite pass: an emissive panel 40 km away (aerial scale) = panel × (1 − opacity) +
  light, from the froxels as the hardware filters them, within 1 %;
- height fog: a panel and the sky through the fog = analytic transmittance and
  in-scattering, within 1 %; CPU optical depth vs 100,000-step integration within 0.1 %.

Golden images (both GPUs): the procedural scene at noon, at sunset (aerial scale 2,000)
and with height fog. Existing shots identical. Zero heap allocations per frame with a
moving sun, fog and transparent surfaces (`tests/atmosphere_zero_alloc.rs`); the GPU-scene
path draws the same bytes as the draw-list path under the atmosphere and fog
(`tests/gpu_scene.rs`).

## Consequences

- Scenes get a sky that follows the sun, sunlight that reddens and sets, sky light that
  matches the sky, aerial perspective and height fog, with two calls.
- Views without an atmosphere or fog are unchanged (pipelines, registers, images, passes).
- The full-screen pass costs about 1–2 ms on the iGPU; most is per-pixel blending and the
  depth read. Next, if measured worthwhile: fog and aerial perspective at half resolution
  with a depth-aware upsample, or folded into another full-screen pass.
- On the Radeon (no async compute by default) the per-frame aerial perspective overlaps the
  culling passes, whose timestamps then read about as long as it (63 µs): an attribution
  artifact; the frame end to end grows by the passes' sum.
- **Not done:** volumetric (froxel) fog with shadowed sunlight (deferred, above); light
  shafts; clouds; the planet's shadow on the aerial perspective from below the horizon is
  approximate (from the froxel's center); one environment per view per frame (as ADR 0046).
- **Patent note:** US 9,582,929 B2 (dynamic skydome; sky color in the vertex shader or
  objects lit in one pass) — not what we do; for the team to confirm.
