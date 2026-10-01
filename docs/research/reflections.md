# Local reflections: reflection probes and screen-space reflections

> Status: see "Work status" at the end.

Research for [ADR 0073](../adr/0073-reflection-probes-and-screen-space-reflections.md).
Checked 2026-10-01.

## Problem

Specular light came only from the sky's prefiltered cube (ADR 0046; with the physical
atmosphere, ADR 0069, the cube is drawn from the sky). Inside buildings (Sponza's arcades)
every glossy surface reflected the bright sky: wrong in color and several times too bright
for occluded places; nothing reflected nearby objects. The owner asked for screen-space
reflections and "very fast reflections". Constraints from the agent rules: views that do
not use a feature pay nothing for it, not even shader registers (opaque shading sits at
64 VGPRs on the Radeon iGPU in the environment variant; 72 cost 9–13 %); no patented
algorithms; permissive licenses only.

## Sources

| Source | What | License / patent |
|---|---|---|
| S. Lagarde, A. Zanuttini, "Local Image-based Lighting With Parallax-corrected Cubemap", SIGGRAPH 2012 talk; Lagarde's blog post "Image-based Lighting approaches and parallax-corrected cubemap" (2012-09-29) | box proxies, ray–box intersection, influence volumes and blending, artist placement | paper; no patent found (Google Patents and USPTO full-text search, 2026-10-01) |
| B. Karis, "Real Shading in Unreal Engine 4", SIGGRAPH 2013 | split sum: prefiltered mips per roughness × DFG | already used (ADR 0046) |
| J. Křivánek, M. Colbert, "Real-time Shading with Filtered Importance Sampling", EGSR 2008 | prefilter with mip-filtered GGX samples | already used by `environment.slang` |
| M. McGuire, M. Mara, "Efficient GPU Screen-Space Ray Tracing", JCGT 3(4), 2014 | perspective-correct DDA against a depth buffer, thickness | paper |
| Y. Uludag, "Hi-Z Screen-Space Cone-Traced Reflections", GPU Pro 5 (2014) | hierarchical min-Z traversal, cone tracing for gloss | book chapter |
| T. Stachowiak, "Stochastic Screen-Space Reflections", SIGGRAPH 2015 Advances (Frostbite) | importance-sampled rays, half-resolution tracing, ray reuse across neighbours, temporal filter | slides; no patent found for the technique (2026-10-01; team to confirm) |
| AMD FidelityFX SSSR, github.com/GPUOpen-Effects/FidelityFX-SSSR (`ffx-sssr/ffx_sssr.h`, master, read 2026-10-01) and FidelityFX Denoiser | hierarchical traversal (`FFX_SSSR_InitialAdvanceRay`, `FFX_SSSR_AdvanceRay`, `FFX_SSSR_HierarchicalRaymarch`), low-occupancy exit, hit validation, spatial/temporal denoise with variance | MIT (© 2021 Advanced Micro Devices); the traversal is adapted in `reflections.slang` (THIRD_PARTY_NOTICES.md) |
| E. Heitz, "Sampling the GGX Distribution of Visible Normals", JCGT 7(4), 2018 | VNDF sampling of reflected rays | paper |
| S. Lagarde, C. de Rousiers, "Moving Frostbite to PBR", SIGGRAPH 2014 course | specular occlusion from ambient occlusion | already used (ADR 0046) |
| J. Jimenez et al., "Practical Realtime Strategies for Accurate Indirect Occlusion", 2016 | GTSO: specular occlusion as a cone–cone intersection with a bent normal | needs a bent normal our GTAO does not output yet |
| Call of Duty (IW), Frostbite and Unreal "reflection capture" documentation | probes relit at run time from a G-buffer captured once vs re-rendered | talks / docs |
| Godot 4 (MIT): `ReflectionProbe`, `scene_forward_clustered.glsl`, the SSR effect; Bevy (MIT/Apache-2.0): `EnvironmentMapLight`, `ssr.wgsl` | shipping open-source versions: probes in a clustered list blended by box distance; Godot's SSR merges into a separated specular buffer; Bevy's SSR marches linearly | read for ideas, no code taken |

**Patents found (not used):** US 10,621,774 B2, US 10,818,079, US 10,796,474 (Electronic
Arts, 2018–2020): SSR whose failed rays are re-cast as world-space ray-traced rays, the
hit colors aggregated; US 10,360,720 B1 and US 10,586,375 (Microsoft, 2018): ray tracing
for rays towards the camera or where SSR fails. Both need world-space ray tracing as the
fallback; ours falls back to probes and the sky. They matter if hardware ray-traced
reflections are added.

## How each part is done elsewhere, and what we take

### Probe storage and capture

- **Cube or octahedral:** cubes sample with hardware seamless filtering and mips;
  octahedral maps pack into 2D atlases (simpler streaming; seams need care). Cube arrays
  are core Vulkan (`imageCubeArray`, every desktop GPU). **Taken:** one RGBA16F cube array,
  6 layers per probe, the same 6 mips per roughness as the sky's cube (one sampling code).
- **Re-render or relight:** CoD and Frostbite capture a G-buffer once and relight it every
  frame (cheap time of day, no re-render) at the cost of a G-buffer per probe (5–6 × the
  memory) and approximate shadows. **Taken:** re-rendering with the forward renderer
  itself: the same materials, lights and shadows, no new shading path; captures cached
  until something changes, spread over frames by a face budget. Relighting is the
  follow-up if continuous time of day forces constant captures.
- **Capture cost:** six views each with the full four shadow cascades cost 1.1 ms (RTX)
  and 8.3 ms (Radeon) per probe in Sponza; one cascade fit to the sphere the capture sees,
  shared by the probe's faces, cut that to 0.18 / 2.4 ms.
- **Prefilter:** the sky's filtered importance sampling (64 GGX samples per texel, the
  source mip from each sample's solid angle), from a capture cube with a full mip chain.

### Selection and blending

- Unreal: probes through the clustered light grid, blended by distance to the box faces,
  sorted by size. Godot: clustered probe elements with a blend distance and a priority.
  Lagarde: smallest-first accumulation with a blend band.
- **Taken:** probes listed per coarse screen cell (16 × 16 per view) and per view-depth
  z-bin (64), bit masks in the view's constants; a pixel tests the probes in both,
  smallest first, and takes `share = influence × (1 − covered)` from the first two it is
  in. A per-tile compute pass (as local lights have) was a pass of its own for nothing at
  these counts.

### Applying them: in the forward shading or in a composite (measured)

Two places can swap the sky's specular light for the probes'. Sponza 1080p, 6 probes,
Radeon iGPU:

| | Radeon pixel shader VGPRs | forward pass | separate pass |
|---|---:|---:|---:|
| sky only (environment variant, prepass on) | 64 | 8.7 ms | – |
| probes in the forward shading (entry points of their own) | 87 | 16.8 ms | – |
| **probes in a full-screen composite (chosen)** | 72 (forward) / 55 (composite) | 8.9 ms | 4.7 ms |

The composite's floor is the pass itself (2.0 ms on the iGPU: reading depth, normal and
weight and blending into RGBA16F at 1080p); the probe search adds about 1.2 ms and each
cube read about 0.8 ms. In Slang, reading a probe through a `Probe*` loaded the whole
144-byte struct (77 → 43 VGPRs reading it float4 by float4).

