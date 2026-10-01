# Sky, atmosphere and fog

Checked 2026-10-01. Sources, versions and licenses at the end. Code adapted from
Hillaire's MIT reference implementation (and, through it, Bruneton's BSD-3 parameterization)
is listed in `THIRD_PARTY_NOTICES.md`; nothing else was copied.

## Why

The sky was a three-color gradient (`Sky { zenith, horizon, ground }`, ADR 0046) filling the
image-based light's cube, and the sun a directional light of fixed color:

- the sky did not follow the sun: no sunset colors, no glow around the sun, the same blue
  at any time of day;
- the sun's light stayed white down to the horizon;
- distant geometry looked as close as near geometry (no aerial perspective);
- there was no fog.

A physically based atmosphere gives all of these from a few physical parameters, and the
same model lights the scene (sun color, sky light) and draws the sky, so they agree.

## Atmosphere models

### Bruneton and Neyret 2008, Bruneton 2017 (BSD-3)

"Precomputed Atmospheric Scattering" (EGSR 2008) and its rewritten implementation
(`ebruneton/precomputed_atmospheric_scattering`, 2017, BSD-3-Clause):

- precomputes single and multiple scattering into a 4D table (altitude, view zenith, sun
  zenith, view-sun angle), stored as a 3D texture (32 × 128 × 32 × 8 in the 2017 code),
  plus transmittance (256 × 64) and ground irradiance tables;
- multiple scattering by iterating orders: each order a full pass over the 4D table;
- accurate (spectral rendering is possible, the reference for comparisons), but:
  - the precomputation takes milliseconds to seconds; changing the atmosphere at run time
    (weather, other planets) is expensive;
  - the 4D table is low-resolution in some dimensions: artifacts at the horizon and in
    the shadow of the planet, and the view-sun angle's parameterization is approximate;
  - aerial perspective needs extra work (the table is sampled twice per pixel and
    subtracted, which loses precision near the camera).
- What we take: its **transmittance table parameterization** (distance to the top
  of the atmosphere mapped linearly between its extremes, altitude as ρ / H) and its
  **parameters for Earth** (Rayleigh, Mie and ozone coefficients integrated over the
  visible spectrum, scale heights, ozone tent at 25 km ± 15 km).

### Hillaire 2020 (MIT reference implementation)

"A Scalable and Production Ready Sky and Atmosphere Rendering Technique" (EGSR 2020;
Unreal Engine's Sky Atmosphere; reference implementation `sebh/UnrealEngineSkyAtmosphere`,
MIT):

- **Transmittance table** (256 × 64), Bruneton's parameterization, 40 steps per texel.
- **Multiple scattering table** (32 × 32, altitude × sun zenith): the key idea. Light
  scattered two or more times is nearly isotropic and varies slowly, so for each texel:
  - integrate second-order light L₂ over 64 directions of the sphere, assuming isotropic
    scattering (equation 5);
  - integrate f_ms, the fraction of light re-scattered back towards the point if what
    reached each point were 1 (equation 7);
  - all orders are a geometric series: Ψ_ms = L₂ / (1 − f_ms) (equation 10).
  One 64-thread workgroup per texel; no iterations over orders.
- **Sky-view table** (192 × 108): the sky seen from the camera's altitude, by zenith
  angle (rows squared towards the horizon, half above, half below) and azimuth from the
  sun (the sky is symmetric about the sun's vertical plane, so half the azimuths suffice,
  squared towards the sun). 30 samples per ray, more for long rays.
  - Single scattering ray-marched with Rayleigh and Mie (Cornette-Shanks) phase functions,
    plus the multiple scattering table's light × the local scattering coefficient.
  - Each step integrated analytically against its transmittance (Hillaire 2015, slide
    28): (S − S·T) / σt, robust to large steps.
- **Aerial perspective** (32 × 32 × 32 froxels over 32 × 4 km, slices squared towards
  the camera): in-scattered light and grey transmittance per froxel, from the camera to the
  froxel; every frame; applied to opaque and transparent surfaces with one 3D texture
  sample.
