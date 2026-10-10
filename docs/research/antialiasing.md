# Antialiasing and scaling modes: MSAA, FXAA, MLAA, SMAA, spatial upscalers

Checked 2026-10-02 (Catmull-Rom and Lanczos-3: 2026-10-10). Sources at the end. What a
game should be able to offer its players besides the temporal methods the engine already
had (TAAU, FSR 3.1, DLSS; ADR 0043, 0060, 0063, 0072), how each method works, how it
combines with the others and with the screen-space effects, which open implementations
exist and under which license, and what is known about patents. The decision and the
measurements are in ADR 0075.

Code: SMAA's reference (`SMAA.hlsl`, MIT) is vendored unmodified behind porting macros,
its area and search tables are taken from its headers; FSR 1 is a port of the FidelityFX
SDK's `ffx_fsr1.h` (MIT, already vendored). FXAA, MLAA's configuration, the MSAA resolves,
alpha to coverage's sharpening and the integer scaler were written here from the
publications. Godot (MIT) and Bevy (MIT/Apache) were read for their structure; Unreal and
Unity only through their public documentation.

## The families

| Family | Where it runs | What it fixes | What it costs | History |
|---|---|---|---|---|
| Supersampling (SSAA) | the whole frame at k² samples | everything | k² of the frame | the reference |
| **MSAA** | rasterization: k coverage/depth samples, one shading per pixel and triangle | geometry edges (and alpha via alpha to coverage) | k× depth/color bandwidth and memory, the resolve | none |
| **Post filters** (FXAA, MLAA, SMAA 1x) | after tone mapping, on the image | visible edges of any kind, by guessing their shape | one to three full-screen passes | none |
| **Temporal** (TAA/TAAU, FSR 2-3, DLSS, SMAA T2x) | jittered frames accumulated along motion vectors | everything, over frames; upscaling | a resolve pass and a history | ghosting, blur in motion |
| **Spatial upscalers** (nearest, bilinear, Catmull-Rom, Lanczos-3, integer, FSR 1) | render size → output | nothing (they scale) | one pass or none | none |

They combine: MSAA under a post filter (the samples resolve geometry, the filter what is
left), MSAA under TAA (fewer aliased edges for the history to fight), SMAA with MSAA 2×
and two-frame jitter (S2x, T2x, 4x), a spatial upscaler under a post filter.

## MSAA

