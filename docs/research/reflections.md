# Local reflections: reflection probes and screen-space reflections

> **Status (2026-10-01, WIP, paused by the coordinator):** see "Work status" at the end.
> The research sections below are a first draft; sources still to be completed with
> versions and check dates before the PR.

## Problem

Specular light comes only from the sky's prefiltered cube (ADR 0046, ADR 0069). Interiors
(Sponza's arcades) reflect the bright sky: wrong and too bright. Wanted: fast local
reflections, probes first, then screen-space reflections (SSR), with a chain
SSR → probe → sky, and no cost (not even shader registers) for views without them.

## Sources (draft)

- Lagarde, Zanuttini, "Local Image-based Lighting With Parallax-corrected Cubemaps",
  SIGGRAPH 2012 talk, and Lagarde's blog "Image-based Lighting approaches and
  parallax-corrected cubemap" (2012): box proxies, ray–box intersection, influence volumes,
  blending. No patent found (searched 2026-10-01).
- Karis, "Real Shading in Unreal Engine 4" (2013): split sum, prefiltered mips per
  roughness; Krivanek and Colbert 2008: filtered importance sampling (already used by
  `environment.slang`).
- McGuire, Mara, "Efficient GPU Screen-Space Ray Tracing", JCGT 2014; Uludag, "Hi-Z
  Screen-Space Cone-Traced Reflections", GPU Pro 5 (2014): min-Z pyramid traversal.
- Stachowiak, "Stochastic Screen-Space Reflections", Frostbite, SIGGRAPH 2015 Advances:
  importance-sampled rays, half-resolution tracing, ray reuse, temporal filter. No patent
  found for the technique itself (searched 2026-10-01; team to confirm).
- AMD FidelityFX SSSR (MIT, github.com/GPUOpen-Effects/FidelityFX-SSSR) and FidelityFX
  Denoiser (MIT): hierarchical traversal (adapted in `reflections.slang`, attribution to
  add to THIRD_PARTY_NOTICES.md), hit validation, spatial/temporal denoise.
- Heitz, "Sampling the GGX Distribution of Visible Normals", JCGT 2018.
- Lagarde, de Rousiers, "Moving Frostbite to PBR" (2014): specular occlusion from AO
  (already in `forward.slang`); Jimenez et al. 2016 (GTSO, needs bent normals: not
  available from our GTAO yet).
- **Patents found (not used):** US 10,621,774 / 10,818,079 / 10,796,474 (Electronic Arts):
  SSR whose failed rays are re-cast as ray-traced rays; US 10,360,720 / 10,586,375
  (Microsoft): hybrid SSR + ray tracing. Both need world-space ray tracing as the
  fallback; our fallback is probes and the sky. Relevant if hardware ray-traced
  reflections are added later.

## Design so far (implemented, see "Work status")

- **Apply as a composite, not in the forward shading.** Views with reflections shade
  their opaque surfaces with new entry points (`fragmentReflect*`) that also write the
  normal and roughness (A2B10G10R10) and the weight of the prefiltered radiance (split
  sum × specular occlusion, RGBA8). One full-screen pass then adds
  `weight × (reflection − sky)`: the sky's specular light, already in the image, is
  swapped for SSR hit → probes → sky. Other views run exactly their old shading (SPIR-V
  of the existing entry points: only constant ids reordered in three motion variants;
  golden images bit-identical on the RTX).
- **Probes:** captured by the forward renderer itself (six 90° views per probe, its
  passes named `probe …`), copied into a capture cube (sky from the environment's cube
  where nothing was drawn, no sun disk: the sun's specular is the directional light's),
  mips, GGX prefilter into one RGBA16F cube array (6 mips, as the sky). Budget of faces per
  frame; captured again on probe edits, `refresh`, sky/sun changes, and on the GPU-scene
  path when instances inside the probe's boxes change (`SceneFrame::changed_boxes`).
  Selection: CPU frustum culling, smallest first (≤ 64 per view), per-16×16-tile bitmasks
  from the probes' screen rectangles; the composite walks its tile's bits wave-uniformly,
  blending front to back by influence weight; parallax correction against the probe's
  box; the rest from the sky.
- **SSR:** nearest-depth pyramid (buffer, all levels), trace at full or half resolution
  (FidelityFX SSSR's hierarchical traversal, VNDF-sampled rays above roughness 0.1,
  roughness cutoff 0.6), spatial filter (depth/normal/roughness weights), temporal filter
  with its own history (reprojected through the reflected point, neighbourhood clamp).
  Traces against the current frame's lit opaque image (before transparent surfaces and
  the atmosphere), so it needs no previous-frame color.

## Work status

Done (builds, `cargo check --workspace --all-targets` clean; existing golden images
identical on the RTX 4060 Ti; the two new shots render without validation errors but were
not looked at yet):

- `forward.slang`: `fragmentReflect`, `…Ao`, `…Motion`, `…MotionAo`.
- `reflections.slang` / `reflections.rs`: probes (capture copy, mips, prefilter, tiles),
  composite, SSR (depth pyramid, trace, spatial, temporal); `ReflectionProbes`,
  `ReflectionProbe`, `ProbeBox`, `ScreenSpaceReflections`, `SsrSettings`, `Reflections`.
- `ForwardRenderer::set_reflections`, `ForwardView::reflections` (37 call sites take
  `Default::default()`), captures inside `add_passes` / `add_scene_passes`, environment
  imported once per call (shared by the captures and the view).
- `Image` gained `ImageKind::CubeArray`; the device enables `imageCubeArray`;
  `EnvironmentFrame::rebuilt`; `GpuScene` records changed boxes.
- Golden harness: `Shot::{probe_grid, ssr, glossy_floor}`; shots `procedural_reflections`
  and `sponza_probes` (no images yet).

Next:

1. Look at the renders, debug (orientation of captured faces, parallax, SSR hits).
2. Tests: parallax vs analytic box intersection (mirror floor in an emissive box room),
   prefilter energy, capture invalidation (static scene: zero captures after the first),
   SSR hits on an analytic mirror plane, zero allocations per frame.
3. Registers (`scene_bench --pipeline-stats` on the Radeon: existing variants must stay
   48/64/64/80), measurements on both GPUs (cached probes, capture + prefilter per probe,
   SSR full/half), `scene_bench --probes/--ssr`, perf-gate scene.
4. Compare the in-forward probe lookup against the composite (task: measure which is
   cheaper); probe shadow cost (one shadow map per probe instead of per face).
5. Switch SSR's inputs to the frame renderer's hooks (ADR 0072) once merged, if it gives
   anything beyond what the current-frame trace does.
6. ADR 0073, dev-guide section, perf history, THIRD_PARTY_NOTICES (FidelityFX SSSR).
