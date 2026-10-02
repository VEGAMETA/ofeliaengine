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

{TABLES}

## Decision

{DECISION}

## Consequences

{CONSEQUENCES}