### Screen-space reflections

- **Tracing:** linear marches (Bevy, older engines) take 16–64 fixed steps and miss thin
  objects; McGuire–Mara's DDA is exact per pixel but linear in distance; Hi-Z traversal
  (Uludag; FidelityFX SSSR) skips empty space in logarithmic steps. **Taken:** SSSR's
  traversal against a nearest-depth pyramid of our own (the occlusion pyramid keeps the
  farthest depth, wrong for tracing), with its low-occupancy exit for glossy rays.
- **Gloss:** cone tracing (Uludag) blurs by mip; stochastic rays (Stachowiak, SSSR)
  sample the GGX lobe and denoise. **Taken:** VNDF sampling (Heitz 2018) above roughness
  0.1, mirror rays below, a roughness cutoff (0.6 by default) where probes take over.
- **Input color:** Unreal and HDRP read last frame's color (reflections and transparency
  included, one frame of lag and ghosting under motion); Godot reads the current frame.
  **Taken:** the current frame's lit opaque image, before the atmosphere and the
  transparent surfaces: no reprojection, no ghosting, and the composite lands before the
  fog that should cover it. The frame renderer's previous color (ADR 0072) stays there
  for multiple bounces later.
- **Denoise:** SSSR's denoiser (spatial with variance, temporal reprojected by the hit's
  virtual position with a variance clip) and Stachowiak's ray reuse. **Taken:** a spatial
  filter at the trace's resolution from group shared memory (depth, normal and roughness
  weights) and a temporal filter that upsamples, reprojects through the reflected point
  and clips history to mean ± 1.25 σ of the trace neighbourhood. Ray reuse with BRDF/pdf
  ratios is the next quality step.