**Hardware:** each pixel has k sample positions (Vulkan's standard locations for 2, 4, 8
match D3D's); a triangle covers some of them, depth is tested per sample, the fragment
shader runs once per pixel and triangle (unless the shader asks for the sample index:
sample-rate shading) and its color is written to the covered samples. At the end the
samples are resolved to one value per pixel. Cost: color and depth bandwidth × k (largely
hidden by color/depth compression on both reference GPUs), memory × k, the resolve.

**Attribute interpolation:** at the pixel center even when the triangle covers only
samples elsewhere in the pixel (center interpolation extrapolates outside the triangle);
`centroid` interpolation avoids that but changes derivatives. The engine's shading entry
points are shared with single-sampled views (the register rule), so MSAA uses center
interpolation; no artifact was visible in the test scenes (texture coordinates of
atlased materials could bleed: a known MSAA trait).

**Alpha to coverage (A2C):** for alpha-tested (masked) materials MSAA only antialiases the
triangle edges; the alpha test's own edge stays aliased. A2C turns the fragment's alpha
into a coverage mask (dithered per sample by the hardware), so the alpha edge gets k
levels. With the plain alpha the edge becomes a wide, blurry band; Golus (2017) sharpens
it to about one pixel around the cutoff, `(alpha − cutoff) / max(fwidth(alpha), ε) + 0.5`,
which keeps the edge where the alpha test puts it. With a depth prepass the coverage must
be decided there (the shading then tests depth for equality per sample); a depth-only
pipeline can use A2C: the fragment shader writes only the alpha at location 0 (Vulkan
requires the output variable, not an attachment). The engine does this (`prepassCoverage`).

**Resolve of HDR color:** the box average of HDR samples aliases at high-contrast edges: one
bright sample (a highlight, the sky) dominates the edge pixel after tone mapping, which
brings back the stair steps MSAA removed. Karis (2014) and Pettineo (2015) weight each
sample by 1 / (1 + luma) — the average the samples would have after a simple tone map —
and invert it, which equals the box filter where samples are dark or equal. The engine's
resolve does that (`msaa.slang`). A full tone-mapped resolve (tone map, average, inverse)
needs an invertible curve and costs more; the weighted form is what UE's MSAA path and
Pettineo's demo use.

**Depth for what reads the scene per pixel:** Vulkan 1.2's depth resolve modes
(`SAMPLE_ZERO` always, `MIN`/`MAX` on NVIDIA, AMD and Intel — both reference GPUs report
them) resolve depth at the end of a render pass (dynamic rendering's resolve attachments).
Nearest (min) for effects that should follow the front surface at a silhouette; farthest
(max) for an occlusion-culling pyramid of farthest depths, where the nearest would claim a
cell is covered when part of it shows what is behind (not conservative).

**Screen-space effects with MSAA** (what other engines do and what the engine does):

| Effect | What it reads | Bevy 0.16 | Godot 4.4 | UE5 forward | The engine |
|---|---|---|---|---|---|
| GTAO | depth (and normals) per pixel | MSAA must be off | resolved depth/normals | resolved depth | nearest-sample depth (prepass resolve); one AO value per pixel, read by each fragment |
| Contact shadows | depth | – | resolved depth | resolved depth | as AO |
| Subsurface scattering | diffuse light, albedo per pixel | – | resolved | – | the nearest subsurface sample's light/albedo (their depth and profile in alpha must stay one sample's); the scatter pass takes its center from the fragment's own samples (`SV_Coverage` input) |
| Reflections (probes, SSR) | depth, normal, roughness, weight, lit color | MSAA must be off | resolved | resolved | inputs of the nearest sample and of the farthest; the composite adds the front surface's reflection to its own samples only (`SV_Coverage` output, a depth tolerance from the surface's own slope) and, with probes, a second draw on the samples behind it; SSR traces the averaged lit image |
| Atmosphere, fog | depth | per pixel | per pixel | not documented | per sample (sample-rate shading of the full-screen pass, each sample's own depth) |
| Local / sun shadows | world position | – | – | – | in the shading, per fragment: unaffected |
| Transparency | depth test | MSAA | MSAA | MSAA | multisampled depth test; reactive mask resolved (average) |
| Motion vectors | per pixel | – | – | resolved | the nearest sample's (what TAA's depth dilation expects) |

Halos come from evaluating an effect for one surface and applying it to samples of
another. The per-sample atmosphere, the coverage-limited reflection composite and the
scatter's own-sample center remove those the test scenes showed; AO and contact shadows
remain one value per pixel (the front surface's), as in Godot and UE: at a silhouette the
samples behind get the front's occlusion. Per-sample AO would multiply its cost by k; a
bilateral fetch in the shading would add an MSAA copy of every AO shading variant (the
register rule forbids adding it to the existing ones). Measured in ADR 0075.

**MSAA and temporal upscalers:** TAA over an MSAA-resolved image works (fewer edges to
fix, the history converges sooner; Ryse, The Order: 1886); FSR 2/3 and DLSS reconstruct
from aliased jittered samples, and both vendors' integration guides ask for an input
without antialiasing (DLSS replaces MSAA; FSR's color input should be aliased): the
engine refuses those combinations.

## FXAA (Lottes 2009, 3.11 in 2011)

One pass on the final (gamma-space) image: luma of the pixel and its four neighbours; if
the local contrast is below `max(edge_threshold_min, max_luma × edge_threshold)`, nothing;
else the edge's direction from second differences (horizontal or vertical), its side (the
neighbour with the larger gradient), a search along the edge in both directions (steps of
1, 1.5, 2, … pixels through bilinear fetches half a pixel across) until the luma leaves the
edge's average, and a blend across the edge by the pixel's position on the span (the
nearer end, if the luma moves the way the pixel's does), plus subpixel smoothing of
single-pixel features (`subpix` 0.75). The cheapest post filter; it blurs textures
(sub-pixel smoothing) and does not see edges shorter than a pixel. NVIDIA published FXAA
3.11 in its samples; no patent was found (below). The engine's version is written from
this description with the 3.11 defaults (edge threshold 0.166, minimum 0.0833) and a
10-step search.

## MLAA (Reshetov 2009) and practical MLAA (Jimenez et al. 2011)

Reshetov's morphological antialiasing finds discontinuities, classifies the L, Z and U
shapes they form, and blends each pixel by the area a reconstructed line cuts from it: a
CPU algorithm (it shipped on the PS3's SPUs: God of War III). Jimenez et al. (GPU Pro 2,
"Practical Morphological Antialiasing") made it a three-pass GPU filter: edges into a
two-channel texture, line searches with bilinear fetches reading two edges at once, the
areas from a precomputed table indexed by the distances and crossing edges, then the
blend. SMAA is its successor and its reference code contains exactly that pipeline with
SMAA's additions switched off (diagonal patterns, corner rounding, local contrast
adaptation); the engine's MLAA is that configuration (8 search steps as in the paper,
luma edges at 0.1), honestly "Jimenez's MLAA as it lives in SMAA's code" rather than a
separate implementation.

