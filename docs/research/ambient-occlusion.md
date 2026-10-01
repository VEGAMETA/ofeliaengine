# Ambient occlusion

Research for screen-space ambient occlusion (the team's priority 4, 2026-09-30).
Decision: [ADR 0058](../adr/0058-ambient-occlusion.md).

Checked on 2026-10-01. XeGTAO's source (MIT) and documentation were read and its formulas
adapted; other engines are described from their public documentation and papers.

## Sources

| Source | Version | License | What we take |
|---|---|---|---|
| J. Jimenez, X. Wu, A. Pesce, A. Jarabo, "Practical Realtime Strategies for Accurate Indirect Occlusion", SIGGRAPH 2016 course, Activision tech report ATVI-TR-16-01 | 2016 | paper | GTAO: slices, horizons, the exact cosine-weighted arc integral, the multi-bounce fit, the fast `acos` |
| Intel, XeGTAO (`GameTechDev/XeGTAO`, `XeGTAO.h`, `XeGTAO.hlsli`) | master, 2021 | MIT (Copyright (C) 2016-2021, Intel Corporation) | the structure (depth MIP filter, main pass, edge-aware denoise), the constants (radius × 1.457, falloff 0.615, MIP offset 3.3, power 2.2, the 0.03 floor, ÷ 1.5 before the blur), R2 + Hilbert noise, the edge packing; its perf figures |
| S. Lagarde, C. de Rousiers, "Moving Frostbite to Physically Based Rendering", SIGGRAPH 2014 | 3.0 | course notes | specular occlusion from ambient occlusion (already in the forward pass, ADR 0046) |
| M. Mittring, "Finding Next Gen: CryEngine 2", SIGGRAPH 2007 | — | course notes | the original SSAO, for the comparison |
| L. Bavoil, M. Sainz, R. Dimitrov, "Image-Space Horizon-Based Ambient Occlusion", SIGGRAPH 2008 talk | — | talk | HBAO, horizons along directions (GTAO's ancestor) |
| M. McGuire et al., "Scalable Ambient Obscurance", HPG 2012 | — | paper | the depth MIP chain for far samples |
| F. Kolyvanov, Intel ASSAO (2016); AMD FidelityFX CACAO | public | MIT (read only) | the tuned sample-based alternative |
| O. Therrien, Y. Levesque, G. Gilet, "Screen Space Indirect Lighting with Visibility Bitmask", The Visual Computer 2023 | — | paper | thickness-aware GTAO, the next step |
| Unreal Engine GTAO, Unity HDRP AO | public docs | products | half resolution and temporal accumulation as the usual cost savers |

## What AO is here

The share of a surface's cosine-weighted hemisphere that the surroundings do not hide,
within a radius. It scales indirect light only: the ambient term and the image-based
diffuse and specular. Direct light has its own occlusion (shadows).

## Screen-space methods

- **SSAO (Crytek 2007):** random points in a sphere, compared with the depth buffer.
  Cheap, but the result is "how much of a sphere is inside geometry", not a visibility:
  flat walls come out grey, and no amount of samples reaches a reference.
- **HBAO (NVIDIA 2008):** along a few screen directions, the highest horizon angle.
  Closer to a visibility, but integrated without the cosine weight.
- **SAO (McGuire 2012):** a depth MIP chain so far samples stay in the cache; obscurance,
  not a visibility.
- **ASSAO / CACAO:** tuned SSAO variants with adaptive sample counts and good denoising.
  Fast, good-looking, not converging to a reference.
- **GTAO (Jimenez 2016):** HBAO's horizons with the exact integral of the cosine-weighted
  visibility between them, per slice, for the normal projected onto the slice. With enough
  slices and samples it equals the ray-traced visibility of the depth buffer's height field.
  Ships in Call of Duty, Unreal (as "GTAO") and XeGTAO.
- **Visibility bitmasks (2023):** GTAO where each sample has a thickness, and the arc is
  a bitmask of sectors instead of two horizons: thin poles no longer occlude everything
  behind them. Same cost order; a follow-up.

## XeGTAO's structure

1. **Depth prefilter:** view-space depth plus four MIP levels in one pass. Each coarser
   texel averages its 2 × 2, weighting nearer depths down with the falloff (a thin
   foreground edge must not pull the background forward, or halos appear).
2. **Main pass:** per pixel, the view-space position, the normal from depth (or given),
   R2 noise indexed by a Hilbert curve (neighbouring pixels get well-spread values), then
   the slices. Per slice: the direction, the normal projected onto the slice plane, the
   angle `n` between it and the view vector; per sample on each side, the depth at
   `s² × radius` (denser near the pixel), the horizon cosine, blended towards the tangent
   plane by the falloff weight; the arc integral
   `(cos n + 2h sin n − cos(2h − n)) / 4` per side.
3. **Denoise:** an edge-aware blur (edges from depth, packed two bits each), 1–3 passes,
   meant to be followed by TAA.

The constants were tuned by Intel against a ray-traced reference at the requested radius:
the radius multiplier 1.457 compensates for the samples missing the outermost horizons.
Our test confirms both halves: 9 × 32 samples match the reference at 1.457 × radius, the
3 × 3 preset matches the one at the radius itself.

XeGTAO's figures: High, full resolution, 1920 × 1080 on an RTX 2060: 0.56 ms; Medium about
two thirds of it, Low two thirds of Medium; fp16 math 5–20 % faster; bent normals +25 %.

## Measured

Sponza, 1920 × 1080, `scene_bench --ao`, RTX 4060 Ti, release:

- High, all passes: 460 µs (main 386, depths 28, denoise 42). Low 231, Medium 312,
  Ultra 1,055. Radeon iGPU: Low 5.9 ms, High 12.1 ms.
- Where the main pass's time goes (High, before the optimizations: 462 µs): with no depth
  reads in the slice loop 258–280 µs; before the loop (normals, edges, noise) 75 µs;
  a pass that only writes 28 µs. So it is bound by arithmetic, mostly the
  transcendentals (`sin`, `cos`, `sqrt`, `rsqrt`, division) that run at a quarter rate.
- What helped: removing per-slice `sin`/`cos` (rotation instead), `rsqrt` for the sample
  distance and direction, the MIP level from `firstbithigh`, no loop for the atlas
  offsets: 462 → 386 µs (−17 %). Gathers in the denoise pass: 46 → 41 µs.
- What did not: R16F depths (−2 %), compile-time loop counts (−1 %), gathers in the main
  pass, 16 × 16 groups, nearest-level rounding, packed edges (no time change; kept for half
  the memory traffic, which matters on shared-memory GPUs).
- The Hilbert index costs about 16 µs; XeGTAO offers a lookup texture for it.
- The ÷ 1.5 before the blur matters for the 1-slice preset: without it the row mean of an
  open floor was 0.948 instead of 0.994.

## Open questions

- A half-resolution mode (Unreal's default on low settings) for weak GPUs, with a
  depth-aware upsample.
- Bent normals for directional ambient.
- Visibility bitmasks for thin geometry.
- fp16 math on AMD, to measure on RDNA hardware stronger than the 2 CU iGPU.
