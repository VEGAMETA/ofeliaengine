# HDRI environments

Research for queue item 2b (owner, 2026-10-10: before GI, a necessity), 2026-10-11:
lighting a scene with an HDR panorama and showing it behind the scene. Decision:
[ADR 0083](../adr/0083-hdri-environments.md). The image-based lighting it feeds is ADR 0046
([image-based-lighting.md](image-based-lighting.md)).

## Sources (checked 2026-10-11)

| Source | License | How | What we take |
|---|---|---|---|
| three.js `PMREMGenerator` (dev branch) | MIT | Equirectangular to cube with four subpixel samples per texel ("to preserve small, bright features"); then a prefiltered map: GGX VNDF importance sampling (256 samples) for the sharp levels, integration over a 16² source for the rough ones | Supersampling the conversion |
| Babylon.js `HDRFiltering` | Apache-2.0 | GGX importance sampling into each mip, the source's mip chosen per sample from its pdf (filtered importance sampling); quality is the sample count; level 0 forced to roughness 0 | The scheme we already have (ADR 0046) |
| Filament `cmgen`, `libs/ibl` | Apache-2.0 | Equirectangular to cube with adaptive supersampling (sample count from the source's footprint, Hammersley points); prefiltering by GGX importance sampling with filtered importance sampling, lod = log₄(K·Ωs/Ωp), K = 4 (ours too), 1,024 samples by default; `--clamp` compresses luminance above 4,096 towards 16,384 (values "chosen arbitrarily... with 4096 samples"); spherical harmonics windowed (sinc⁴, the cutoff found by bisection so that irradiance never goes negative) | Their answers to a sun: clamping and windowing. We take the sun out instead |
| Godot 4 (`Sky`, `PanoramaSkyMaterial`) | MIT | The sky shader samples the panorama directly; the radiance cube is made by importance sampling (quality, incremental and real-time modes, the last after Manson and Sloan 2016). Its documentation warns that panoramas with the sun visible sparkle in reflections, and suggests more GGX samples | The problem |
| Bevy 0.17 (`GeneratedEnvironmentMapLight`, PR #19076) | MIT / Apache-2.0 | Single-pass downsampling, bounded VNDF GGX sampling with blue noise, a forward and reverse tone map that suppresses fireflies. Issue #16278 (no sun shadows under HDRI lighting) proposes detecting the brightest pixel to make a directional light, or shadow-only lights; still open | Our extraction is that proposal |
| Engine practice (Unreal and Unity HDRP guides) | — | Duplicate the panorama, paint the sun out of the copy that lights the scene, keep it in the background, add a directional light along the sun | What we automate |
| Ward, "Real Pixels" (Graphics Gems II, 1991); Radiance's file format | — | RGBE: one shared exponent per pixel, run-length encoded scanlines | Our decoder |
| Křivánek and Colbert, filtered importance sampling (EGSR 2008) | paper | Each sample reads the source mip of its solid angle | Already ours |

No code was copied from any of them: three.js, Babylon.js, Filament, Godot and Bevy were
read for their approach.

## The test panorama, measured

Poly Haven's Kloofendal 48d Partly Cloudy (Pure Sky) at 2K (2048 × 1024; CC0; Greg Zaal,
sky edits by Jarod Guest), measured with a Python decoder of our own:

- brightest pixel: luminance 72,815; six channel values above half floats' 65,504;
- the sun's light above the sky around it: 4.29 within 0.5° of its center, 4.38 within
  1°, 4.70 within 8° (glare and aureole);
- on an upward surface the sun is 68 % of the light (3.25 of 4.80);
- mean luminance by distance from the sun: 51,136 within 0.25°, 4,467 at 0.25–0.5°, 212
  at 0.5–0.75°, 59 at 0.75–1°, 31 at 1–1.5°, 16 at 2–3°, 5.4 at 4–5°, 2.5 at 6–8°,
  1.0 at 10–12°, 0.7 beyond 15° (the sky).

The sun is a spike 10⁵ times the sky holding most of the light. Kept in a 128² lighting
cube, it turns into fireflies in the prefiltered levels (64 samples per texel), rings in
the nine spherical harmonics (negative light opposite it), and casts no shadow.

## Conclusions

1. **Our own `.hdr` decoder** (the format is a header and run-length encoded scanlines),
   pixels kept as RGBE, 4 bytes each, and decoded on the GPU: nothing of their range is
   lost. OpenEXR later (the `exr` crate, BSD-3-Clause, or our own), converted to RGBE.
2. **The sun out of the map, into a directional light.** The brightest pixel is a sun if it
   outshines the median of the sky 10° to 20° around it 256-fold. Its cone ends a pixel
   past the first one-pixel ring around it whose mean falls below 1/1024 of the peak; its
   illuminance is the light within the cone above the sky just outside it (the mean of a
   ring out to twice the cone). The lighting cube takes that sky inside the cone; the
   background keeps the sun. Shadows come from the light, and the lighting cube stays
   smooth: no clamping, no more samples, no windowing of the harmonics.
3. **A background cube of its own,** made from the panorama on the GPU (2 × 2 samples per
   texel; faces a quarter of the panorama's width, at most 1,024 by default), so every
   reader of the sky stays a cube read: the sky pass, the atmosphere's composite with fog,
   reflection probes. The 128² lighting cube is filled from it (4 × 4 samples per texel
   at the matching mip).
4. **Turning** the panorama remakes both from the texels, which stay on the GPU.

Not taken: luminance compression (Filament's `--clamp`) loses the sun's energy and casts
no shadow; with the sun out, windowing the harmonics is not needed; sampling the panorama
directly in the sky pass (Godot) would need a panorama variant of every reader of the sky.

**Phase 2** (ADR 0083's amendments): panoramas cooked into BC6H cubes with mips (the DDC
and packs), the background 8 times smaller; the cube turned when sampled; OpenEXR through
the `exr` crate (Poly Haven's EXRs are PIZ compressed, float). Still to do: the sun's
angular size for soft shadows.

## Patents

A quick search (2026-10-11) found no patent on taking a light out of a given HDR map by a
threshold; those found concern estimating lighting from LDR photographs (US10609286B2,
Adobe, learned), augmented reality (US10665011B1) and lighting from a reference object
(US11330196B2). Taking lights out of HDR maps is old public work: Debevec's median cut
(SIGGRAPH 2005), Havran et al., "Interactive System for Dynamic Scene Lighting using
Captured Video Environment Maps" (EGSR 2005).

## Sources

- three.js `src/extras/PMREMGenerator.js` (MIT), dev branch, read 2026-10-11
- Babylon.js `packages/dev/core/src/Materials/Textures/Filtering/hdrFiltering.ts` (Apache-2.0), master, read 2026-10-11
- Filament `tools/cmgen/src/cmgen.cpp`, `libs/ibl/src/CubemapIBL.cpp`, `CubemapSH.cpp`, `CubemapUtils.cpp` (Apache-2.0), main, read 2026-10-11
- Godot `Sky` class reference and environment documentation (https://docs.godotengine.org), godot-proposals #498, read 2026-10-11
- Bevy PR #19076 (environment map filtering), issue #16278 (sun shadows with HDRI lighting), read 2026-10-11
- Poly Haven, Kloofendal 48d Partly Cloudy (Pure Sky) (https://polyhaven.com/a/kloofendal_48d_partly_cloudy_puresky), CC0
- Ward, "Real Pixels", Graphics Gems II, 1991
- Křivánek, Colbert, "Real-time Shading with Filtered Importance Sampling", EGSR 2008
- Debevec, "A Median Cut Algorithm for Light Probe Sampling", SIGGRAPH 2005 poster
- Havran et al., "Interactive System for Dynamic Scene Lighting using Captured Video Environment Maps", EGSR 2005