## SMAA (Jimenez, Echevarria, Sousa, Gutierrez 2012)

Over MLAA: local contrast adaptation (an edge is dropped when a neighbour's is twice as
strong: no doubled edges), diagonal patterns (search along diagonals, their own area
table), corner rounding (sharp corners kept), predicated thresholding (optional), and
the modes:

- **SMAA 1x:** the three passes. Presets set the threshold and searches: Low (0.15, 4
  steps, no diagonals, no corners), Medium (0.1, 8), High (0.1, 16, diagonals 8, corners
  25 %), Ultra (0.05, 32, diagonals 16). Each preset is compile time in the reference: the
  engine compiles each to its own entry points, so a view pays for its own preset only.
- **SMAA S2x:** MSAA 2× with the standard sample pattern; each sample's image goes through
  SMAA 1x with its own area table offsets (`subsampleIndices`), the second blended at half
  weight. The reference recommends separating the samples in the tone-mapping pass (SMAA
  works on tone-mapped colors): the engine's MSAA resolve writes the average and half the
  samples' difference, the hook's additions apply to both, the split rebuilds each
  sample's HDR image after the hook, the tone mapper maps both, SMAA runs on both.
- **SMAA T2x:** two subpixel jitter positions (±¼ pixel on the diagonal, the same as S2x's
  samples), SMAA 1x per frame with that frame's offsets, then a resolve with the previous
  frame's SMAA result along the motion vectors, weighted down where the pixel's speed
  (kept in alpha by the blending pass) differs from the history's (CryENGINE 3's
  reprojection). No accumulation beyond two frames: little ghosting, and textures stay
  sharp (no mip bias needed).
- **SMAA 4x:** S2x and T2x: four samples over two frames, with their own offsets table.

The tables: `AreaTex` (RG8, 160 × 560: the orthogonal and diagonal areas for every
subsample offset) and `SearchTex` (R8, 64 × 16). The reference ships them as C headers and
DDS files generated by its Python scripts (MIT); the engine stores the headers' bytes
(`crates/engine_render/data/`) and uploads them once.

Implementations read: the reference (iryoku/smaa, MIT, 2013), Bevy's port (WGSL, the
reference's KTX2 tables), Godot 4.5's SMAA 1x (GLSL port). All three run SMAA after tone
mapping.

## Spatial upscaling

- **Nearest:** every output pixel takes one scene pixel: blocky, no blur; for non-integer
  ratios the blocks have uneven sizes (shimmer while moving). Free in the tone mapper
  (a nearest sampler).
- **Bilinear:** the default stretch: soft, cheap. Free in the tone mapper.
- **Integer scaling (pixel art):** the largest whole factor k with k × the render size
  fitting the output, the rest letterboxed; every scene pixel is a k × k square. Retro
  and pixel-art games render at a fixed internal resolution (320 × 180, 640 × 360);
  Godot 4.2's `stretch/scale_mode = integer`, the "integer scaling" of GPU drivers and
  emulators. The engine: a fixed `render_size` (or the factor nearest the quality's
  ratio), nearest sampling into the centered rectangle, a clear of the rest.
- **FSR 1 (AMD, 2021, MIT):** EASU, edge-adaptive spatial upsampling — a 12-tap
  approximation of a Lanczos-2 kernel, rotated and stretched along the local gradient
  direction from a 2 × 2 neighbourhood's luma, its negative lobe adapted to the edge
  strength, clamped to the four nearest texels (no ringing) — then RCAS, robust
  contrast-adaptive sharpening (a 5-tap cross whose lobe is solved for the most sharpening
  that does not clip, limited to 0.1875). Made for perceptual (tone-mapped) input in 0–1;
  AMD's guide puts it after tone mapping and before film grain and UI, or on HDR through a
  reversible tone map. The engine runs it at step 4 on HDR mapped by
  c·e / (1 + max(c·e)) (e: the exposure the tone mapper applies) and maps back, so bloom
  and tone mapping follow unchanged; at the output size RCAS alone sharpens.
  32-bit paths only (the 16-bit ones need `shaderFloat16` and help on RDNA's packed math;
  not measured).

