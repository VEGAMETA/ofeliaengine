# 0075. Every antialiasing and scaling mode: MSAA, FXAA, MLAA, SMAA, nearest, bilinear, integer, FSR 1

- Status: Accepted
- Date: 2026-10-02
- Builds on: [ADR 0072](0072-frame-renderer.md) (the frame renderer and its order),
  [ADR 0043](0043-motion-vectors-and-taau.md) (TAAU), [ADR 0060](0060-fsr-3-1-upscaling.md)
  (FSR 3.1), [ADR 0063](0063-dlss-through-streamline.md) (DLSS),
  [ADR 0042](0042-depth-prepass.md) (the depth prepass), [ADR 0058](0058-ambient-occlusion.md),
  [ADR 0059](0059-subsurface-scattering.md), [ADR 0069](0069-sky-atmosphere-and-fog.md),
  [ADR 0071](0071-local-light-and-contact-shadows.md),
  [ADR 0073](0073-reflection-probes-and-screen-space-reflections.md) (the screen-space
  effects MSAA must keep working), [ADR 0062](0062-golden-images-and-perf-gate.md)
- Research: [docs/research/antialiasing.md](../research/antialiasing.md)

## Context

- Team decision (2026-10-01): "the engine must implement all variants so the developer
  can decide what the player is allowed, including nearest and bilinear". The frame
  renderer offered `Antialiasing::Off | Temporal(TAAU | FSR | DLSS)` and a render size;
  below the output size `Off` was stretched bilinearly by the tone mapper.
- What a game asks for: MSAA for crisp geometry without temporal blur (VR, racing, games
  that hate ghosting), cheap post filters, SMAA's modes, pixel-perfect integer scaling for
  pixel art, nearest scaling, AMD's FSR 1 where temporal upscaling is not wanted, and any
  sensible combination; a menu built from what this GPU supports.
- Constraints: a mode that is not selected costs nothing (no passes, no pipelines, no
  registers: CLAUDE.md §5's register rule; the forward shading's 48/64/64/80 VGPRs on the
  Radeon must not move); every screen-space effect must keep working with MSAA; no
  patented algorithm; existing golden images unchanged; zero heap allocations per frame.

## Options

**Shape of the setting**

1. *One enum of named modes* (UE's `r.AntiAliasingMethod`, Unity's per-camera enum):
   simple menus, but every combination (MSAA 4× + SMAA, FSR 1 + SMAA, TAA over MSAA)
   needs its own variant, and the list grows with every new mode.
2. *Independent parts (chosen)*: `Antialiasing { samples: Msaa, upscale: Upscale, post:
   PostAntialiasing }` — raster samples (1, 2, 4, 8), how the render size reaches the
   output (nearest, bilinear, integer, FSR 1, a temporal upscaler, SMAA T2x), a filter
   after tone mapping (FXAA, MLAA, SMAA presets). Named constants cover the usual modes
   (`OFF`, `TAAU`, `FSR`, `DLSS`, `FXAA`, `SMAA_1X`, `SMAA_T2X`, `SMAA_S2X`, `SMAA_4X`,
   `msaa(n)`); SMAA's composite modes fall out of the parts (2 samples + SMAA is S2x,
   SMAA T2x's jitter + SMAA is T2x, both are 4x), as in the SMAA paper.
   `FrameRenderer::check` validates a combination (the error says why it cannot run),
   `antialiasing_support()` lists the sample counts the scene's targets support and the
   upscalers available, `prepare` refuses invalid ones with the same error. Refused:
   SMAA T2x without SMAA or below the output size, MSAA under FSR 3.1 or DLSS (both
   vendors ask for aliased input), sample counts the GPU's limits or formats lack.

**MSAA in the forward renderer**

1. *Dynamic sample count* (`VK_EXT_extended_dynamic_state3`): every existing pipeline
   would become sample-count dynamic, which changes how the AMD driver compiles the
   single-sampled ones (the register rule) and is optional on drivers.
2. *A second `ForwardRenderer` per sample count*: duplicates its state (the local-shadow
   atlas and its cache, the subsurface profiles, the settings).