- **Costs** in the paper (PS4, 1080p): transmittance 0.01 ms, multiple scattering
  0.07 ms, sky view 0.05 ms, aerial perspective 0.04 ms, sky + aerial perspective applied
  0.12 ms — "scalable": everything except the application is resolution independent.
- Accuracy: within a few percent of a path tracer for Earth-like atmospheres; the
  isotropic multiple-scattering assumption overestimates slightly at high Mie
  anisotropy.
- What we take: **everything above** (the tables, their sizes and parameterizations, the
  integration, the approximation), with three changes:
  - each march step's sample at its midpoint: the reference samples 0.3 into each step
    and integrates from 0 to there, so its even-step marches end 0.7 of a step short of
    the ray's end (transmittance biased high); with midpoints the transmittance table is
    within 0.003 of 512 steps in double precision, the sky-view table within 2.3%;
  - the aerial perspective with **one thread per froxel and a parallel prefix scan** per
    column (each thread integrates only its slice's segment; the column's (light,
    transmittance) pairs are combined front to back): the reference marches each froxel's
    whole ray from the camera (∑ 1..32 · 2 = 1,056 steps per column); one thread per
    column (32 × 2 steps, sequential) used too few threads for the GPU (26.8 µs on the
    RTX); the scan does 64 steps per column in parallel (12 µs RTX, 63 µs Radeon);
  - ground albedo 0.3 by default (the reference uses 0 for comparisons with Bruneton):
    the ground below the horizon and the light it bounces into the sky.

### Others

- **Preetham et al. 1999 / Hosek-Wilkie 2012**: analytic fits of clear skies from the
  ground. Cheap, but fixed to Earth from the ground, no aerial perspective model, sunsets
  poorly (Preetham), and the sun's color is a separate approximation. Godot's
  `PhysicalSkyMaterial` uses a Preetham-like model (MIT); not taken.
- **Bevy** (MIT/Apache-2.0, `bevy_pbr::atmosphere`, 0.16+): Hillaire 2020 with the same
  four tables (transmittance, multiple scattering, sky view, aerial view), rebuilt every
  frame; a cube from the sky-view table for environment light. Confirms the design.
- **Unreal Engine** (Sky Atmosphere component): Hillaire's technique in production; the
  "aerial perspective view distance scale" parameter (exaggerate distances in small
  scenes) is ours too.

## Fog

### Exponential height fog (Unreal Engine; Wenzel, "Real-time Atmospheric Effects in Games
Revisited", GDC 2007; Quílez, "Better Fog")

- Density ρ(y) = d · exp(−k (y − y₀)): thick near the ground, thinning upwards.
- Along a ray from the camera at height y_c in direction v over distance s, the optical
  depth has a closed form: d · e^(−k (y_c − y₀)) · s · (1 − e^(−k v_y s)) / (k v_y s), with
  the limit d · e^(−k (y_c − y₀)) · s for horizontal rays (series 1 − x/2 near 0).
- In-scattered light with constant lighting along the ray: albedo × (1 − T) × L_in.
  Unreal adds a "directional inscattering" lobe with an exponent; we use a
  Henyey-Greenstein phase function of the sun's light (physical, one parameter).
- Cost: a few exponentials per pixel; no volume, no shadows (light shafts need
  volumetric fog).
- What we take: all of it, with L_in = the sky's average radiance (band 0 of the
  environment's irradiance harmonics, ADR 0046: Y₀₀ · c₀ / π) + the sun at the camera ×
  HG(g, v·l). Start distance and maximum opacity as in Unreal.

### Volumetric (froxel) fog