### Catmull-Rom and Lanczos-3 (queue item 0c, checked 2026-10-10)

The owner asked for both (2026-10-02), Lanczos-3 with an anti-ringing clamp, and no
Lanczos-2 (FSR 1's EASU approximates it).

- **Catmull-Rom** (Catmull and Rom 1974; as an image interpolator Keys 1981, cubic
  convolution with a = -1/2; the B = 0, C = 1/2 member of Mitchell and Netravali's 1988
  family): 4 x 4 texels, per axis the weights of the fraction f past the second texel
  w0 = -f/2 + f^2 - f^3/2, w1 = 1 - 5f^2/2 + 3f^3/2, w2 = f/2 + 2f^2 - 3f^3/2,
  w3 = -f^2/2 + f^3/2 (they sum to 1). It interpolates (texel centers keep their
  values), is sharper than bilinear, and has one negative lobe per side: the kernel's
  minimum is -0.074, a step overshoots by 7.4 %.
- **Lanczos-3** (Duchon 1979; Turkowski 1990 recommends it for resampling):
  L(x) = sinc(x) sinc(x/3) for |x| < 3, separable, 6 x 6 texels. Between texels its
  weights do not sum to 1 (normalized). A wider pass band than Catmull-Rom's, and two
  lobes per side: the kernel's minimum is -0.147, a step overshoots by 11.8 %.
- **Who offers what.** Unreal's `r.Upscale.Quality`: 0 nearest, 1 bilinear, 2 a
  directional blur with unsharp mask, 3 (the default) a 5-tap Catmull-Rom "approximating
  Lanczos 2", 4 a 13-tap Lanczos-3, 5 a 36-tap Gaussian unsharp mask. Unity HDRP's
  dynamic resolution: Catmull-Rom (four bilinear samples) at the end of post-processing,
  CAS, FSR 1, TAA upscale. Godot 4: bilinear, FSR 1, FSR 2.2, MetalFX (no bicubic for 3D
  scaling). mpv / libplacebo (LGPL-2.1, read only, no code taken): lanczos, spline36,
  ewa_lanczos(sharp) and friends with an `antiringing` strength from 0 to 1 that clamps
  each output to the range of the source pixels under the kernel's central lobe.
- **Ringing in HDR.** A negative lobe's undershoot scales with the contrast it sits on.
  Unreal and Unity scale display-encoded 0-1 colors after tone mapping, where it is a
  faint halo; the engine's tone mapper scales the HDR scene (the bilinear stretch does),
  where a sun of 1000 next to sky of 1 pulls its neighbours to -73 (Catmull-Rom) or -117
  (Lanczos-3): black rings around every highlight once negative values clip. (The same
  in linear light elsewhere: oximg issue #72, "linear-light Lanczos3 rings around
  light-on-dark text".) Remedies: scale after tone mapping (a different frame order:
  bloom and tone mapping at the render size), a reversible tone map per texel (FSR 1 at
  step 4, TAAU), or the **anti-ringing clamp**: the result limited to the range of the
  2 x 2 texels around the sample (FSR 1's EASU does exactly this; libplacebo's
  `antiringing` 1.0 is the same idea). The clamp removes every overshoot whatever the
  contrast and keeps the kernel's steeper edges; it costs a min and a max of texels the
  kernel reads anyway. Chosen, for both kernels.
- **GPU implementations.** The usual trick merges pairs of same-sign weights into one
  bilinear fetch at an offset between the two texels (Sigg and Hadwiger, "Fast
  Third-Order Texture Filtering", GPU Gems 2, 2005: 4 fetches for a cubic B-spline; 9 for
  Catmull-Rom, whose outer weights are negative, e.g. MJP's MIT gist; 5 with the corners
  dropped, Jimenez's Filmic SMAA 2016, Unreal's and Unity's). **ATI's US 8,675,007 B2
  claims that trick** (see Patents): the engine reads each texel with `Load` instead.
  Measured (aa_bench, Sponza 1080p from 67 %): reading the 4 x 4 or 6 x 6 texels with
  gathers (`GatherRed/Green/Blue` per 2 x 2 quad: 12 and 27 instructions instead of 16
  and 36 loads) was slower on both GPUs (RTX 0.04 -> 0.06 ms and 0.10 -> 0.16 ms, Radeon
  0.91 -> 1.06 and 3.13 -> 3.32 ms).
- **Separable Lanczos-3.** 36 loads of RGBA16F for each output pixel make Lanczos-3
  bound by the texture cache on the Radeon (3.1 ms). In two passes, the rows into an
  image of the output's width and the render height, then the columns, it reads 6 + 6
  texels: 1.8-2.2 ms on the Radeon, 0.07 ms on the RTX (from 0.10), for an
  intermediate of 11 MB at 1080p from 67 %. Each pass clamps to its two nearest texels
  (libplacebo clamps its separable scalers the same way), so the result still stays in
  the 2 x 2 range; the per-pass clamp is a little stricter than one clamp over the 2 x 2
  range (still FLIP 0.0351 instead of 0.0347, PSNR 34.65 instead of 34.61 dB, flicker
  1.62 instead of 1.76 % on the RTX). Catmull-Rom stays in one pass: its 16 loads cost
  less than a pass and an intermediate image would.

## Combinations

| | spatial scaling | temporal (TAAU) | temporal (FSR 3.1, DLSS) | SMAA T2x | post FXAA / MLAA / SMAA |
|---|---|---|---|---|---|
| MSAA 2/4/8 | yes | yes | **no** (they want aliased jittered input) | yes (2×: SMAA 4x) | yes (2× + SMAA: S2x) |
| spatial | – | one slot: either | one slot | one slot | yes |
| SMAA T2x | – | – | – | – | needs SMAA (its spatial half), the output size |

Post filters after a temporal upscaler are allowed (sharpening-like, some games do it);
they rarely help.

## Patents (searched 2026-10-02; an engineer's search, not a legal opinion)

Google Patents and Justia searches by technique name, inventor (Reshetov, Lottes,
Jimenez) and assignee (Intel, NVIDIA, AMD, Sony), and the references the papers cite:

| Technique | Found | Conclusion |
|---|---|---|
| MSAA | hardware patents of the 1990s (SGI RealityEngine, Akeley) | expired; a Vulkan core feature |
| Alpha to coverage | SGI-era (1993-1996) | expired; core Vulkan |
| Weighted (tone-mapped) resolve | none; published by Karis (2014), Pettineo (2015) | used |
| FXAA | none on FXAA itself; Lottes's NVIDIA patents found are on MSAA hardware ("enhanced multi-sample anti-aliasing", filed 2016) | used, written from the description |
| MLAA (Reshetov) | none on the algorithm; Intel's CMAA application US 2015/0093026 A1 (abandoned 2020; EP2854102B1 granted 2017) and its successors US 10,902,605 B2, US 11,354,807 B2 (CMAA 2 with MSAA) claim CMAA's specific shape handling; Sony's US 8,619,094 B2 claims MLAA of a stereoscopic re-projection | used (Jimenez's GPU MLAA); CMAA not implemented |
| SMAA | none | used (MIT code) |
| FSR 1 (EASU, RCAS) | none on EASU/RCAS; AMD's "Gaming Super Resolution" US 2021/0150669 A1 is a neural upscaler | used (MIT code; MIT carries no explicit patent grant: question for the team in ADR 0075) |
| Integer / nearest / bilinear scaling | none applicable | used |
| Catmull-Rom (cubic convolution), Lanczos | none (Catmull and Rom 1974, Keys 1981, Duchon 1979: published, any patent long expired) | used (checked 2026-10-10) |
| Higher-order filters from combined bilinear fetches | **ATI Technologies ULC (AMD), US 8,675,007 B2**, "System and method for higher level filtering by combination of bilinear results", filed 2005-10-06, granted 2014, maintenance fees paid through 2025, expires 2032-09-11. Claims 4, 9 and 13: weights and new sub-texel offsets generated from the fraction, bilinear fetches at the new coordinates, combined with the weights (claim 9: by a shader); claims 1-3 read literally on any combination of bilinear fetches. Sigg and Hadwiger's GPU Gems 2 chapter (March 2005) predates the filing; validity is for courts | **not used** in the new filters (each texel read with `Load`). **TAAU's optional Catmull-Rom history (`HistoryFilter::CatmullRom`, 5 bilinear fetches, ADR 0043; off by default) uses it: question for the team (2026-10-10)** |
| Anti-ringing clamp to the 2 x 2 range | AMD's US 12,293,485 B2 ("Super resolution upscaling", FSR 2, priority 2022) describes clamping a 5 x 5 Lanczos result to the center 2 x 2 range inside a temporal upscaler (a previously upscaled frame, motion vectors, locks); Micron's US 7,333,674 B2 (ringing suppression in resizing) expired 2021 | used on single frames (prior art: FSR 1's EASU, MIT, July 2021; mpv's antiringing) |

Nothing was left out for patents; the bilinear-fetch trick (2026-10-10) is avoided by
reading texels one by one.

## Sources

- A. Reshetov, "Morphological Antialiasing", HPG 2009.
- J. Jimenez, B. Masia, J. I. Echevarria, F. Navarro, D. Gutierrez, "Practical Morphological
  Antialiasing", GPU Pro 2, 2011; www.iryoku.com/mlaa.
- J. Jimenez, J. I. Echevarria, T. Sousa, D. Gutierrez, "SMAA: Enhanced Subpixel
  Morphological Antialiasing", Computer Graphics Forum (Eurographics 2012).
- SMAA reference code, github.com/iryoku/smaa, commit 71c806a838bd (2013-11-06), MIT:
  `SMAA.hlsl`, `Textures/AreaTex.h`, `Textures/SearchTex.h`, `Scripts/*.py`.
- T. Lottes, "FXAA", NVIDIA white paper, 2009; FXAA 3.11 (2011) description in NVIDIA's
  samples and blog posts.
- J. Jimenez et al., "Filtering Approaches for Real-Time Anti-Aliasing", SIGGRAPH 2011
  course (MLAA, FXAA, SRAA, DLAA, CMAA's predecessors compared).
- B. Karis, "High Quality Temporal Supersampling", SIGGRAPH 2014 Advances (weighted
  resolve).
- M. Pettineo, "MSAA Resolve + Temporal AA", 2015, and his MSAAFilter demo (MIT).
- B. Golus, "Anti-aliased Alpha Test: The Esoteric Alpha To Coverage", 2017.
- AMD FidelityFX Super Resolution 1: `ffx_fsr1.h` in FidelityFX SDK 1.1.4 (vendored,
  MIT); gpuopen.com/manuals/fidelityfx_sdk/techniques/super-resolution-spatial/.
- Vulkan 1.3 specification: multisampling, sample shading, `SampleMask`,
  `VkRenderingAttachmentInfo` resolve modes, `VkPhysicalDeviceDepthStencilResolveProperties`.
- Godot 4.4/4.5 (MIT): `servers/rendering/renderer_rd` (MSAA resolves for SSAO/SSR inputs,
  FXAA, SMAA, FSR 1 scaling, `scale_mode = integer`).
- Bevy 0.16 (MIT/Apache): `bevy_core_pipeline::{fxaa, smaa, contrast_adaptive_sharpening}`,
  `bevy_pbr::ssao` and `ssr` (MSAA must be off).
- Unreal Engine 5 documentation: "Forward Shading Renderer" (MSAA, resolved depth for
  screen-space effects).
- Patents: US 2015/0093026 A1, EP 2854102 B1, US 10,902,605 B2, US 11,354,807 B2 (Intel,
  CMAA); US 8,619,094 B2 / WO 2012/094076 (Sony); US 2021/0150669 A1 (AMD);
  US 8,675,007 B2, US 12,293,485 B2 (ATI / AMD); US 7,333,674 B2 (Micron).
- E. Catmull, R. Rom, "A Class of Local Interpolating Splines", Computer Aided Geometric
  Design, 1974. R. Keys, "Cubic Convolution Interpolation for Digital Image Processing",
  IEEE Trans. ASSP 29(6), 1981. D. Mitchell, A. Netravali, "Reconstruction Filters in
  Computer Graphics", SIGGRAPH 1988.
- C. Duchon, "Lanczos Filtering in One and Two Dimensions", J. Applied Meteorology, 1979.
  K. Turkowski, "Filters for Common Resampling Tasks", Graphics Gems, 1990.
- C. Sigg, M. Hadwiger, "Fast Third-Order Texture Filtering", GPU Gems 2, ch. 20, 2005.
  M. Pettineo (MJP), "Bicubic Catmull-Rom texture filtering with 9 bilinear taps" (gist,
  MIT). J. Jimenez, "Filmic SMAA", SIGGRAPH 2016 (5-tap Catmull-Rom).
- Unreal Engine documentation, `r.Upscale.Quality`; Unity HDRP documentation, "Dynamic
  resolution" (upscale filters); libplacebo `docs/options.md` (`antiringing`, LGPL-2.1,
  read only); oximg issue #72 (linear-light Lanczos-3 ringing).
