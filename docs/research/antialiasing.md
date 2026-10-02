# Antialiasing and scaling modes: MSAA, FXAA, MLAA, SMAA, spatial upscalers

Checked 2026-10-02. Sources at the end. What a game should be able to offer its players
besides the temporal methods the engine already had (TAAU, FSR 3.1, DLSS; ADR 0043, 0060,
0063, 0072), how each method works, how it combines with the others and with the
screen-space effects, which open implementations exist and under which license, and what
is known about patents. The decision and the measurements are in ADR 0075.

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
| **Spatial upscalers** (nearest, bilinear, integer, FSR 1) | render size → output | nothing (they scale) | one pass or none | none |

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

Nothing was left out for patents.

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
  CMAA); US 8,619,094 B2 / WO 2012/094076 (Sony); US 2021/0150669 A1 (AMD).