- **Wronski, "Volumetric Fog" (Assassin's Creed IV, SIGGRAPH 2014)**: a camera-aligned
  froxel grid (160 × 90 × 64 in AC4), density and lighting injected per froxel with
  shadow-map lookups, then integrated front to back along each column; applied to every
  surface with one 3D texture sample. Temporal reprojection and jitter hide the coarse
  depth slices.
- **Hillaire, "Physically Based and Unified Volumetric Rendering in Frostbite" (SIGGRAPH
  2015)**: the same grid, physically based (extinction, scattering, albedo, phase), the
  analytic per-step integration used above, participating media volumes, temporal
  integration, and the fog's light also lighting particles.
- **Godot 4** (MIT): froxel volumetric fog with fog volumes and temporal reprojection;
  separate exponential height fog in its `Environment`.
- Cost, extrapolated from our measurements: the aerial perspective's froxels cost about
  1 ns per froxel step on the Radeon (32,768 froxels × 2 steps in 63 µs). A 160 × 90 × 64
  grid is 921,600 froxels: at least 0.9 ms before its shadow lookups (cascade selection and
  PCF per froxel), the scan, and the temporal filter it needs, on an iGPU frame that the
  composite pass already grows by 2 ms. Not done here (ADR 0069, deferred).

## Patents (checked 2026-10-01)

- No patent found on Hillaire's technique (Google Patents: "sky atmosphere" multiple
  scattering look-up table, Epic Games / Hillaire), nor on Bruneton's (an academic
  publication with BSD code), nor on exponential height fog (a closed-form integral in
  the literature since the 1990s).
- **US 9,582,929 B2** (Confetti Interactive / Wolfgang Engel, "Dynamic skydome system",
  priority 2013-06-04, active): claim 1 covers a skydome system that "performs dynamic
  actual lighting for all accumulated objects in one rendering pass, and/or calculates
  the color of the sky or atmosphere in the vertex shader". We compute the sky in compute
  passes and a fragment shader, and light objects in our existing passes; we do not
  compute sky color in a vertex shader. Listed for the team (ADR 0069, questions).
- **US 6,677,947** (frustum-cache acceleration of line integrals for volume rendering,
  filed 2000, expired): about the froxel idea; expired.
- Froxel fog itself (Wronski 2014, Hillaire 2015) is published and shipped widely; no
  patent found. Re-check before implementing volumetric fog.

## Sources

- S. Hillaire, "A Scalable and Production Ready Sky and Atmosphere Rendering
  Technique", Computer Graphics Forum 39(4), EGSR 2020. Code:
  https://github.com/sebh/UnrealEngineSkyAtmosphere, commit 183ead5bdacc (MIT, Copyright
  (c) 2020 Epic Games, Inc.).
- E. Bruneton, F. Neyret, "Precomputed Atmospheric Scattering", EGSR 2008. E. Bruneton,
  "A Qualitative and Quantitative Evaluation of 8 Clear Sky Models", TVCG 2017. Code:
  https://github.com/ebruneton/precomputed_atmospheric_scattering, commit 34f14e745cff
  (BSD-3-Clause, Copyright (c) 2017 Eric Bruneton).
- S. Hillaire, "Physically Based and Unified Volumetric Rendering in Frostbite",
  SIGGRAPH 2015, Advances in Real-Time Rendering.
- B. Wronski, "Volumetric Fog: Unified Compute Shader Based Solution to Atmospheric
  Scattering", SIGGRAPH 2014, Advances in Real-Time Rendering.
- Unreal Engine documentation: Sky Atmosphere component; Exponential Height Fog
  (dev.epicgames.com, read 2026-10-01).
- C. Wenzel, "Real-time Atmospheric Effects in Games Revisited", GDC 2007; I. Quílez,
  "Better Fog" (iquilezles.org).
- W. Cornette, J. Shanks, "Physically reasonable analytic expression for the
  single-scattering phase function", Applied Optics 31(16), 1992.
- Bevy `bevy_pbr::atmosphere` documentation (docs.rs, `AtmosphereSettings`, read
  2026-10-01; MIT/Apache-2.0).
- Godot 4 documentation: Environment and post-processing, Volumetric fog (MIT).
- Google Patents: US 9,582,929 B2, US 6,677,947 (read 2026-10-01).