- **Half resolution:** SSSR and Frostbite trace at half resolution: a quarter of the rays.
  Default here, one of each 2 × 2 pixels traced, turning every frame.

### Specular occlusion

GTAO's visibility already multiplies the material's occlusion in the views that have it,
and Lagarde's approximation turns it into specular occlusion (ADR 0046, ADR 0058); the
weight the composite uses includes it, so probes and SSR are occluded the same way.
GTSO needs a bent normal (a follow-up once GTAO outputs one).

## Not done (with reasons)

- Relit G-buffer probes (memory, approximate shadows): if time of day demands.
- Probes and SSR on transparent surfaces (they keep the sky's light; the composite runs
  before them).
- Ray reuse across neighbours, cone tracing.
- Hardware ray-traced reflections (see the patents above).

No code was copied from Godot or Bevy; FidelityFX SSSR's traversal is adapted (MIT).

## Work status (2026-10-01, paused by the coordinator a second time)

Done (branch `feat/reflections`, origin/main with #101 merged in; clippy `-D warnings`
clean; every existing golden image byte-identical on both GPUs; Radeon opaque shading
still 48/64/64/80 VGPRs):

- Probes captured by the forward renderer (one shared shadow cascade per probe), mips and
  GGX prefilter into a cube array, cached with a face budget, recaptured on probe edits,
  `refresh`, sky/sun changes and GPU-scene changes inside their boxes; selection by screen
  cells × z-bins; parallax correction; blending smallest first.
- SSR: nearest-depth pyramid, FidelityFX SSSR traversal (with its low-occupancy exit),
  VNDF rays, spatial filter at trace resolution, temporal filter with upsampling and
  variance clip; half or full resolution.
- One composite (probes / SSR / both) before the atmosphere and transparent surfaces.
- Frame renderer: `ViewSettings::screen_space_reflections` (the `View` owns the history),
  `SceneInput::reflection_probes`. SSR is wired through `ForwardView::reflections` inside
  the scene passes, **not** the frame renderer's post-scene hook: the composite must land
  before the atmosphere/fog and transparent surfaces, and tracing the current frame's
  opaque image needs no previous color (argued in the ADR; the hook's previous color is
  the path to multiple bounces).
- Tests (both GPUs): parallax vs analytic box (99.9 % of floor pixels), energy at every
  probe mip, capture caching/budget (draw lists and GPU scene), SSR vs analytic mirror
  (full and half res, 100 %), zero allocations (capturing every frame, and cached).
  Golden shots `procedural_reflections`, `sponza_probes` (both GPUs); perf-gate scene
  `sponza_reflections` added (baselines not written yet).

Measured so far (Sponza 1080p, `scene_bench`; RTX numbers noisy on the shared machine):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| frame, sky + prepass, no reflections | 535–579 µs | 12.9 ms |
| frame, 6 cached probes | 695 µs | 18.9–20.5 ms |
| probe composite | 139 µs | 4.9 ms |
| capture of a whole probe (6 faces, shared shadow) + prefilter | 183 + 47 µs | 2.4 + 0.66 ms |
| one face per frame | ~70–86 µs | 0.95–1.07 ms |
| SSR half: depth / trace / spatial / temporal / composite | 28 / 92 / 53 / 55 / 35 µs | 0.37 / 3.1 / 1.7 / 1.7 / 0.9 ms (before the low-occupancy exit) |
| SSR full: same | 27 / 281 / 196 / 63 / 34 µs | not measured yet |

Next: re-measure SSR after the low-occupancy exit; perf-gate baselines for
`sponza_reflections` (both GPUs); ADR 0073, dev-guide section (rendering.md §16), perf
history row, THIRD_PARTY_NOTICES (FidelityFX SSSR traversal, MIT); `tools/check.ps1 -Gpu
-Perf`; PR. Open question for the team: SSR via the hook vs in the scene passes (above).