3. *A pipeline set per sample count inside the renderer (chosen)*: made by
   `set_samples` when a view first asks for it (the frame renderer's `prepare`) and
   extended as features are turned on; single-sampled views keep the renderer's own
   pipelines and code paths untouched. Multisampled views always draw the depth prepass,
   so the set needs only the after-prepass shading (equal depth test, per sample), which
   also gives masked materials one place to turn alpha into coverage (`prepassCoverage`:
   alpha sharpened to a pixel around the cutoff, Golus 2017).

**The HDR resolve:** box average (aliases bright edges after tone mapping) or the
samples weighted by 1 / (1 + luma) (Karis 2014; their tone-mapped average, chosen;
equal to the box filter where samples are dark or equal). Motion vectors: the nearest
sample's (TAA's depth dilation expects the front surface's motion). Depth: resolved by
the prepass itself (dynamic rendering's depth resolve): nearest for what reads the scene
per pixel, farthest for the occlusion-culling pyramid (conservative).

**Screen-space effects with MSAA** (per effect; the alternatives were resolving
everything to sample 0 or the average (halos), or sample-rate shading of everything
(k× the cost)):

| Effect | Choice | Why |
|---|---|---|
| Ambient occlusion, contact shadows | computed from the nearest-sample depth, one value per pixel, read by each fragment | per-sample AO costs k×; a bilateral read would need MSAA copies of every AO shading variant |
| Subsurface scattering | light and albedo resolved to the nearest subsurface sample's; the scatter pass takes its center from the fragment's own samples (`SV_Coverage` input, `scatterFragmentMs`) | averages would mix depth and profile index in alpha with empty samples |
| Reflections (probes, SSR) | inputs of the nearest and of the farthest sample; the composite adds the front surface's reflection to its own samples only (`SV_Coverage` output, a tolerance from the surface's own depth slope) and, with probes, a second draw on the samples behind (`compositeProbesFarMs`, stops inside surfaces); SSR traces the averaged lit image | one reflection for the whole pixel spilled over silhouettes |
| Atmosphere, fog | per sample (sample-rate shading of the full-screen pass, each sample's own depth) | per pixel put the far side's fog over the near side |
| Transparency | multisampled depth test; the reactive mask averaged | – |
| Skinned motion | `Skinning::set_motion_samples`: its pass into the multisampled targets | its depth test must see the samples |
| The hook (`add_view_with`) | gets the resolved color, depth and motion | effects of the game's own see one value per pixel |

**Filters after tone mapping:** the tone mapper writes a perceptual image (sRGB over the
output's brightest value, or PQ; `tonemap.slang`'s new `fragmentPerceptual` entry point,
`fragmentMain`'s SPIR-V unchanged) that the filters read and the last pass decodes into
the output's encoding: SDR, scRGB and HDR10 outputs all work. FXAA: written from the
FXAA 3.11 description. SMAA and MLAA: the reference code (MIT) behind porting macros, one
file and pipeline pair per preset (presets are compile time in the reference: a view
pays for its own preset's registers only); MLAA is Jimenez's GPU MLAA as it lives there
(SMAA's diagonals, corners and local contrast adaptation off).

**SMAA T2x and 4x:** the reference's jitter (±¼ pixel, the samples of MSAA 2×; ±⅛ for 4x)
as a fixed jitter pattern of the view's `Temporal`, the temporal resolve after tone
mapping as in the reference (a per-view history of the perceptual image). Not an
`Upscaler`: its resolve runs after tone mapping.

**FSR 1:** at step 4 on HDR through a reversible tone map with the engine's exposure
(bloom and tone mapping follow unchanged), or after tone mapping on the perceptual
image (AMD's preferred place, but bloom and tone mapping would then run at the render
size: a different frame order). Step 4 chosen, as the team brief says.

## Measurements

ref-1, release, `VK_LOADER_LAYERS_DISABLE=~implicit~`, under the machine's GPU lock with
a quiet CPU. Sponza 1920 × 1080 with every feature (sky, atmosphere, height fog, GTAO
High, contact shadows, Sponza's local lights with shadows, six reflection probes, SSR,
bloom), `cargo run -p engine_golden --release --example aa_bench`. Passes: the GPU's
busy time over every pass (median of 100 frames); "vs off": against no antialiasing at
the output size; "own": the mode's own passes (MSAA's other cost is in the scene
passes). FLIP / PSNR against the frame at 4 × 4 the size without antialiasing,
averaged in linear light (a still camera; "moving": the last of 10 frames of a camera
strafing 4 mm per frame, temporal modes accumulating while it moves). Shimmer: the mean
of |(F[t] − F[t−1]) − (R[t] − R[t−1])| in display-encoded luma (0–255) over the
sequence, F the mode's frames, R the reference's: change that the reference does not
have (crawling edges, flicker, lag); "plain" on Sponza lit by sun and sky alone (no
stochastic effect).

**RTX 4060 Ti**

| mode | frame ms | vs off ms | own passes ms | still FLIP | still PSNR dB | moving FLIP | moving PSNR dB | flicker % | flicker %, plain |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| off | 2.18 | +0.00 | – | 0.0240 | 37.39 | 0.0241 | 37.38 | 1.85 | 3.36 |
| FXAA | 2.21 | +0.03 | 0.05 | 0.0255 | 38.34 | 0.0255 | 38.30 | 1.45 | 2.21 |
| SMAA 1x medium | 2.39 | +0.21 | 0.11 | 0.0245 | 38.49 | 0.0245 | 38.43 | 1.67 | 2.80 |
| MLAA | 2.40 | +0.22 | 0.11 | 0.0246 | 38.37 | 0.0246 | 38.36 | 1.68 | 2.80 |
| SMAA 1x low | 2.29 | +0.11 | 0.10 | 0.0243 | 38.41 | 0.0244 | 38.37 | 1.74 | 3.12 |
| SMAA 1x high | 2.42 | +0.24 | 0.14 | 0.0244 | 38.55 | 0.0243 | 38.54 | 1.69 | 2.87 |
| SMAA 1x ultra | 2.43 | +0.26 | 0.16 | 0.0248 | 38.53 | 0.0247 | 38.51 | 1.45 | 2.43 |
| SMAA T2x | 2.54 | +0.36 | 0.21 | 0.0237 | 39.03 | 0.0255 | 38.43 | 1.00 | 1.70 |
| SMAA S2x | 3.30 | +1.12 | 0.59 | 0.0246 | 38.90 | 0.0245 | 38.92 | 1.47 | 2.08 |
| SMAA 4x | 3.45 | +1.27 | 0.75 | 0.0243 | 39.48 | 0.0259 | 38.90 | 0.90 | 1.41 |
| MSAA 2× | 2.90 | +0.72 | 0.20 | 0.0240 | 38.52 | 0.0239 | 38.56 | 1.60 | 2.52 |
| MSAA 4× | 3.33 | +1.16 | 0.29 | 0.0239 | 39.33 | 0.0239 | 39.35 | 1.35 | 1.82 |
| MSAA 8× | 4.30 | +2.12 | 0.43 | 0.0243 | 39.29 | 0.0242 | 39.32 | 1.33 | 1.63 |
| MSAA 4× + SMAA | 3.34 | +1.16 | 0.42 | 0.0244 | 39.23 | 0.0244 | 39.15 | 1.37 | 1.70 |
| MSAA 4× + TAA | 3.70 | +1.52 | 0.58 | 0.0276 | 38.73 | 0.0332 | 36.17 | 0.27 | 0.15 |
| TAA | 2.48 | +0.30 | 0.15 | 0.0266 | 38.87 | 0.0321 | 36.43 | 0.26 | 0.18 |
| TAAU 67 % | 1.52 | -0.65 | 0.13 | 0.0328 | 36.92 | 0.0385 | 34.99 | 0.27 | 0.15 |
| TAAU 50 % | 1.16 | -1.02 | 0.13 | 0.0438 | 33.88 | 0.0492 | 32.71 | 0.29 | 0.13 |
| FSR 3.1 Native AA | 3.06 | +0.88 | 0.83 | 0.0269 | 39.85 | 0.0241 | 38.90 | 0.60 | 1.14 |
| FSR 3.1 Quality (67 %) | 1.95 | -0.23 | 0.57 | 0.0335 | 37.64 | 0.0301 | 37.44 | 0.50 | 0.72 |
| DLAA | 3.22 | +1.04 | 0.96 | 0.0190 | 42.77 | 0.0242 | 40.42 | 0.50 | 0.79 |
| DLSS Quality (67 %) | 2.36 | +0.08 | 0.95 | 0.0222 | 41.03 | 0.0263 | 39.12 | 0.50 | 0.74 |
| bilinear 67 % | 1.29 | -0.99 | – | 0.0370 | 34.50 | 0.0365 | 34.58 | 1.17 | 1.78 |
| bilinear 50 % | 0.90 | -1.37 | – | 0.0512 | 31.97 | 0.0506 | 31.99 | 1.28 | 1.80 |
| nearest 67 % | 1.29 | -0.98 | – | 0.0418 | 32.65 | 0.0413 | 32.66 | 2.25 | 4.30 |
| nearest 50 % | 0.90 | -1.37 | – | 0.0489 | 31.35 | 0.0483 | 31.43 | 2.00 | 3.13 |
| integer 50 % | 0.90 | -1.37 | – | 0.0489 | 31.35 | 0.0483 | 31.43 | 2.00 | 3.13 |
| FSR 1 RCAS only (100 %) | 2.33 | +0.06 | 0.06 | 0.0405 | 30.90 | 0.0423 | 30.53 | 17.11 | 24.09 |
| FSR 1 67 % | 1.54 | -0.73 | 0.20 | 0.0364 | 33.87 | 0.0368 | 33.69 | 4.86 | 7.82 |
| FSR 1 50 % | 1.17 | -1.11 | 0.20 | 0.0470 | 31.97 | 0.0464 | 32.07 | 3.45 | 5.06 |
| FSR 1 67 % + MSAA 4× | 2.03 | -0.25 | 0.30 | 0.0358 | 34.65 | 0.0354 | 34.69 | 4.44 | 6.39 |
| FSR 1 67 % + SMAA | 1.68 | -0.59 | 0.34 | 0.0362 | 34.16 | 0.0364 | 33.98 | 4.82 | 7.71 |

**Radeon iGPU (2 CU)**

| mode | frame ms | vs off ms | own passes ms | still FLIP | still PSNR dB | moving FLIP | moving PSNR dB | flicker % | flicker %, plain |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| off | 58.17 | +0.00 | – | 0.0233 | 37.64 | 0.0234 | 37.61 | 1.91 | 3.48 |
| FXAA | 59.68 | +1.51 | 1.42 | 0.0251 | 38.62 | 0.0251 | 38.55 | 1.50 | 2.31 |
| SMAA 1x medium | 60.60 | +2.42 | 2.19 | 0.0240 | 38.79 | 0.0241 | 38.70 | 1.73 | 2.90 |
| MLAA | 60.33 | +2.16 | 2.18 | 0.0242 | 38.68 | 0.0242 | 38.59 | 1.74 | 2.89 |
| SMAA 1x low | 60.10 | +1.93 | 1.95 | 0.0238 | 38.71 | 0.0239 | 38.64 | 1.80 | 3.22 |
| SMAA 1x high | 60.61 | +2.44 | 2.49 | 0.0239 | 38.86 | 0.0239 | 38.82 | 1.76 | 2.99 |
| SMAA 1x ultra | 61.44 | +3.27 | 3.25 | 0.0244 | 38.84 | 0.0244 | 38.79 | 1.52 | 2.58 |
| SMAA T2x | 61.25 | +3.08 | 3.45 | 0.0233 | 39.44 | 0.0252 | 38.71 | 1.06 | 1.82 |
| SMAA S2x | 77.29 | +19.12 | 8.22 | 0.0241 | 39.29 | 0.0240 | 39.26 | 1.53 | 2.19 |
| SMAA 4x | 78.67 | +20.50 | 10.01 | 0.0233 | 39.95 | 0.0251 | 39.27 | 0.96 | 1.51 |
| MSAA 2× | 70.32 | +12.15 | 2.18 | 0.0233 | 38.82 | 0.0232 | 38.83 | 1.67 | 2.67 |
| MSAA 4× | 78.73 | +20.56 | 2.96 | 0.0231 | 39.71 | 0.0231 | 39.69 | 1.42 | 1.98 |
| MSAA 8× | 94.31 | +36.14 | 5.31 | 0.0235 | 39.67 | 0.0235 | 39.66 | 1.40 | 1.78 |
| MSAA 4× + SMAA | 81.38 | +23.21 | 5.38 | 0.0239 | 39.62 | 0.0240 | 39.49 | 1.44 | 1.84 |
| MSAA 4× + TAA | 83.72 | +25.54 | 8.23 | 0.0275 | 39.00 | 0.0329 | 36.30 | 0.26 | 0.15 |
| TAA | 61.61 | +3.43 | 3.84 | 0.0264 | 39.17 | 0.0319 | 36.55 | 0.26 | 0.17 |
| TAAU 67 % | 34.47 | -23.71 | 3.86 | 0.0328 | 36.87 | 0.0386 | 34.95 | 0.27 | 0.14 |
| TAAU 50 % | 25.23 | -32.94 | 3.85 | 0.0439 | 33.73 | 0.0496 | 32.63 | 0.29 | 0.12 |
| FSR 3.1 Native AA | 77.61 | +19.44 | 19.86 | 0.0243 | 40.42 | 0.0236 | 39.22 | 0.61 | 1.16 |
| FSR 3.1 Quality (67 %) | 43.06 | -15.11 | 12.37 | 0.0327 | 37.53 | 0.0297 | 37.34 | 0.50 | 0.69 |
| bilinear 67 % | 29.59 | -28.59 | – | 0.0356 | 34.77 | 0.0353 | 34.82 | 1.21 | 1.90 |
| bilinear 50 % | 19.71 | -38.47 | – | 0.0500 | 32.09 | 0.0493 | 32.10 | 1.33 | 1.91 |
| nearest 67 % | 29.62 | -28.56 | – | 0.0412 | 32.75 | 0.0408 | 32.75 | 2.51 | 5.18 |
| nearest 50 % | 19.72 | -38.45 | – | 0.0476 | 31.44 | 0.0470 | 31.53 | 2.14 | 3.66 |
| integer 50 % | 19.68 | -38.49 | – | 0.0476 | 31.44 | 0.0470 | 31.53 | 2.14 | 3.66 |
| FSR 1 RCAS only (100 %) | 59.31 | +1.14 | 1.22 | 0.0421 | 30.33 | 0.0433 | 30.02 | 19.58 | 27.32 |
| FSR 1 67 % | 35.16 | -23.01 | 4.89 | 0.0364 | 33.90 | 0.0368 | 33.74 | 6.07 | 10.31 |
| FSR 1 50 % | 25.52 | -32.65 | 4.89 | 0.0465 | 32.05 | 0.0459 | 32.14 | 3.84 | 6.16 |
| FSR 1 67 % + MSAA 4× | 46.12 | -12.05 | 6.11 | 0.0358 | 34.68 | 0.0352 | 34.79 | 5.64 | 8.92 |
| FSR 1 67 % + SMAA | 37.79 | -20.39 | 7.43 | 0.0362 | 34.20 | 0.0365 | 34.04 | 6.00 | 10.14 |

(Rows from runs on one afternoon with a quiet CPU; "vs off" against the "off" of the same
run, `own passes` the mode's own passes: MSAA's other cost is in the scene passes. The
spatial rows on the RTX and DLSS Quality are from a second run. Measure your own scenes
with `cargo run -p engine_golden --release --example aa_bench`.)

**MSAA's edges with every screen-space effect** (`tests/golden/tests/aa_modes.rs`,
procedural scene 320 × 180, edge pixels of the scene's silhouettes, error against 4 × 4
supersampling; RTX, the Radeon within 1 %): MSAA 4× cuts the edge error by 34 % without
effects and by 18 % with all of them (ambient occlusion, contact shadows, local shadows,
subsurface scattering, probes, SSR, atmosphere, fog, occlusion culling); 195 edge pixels
come closer to the reference, 70 go farther. Per effect (`msaa_edge_gain_per_effect`):
none 34 %, local shadows 33 %, subsurface 33 % (6 % before its own-sample scatter),
glossy floor 35 %, fog 31 %, atmosphere 28 % (per sample), AO 28 %, SSR 27 % (7 % before
the coverage-limited composite), contact shadows 26 %, probes 20 % (−31 % before the
two-sided composite). What is left is AO's and contact shadows' one value per pixel.

**Registers** (`ENGINE_PIPELINE_STATS=1 ENGINE_ADAPTER=radeon scene_bench --sky
--pipeline-stats --msaa 4`): the single-sampled opaque shading stays at 48 / 64 / 64 / 80
VGPRs (plain, local lights, environment, both), the multisampled set has the same
48 / 64 / 64 / 80, the atmosphere's pass 35 (per sample 36). No existing SPIR-V changed.

**Allocations:** 0 per frame in steady state for off, TAAU, FSR 3.1, FSR 1, integer,
FXAA, MSAA 4× + SMAA, SMAA 4x, MSAA 2× + TAAU (`engine_frame/tests/zero_alloc.rs`).

## Decision

- `engine_frame::Antialiasing` is three parts: `samples: Msaa` (X1, X2, X4, X8),
  `upscale: Upscale` (Nearest, Bilinear, Integer, Fsr1, Temporal(Taau | Fsr | Dlss),
  SmaaT2x), `post: PostAntialiasing` (Off, Fxaa, Mlaa, Smaa(Low | Medium | High |
  Ultra)). `FrameRenderer::check` / `antialiasing_support` / `prepare`'s
  `FrameError::Antialiasing(InvalidAntialiasing)` as above. `ViewSettings` gains
  `sharpness` (FSR 1's RCAS) and `render_size` (a fixed render size for pixel art; with
  `Integer`, shown at the largest whole multiple, centered, letterboxed, projected with
  its own aspect). The runtime's `Camera` stores the three parts in its `u32` (old
  values keep their meaning).
- **Every mode costs nothing until a view uses it:** MSAA's pipeline sets, the resolves,
  FSR 1, the perceptual tone mapper, each filter and SMAA preset, SMAA's tables and T2x's
  history are made by the first `prepare` that asks; nothing is decoded or allocated per
  frame afterwards (the zero-allocation test covers them). No existing shader changed:
  every new path has entry points of its own (`prepassCoverage`, `compositeFragmentMs`,
  `scatterFragmentMs`, the `*Ms` reflection composites, `fragmentPerceptual`), so the
  single-sampled forward shading is still 48/64/64/80 VGPRs on the Radeon and every
  existing golden image renders byte-identical.
- **MSAA** as described: a pipeline set per sample count, the depth prepass always on,
  alpha to coverage there, the resolves and the per-effect choices of the table above.
- **Post filters** after tone mapping, on a perceptual image, before UI (callers draw UI
  after `add_view`); SMAA's composite modes from the parts.
- **FSR 1** at step 4 on HDR through a reversible tone map with the engine's exposure.
- **Defaults per device class unchanged** (discrete: TAA, integrated: TAAU from 67 %):
  the measurements below confirm them for a game that wants the best image per
  millisecond; the table is in the dev guide for games that want something else.

## Measured defaults' check and recommendations

- **The best image when it is available:** DLAA / DLSS (RTX only, the player's DLLs):
  the best still and moving image and almost no flicker; DLSS Quality costs about what no
  antialiasing at 100 % costs.
- **Sharp and stable without temporal blur:** MSAA 4× (RTX +1.2 ms, Radeon +20.6 ms:
  +53 % / +35 % of the frame): the best moving image of all but DLAA (its edges do not
  blur when the camera moves), flicker almost halved against no antialiasing. MSAA 8× costs
  twice as much and gives nothing more on Sponza (its aliasing is mostly inside surfaces,
  where samples do not help). MSAA 2× is half the gain for 60 % of the cost.
- **The least flicker:** the temporal modes (TAA, TAAU, FSR 3.1, DLSS): 0.1–0.8 % of
  pixels against 3.4 % without antialiasing. The price is softness in motion (TAA's moving
  FLIP 0.032 against 0.024 without antialiasing) and ghosting; MSAA under TAA adds cost
  and nothing visible here.
- **Cheap and safe:** SMAA 1x High: +0.14 ms on the RTX, +2.5 ms on the Radeon, +1.2 dB,
  edges smoothed without blurring textures. FXAA is cheaper still (0.05 / 1.4 ms) and
  removes a third of the flicker but blurs textures (the worst still FLIP of the filters).
  MLAA lands between SMAA Low and Medium; SMAA Ultra is not better than High on Sponza.
- **SMAA's composite modes:** T2x (+0.2 / 3.5 ms) cuts SMAA 1x's flicker by 40 % with only two
  frames of history (a little softness in motion); S2x costs MSAA 2×'s scene cost plus a
  second SMAA; 4x is the steadiest of the family, with the softness of a two-frame history.
- **Integrated GPUs:** render size decides: TAAU from 67 % (34 ms) costs 60 % of plain
  1080p (58 ms) with less flicker; post filters cost 1.4–3.3 ms; MSAA 4× costs a third of
  the frame. The engine's integrated default (TAAU from 67 %) stays.
- **Scaling for looks:** nearest and integer scaling are free (in the tone mapper) and
  keep pixels square; integer scaling is the pixel-art mode (identical to nearest at a
  whole factor, letterboxed otherwise). Bilinear is the softest and flickers least of
  the spatial modes.
- **FSR 1** (0.2 / 4.9 ms) sharpens: against a smooth supersampled reference it scores
  below bilinear in PSNR, slightly better in FLIP, and its sharpening amplifies aliasing
  (flicker 4.9 % at 67 %, 17 % with RCAS alone at 100 %). AMD asks for an antialiased
  input: give it MSAA (less flicker, better PSNR) or raise `sharpness` (in stops: 1 is
  half as strong); for upscaling, TAAU from the same size is both sharper and steadier
  here.

## Consequences

- A game offers any whitelist of modes and builds its menu with `check`; the dev guide's
  "Choosing antialiasing" has the table above for the choice.
- `FrameRenderer::prepare` takes `&mut self` (it makes pipelines on first use) and
  `PrepareContext` takes the scene's `Skinning` (MSAA's skinned motion pass);
  `FrameRenderer::destroy` frees SMAA's tables and the scene passes' atlases.
- MSAA views always draw the depth prepass. On dense geometry the prepass costs a second
  vertex pass (ADR 0042); with MSAA it also saves shading samples the depth test would
  have overwritten. Not measured apart.
- MSAA's shading interpolates at the pixel center (no centroid: the shading entry points
  are shared with single-sampled views, the register rule): a triangle that covers only
  part of a pixel may sample its textures slightly outside itself. Not visible in the
  test scenes.
- Ambient occlusion and contact shadows stay one value per pixel with MSAA (the nearest
  sample's); the halo test shows MSAA keeps its edge gain with every effect on (two
  pixels brought closer to the supersampled reference for each one taken away), and
  per-effect numbers are in `msaa_edge_gain_per_effect`. Per-sample AO would cost k×.
- SMAA T2x's resolve and SMAA S2x's second sample run after tone mapping, outside the
  `Upscaler` interface; SMAA T2x does not upscale.
- FSR 1 expects antialiased input (AMD's guide): on an aliased image its sharpening
  raises flicker; over MSAA it does not (table). FSR 1's 16-bit paths are not ported.
- Golden shots: FXAA, SMAA 1x, SMAA T2x, MSAA 4×, MSAA 4× + SMAA, nearest from 50 %,
  integer scaling, FSR 1 from 67 % on both GPUs; `tests/golden/tests/aa_modes.rs` runs
  every combination under the validation layer with every feature on, and the halo
  test; the perf gate measures MSAA 4×, FXAA, SMAA 1x, SMAA T2x and FSR 1.
- Follow-ups: dynamic resolution with MSAA (each size makes new multisampled
  transients), a bilateral AO read for MSAA if a game shows a halo, FSR 1 after tone
  mapping as an option, `aa_bench` in the perf history when modes change.
